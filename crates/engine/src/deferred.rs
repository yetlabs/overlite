//! `SET CONSTRAINTS`. SQLite checks a DEFERRABLE INITIALLY DEFERRED foreign key at
//! COMMIT and has no way to move that check. Postgres also checks it when SET
//! CONSTRAINTS makes it IMMEDIATE, and after every statement from then on, which
//! Django's check_constraints() relies on: it sets them IMMEDIATE to learn whether the
//! rows it loaded hold. SQLite counts the violations still pending
//! (SQLITE_DBSTATUS_DEFERRED_FKS), so asking costs nothing; finding which row it is
//! takes a PRAGMA foreign_key_check, made only when the count says there is one.

use crate::catalog::constraints::fk_constraint_name;
use crate::error::{Error, Result};
use crate::sqlite::DbSession;
use overlite_core::text::quote_literal;
use overlite_core::{ResultSet, SqlState};
use rusqlite::{Connection, ffi};

/// What a SET CONSTRAINTS says: the constraints, None for ALL, and whether they
/// become IMMEDIATE.
pub(crate) fn parse_set_constraints(sql: &str) -> Option<(Option<Vec<String>>, bool)> {
    let t = sql.trim().trim_end_matches(';').trim();
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.len() < 4 || !words[0].eq_ignore_ascii_case("set") || !words[1].eq_ignore_ascii_case("constraints") {
        return None;
    }
    let last = words[words.len() - 1];
    let immediate = if last.eq_ignore_ascii_case("immediate") {
        true
    } else if last.eq_ignore_ascii_case("deferred") {
        false
    } else {
        return None;
    };
    let body = t[t.to_ascii_lowercase().find("constraints")? + "constraints".len()..t.len() - last.len()].trim();
    if body.eq_ignore_ascii_case("all") {
        return Some((None, immediate));
    }
    let names = body
        .split(',')
        .map(|n| {
            let n = n.trim();
            let n = n.rsplit('.').next().unwrap_or(n);
            match n.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
                Some(q) => q.replace("\"\"", "\""),
                None => n.to_ascii_lowercase(),
            }
        })
        .collect();
    Some((Some(names), immediate))
}

/// The foreign key violations SQLite still holds for COMMIT.
fn pending(q: &Connection) -> i32 {
    let (mut cur, mut hi) = (0, 0);
    // SAFETY: the handle is this connection's, open for as long as q is borrowed, and
    // sqlite3_db_status only reads a counter of it into the two integers given.
    unsafe {
        ffi::sqlite3_db_status(
            q.handle(),
            ffi::SQLITE_DBSTATUS_DEFERRED_FKS,
            &raw mut cur,
            &raw mut hi,
            0,
        );
    }
    cur
}

/// The first violation of the constraints named (all for None), as Postgres words it.
fn violation(q: &Connection, names: Option<&[String]>) -> Result<Option<Error>> {
    let mut check = q
        .prepare("PRAGMA foreign_key_check")
        .map_err(crate::sqlite::sqlite_error)?;
    let rows: Vec<(String, Option<i64>, String, i64)> = check
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(crate::sqlite::sqlite_error)?
        .filter_map(std::result::Result::ok)
        .collect();
    for (table, _rowid, parent, fkid) in rows {
        let cols: Vec<String> = {
            let mut st = q
                .prepare(&format!(
                    "SELECT \"from\" FROM pragma_foreign_key_list({}) WHERE id = ?1 ORDER BY seq",
                    quote_literal(&table)
                ))
                .map_err(crate::sqlite::sqlite_error)?;
            st.query_map([fkid], |r| r.get(0))
                .map_err(crate::sqlite::sqlite_error)?
                .filter_map(std::result::Result::ok)
                .collect()
        };
        let ddl: String = q
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [&table],
                |r| r.get(0),
            )
            .unwrap_or_default();
        let relation = table.rsplit('.').next().unwrap_or(&table).to_string();
        let parent_relation = parent.rsplit('.').next().unwrap_or(&parent).to_string();
        let name = fk_constraint_name(&ddl, &relation, &cols, &parent_relation, fkid);
        if names.is_some_and(|n| !n.iter().any(|x| x.eq_ignore_ascii_case(&name))) {
            continue;
        }
        return Ok(Some(Error::with_state(
            SqlState::FOREIGN_KEY_VIOLATION,
            format!("insert or update on table \"{relation}\" violates foreign key constraint \"{name}\""),
        )));
    }
    Ok(None)
}

impl DbSession {
    /// Runs a SET CONSTRAINTS: making constraints IMMEDIATE checks what is pending
    /// against them, and leaves the session checking after each statement until the
    /// transaction ends or they are DEFERRED again. None for any other statement.
    pub(crate) fn set_constraints(&self, sql: &str) -> Option<Result<ResultSet>> {
        let (names, immediate) = parse_set_constraints(sql)?;
        let outcome = (|| {
            if immediate
                && pending(&self.conn) > 0
                && let Some(e) = violation(&self.conn, names.as_deref())?
            {
                return Err(e);
            }
            // A transaction that has not written yet has no SQLite transaction open.
            if self.in_transaction() {
                self.immediate_fks.set(immediate && names.is_none());
            }
            Ok(ResultSet {
                command: "SET CONSTRAINTS".into(),
                ..Default::default()
            })
        })();
        Some(outcome)
    }

    /// The violation of a deferred foreign key a COMMIT would meet, if any.
    pub(crate) fn pending_violation(&self) -> Result<Option<Error>> {
        if self.conn.is_autocommit() || pending(&self.conn) == 0 {
            return Ok(None);
        }
        violation(&self.conn, None)
    }

    /// After a statement in a transaction whose constraints were made IMMEDIATE: the
    /// violation it left, if any.
    pub(crate) fn check_immediate(&self) -> Result<()> {
        if !self.immediate_fks.get() {
            return Ok(());
        }
        if !self.in_transaction() {
            self.immediate_fks.set(false);
            return Ok(());
        }
        // Nothing is pending before the transaction's first write opens SQLite's.
        if !self.conn.is_autocommit()
            && pending(&self.conn) > 0
            && let Some(e) = violation(&self.conn, None)?
        {
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_constraints_forms() {
        assert_eq!(
            parse_set_constraints("SET CONSTRAINTS ALL IMMEDIATE"),
            Some((None, true))
        );
        assert_eq!(
            parse_set_constraints("set constraints all deferred;"),
            Some((None, false))
        );
        assert_eq!(
            parse_set_constraints(r#"SET CONSTRAINTS "A_fk", s.b IMMEDIATE"#),
            Some((Some(vec!["A_fk".to_string(), "b".to_string()]), true))
        );
        assert_eq!(parse_set_constraints("SET search_path TO x"), None);
    }
}
