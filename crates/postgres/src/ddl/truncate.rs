//! TRUNCATE, which SQLite does not have: a DELETE of every row of each table, in one
//! savepoint, with the sequences of their columns restarted under RESTART IDENTITY
//! and the tables whose foreign keys point at them emptied too under CASCADE.

use crate::auth::rls::text_or_empty;
use crate::ddl::tree;
use crate::error::{Result, errf};
use crate::session::Session;
use crate::session::utility::{first_word_upper, log_query};
use overlite_core::text::{eq_fold, quote_ident, quote_literal, quoted};
use overlite_core::{Command, SqlState};
use pg_query::NodeEnum;

/// What a TRUNCATE says.
#[derive(Debug, Default, PartialEq)]
struct Truncate {
    /// The tables as references (`split_qualified_ref`'s), and as a message names
    /// each: `s.t` or `t`.
    tables: Vec<(String, String)>,
    restart: bool,
    cascade: bool,
}

pub(crate) fn is_truncate(sql: &str) -> bool {
    first_word_upper(sql).eq_ignore_ascii_case("TRUNCATE")
}

/// A TRUNCATE's node, read; None for anything else.
fn truncate_plan(n: &NodeEnum) -> Option<Truncate> {
    let NodeEnum::TruncateStmt(t) = n else {
        return None;
    };
    let tables = t
        .relations
        .iter()
        .filter_map(|r| match &r.node {
            Some(NodeEnum::RangeVar(r)) => {
                let shown = if r.schemaname.is_empty() {
                    r.relname.clone()
                } else {
                    [r.schemaname.as_str(), ".", &r.relname].concat()
                };
                Some((tree::reference(r), shown))
            }
            _ => None,
        })
        .collect();
    Some(Truncate {
        tables,
        restart: t.restart_seqs,
        cascade: t.behavior == pg_query::protobuf::DropBehavior::DropCascade as i32,
    })
}

impl Session {
    /// Runs a TRUNCATE; None when sql is not one.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_truncate(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_truncate(sql) {
            return None;
        }
        let t = match tree::statement(sql, |n| truncate_plan(n).ok_or_else(|| tree::parse_failure(sql))) {
            Ok(t) => t,
            Err(e) => return Some(Err(e)),
        };
        Some(
            self.check_privileges(sql)
                .and_then(|()| self.truncate(&t))
                .map(|()| Command::from("TRUNCATE TABLE")),
        )
    }

    fn truncate(&mut self, t: &Truncate) -> Result<()> {
        // (as written, stored name) of each table.
        let mut tables: Vec<(String, String)> = Vec::new();
        for (reference, shown) in &t.tables {
            let stored = self.stored_name(reference);
            let q = "SELECT name FROM main.sqlite_master WHERE type = 'table' AND lower(name) = lower(".to_string()
                + &quote_literal(&stored)
                + ")";
            let Some(row) = self.exec(&q, &[])?.rows.into_iter().next() else {
                return Err(errf!(
                    SqlState::UNDEFINED_TABLE,
                    "relation {:?} does not exist",
                    quoted(shown)
                ));
            };
            let name = text_or_empty(&row[0]);
            if !tables.iter().any(|(_, s)| eq_fold(s, &name)) {
                tables.push((reference.clone(), name));
            }
        }
        // The tables whose foreign keys point at one being emptied: an error, or
        // emptied too under CASCADE, and so on down.
        let refs = self.exec(
            "SELECT m.name, fk.\"table\" FROM main.sqlite_master m JOIN pragma_foreign_key_list(m.name) fk \
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' AND m.name NOT GLOB '_overlite_*'",
            &[],
        )?;
        loop {
            let mut added = false;
            for row in &refs.rows {
                let (child, parent) = (text_or_empty(&row[0]), text_or_empty(&row[1]));
                let in_set = |n: &str| tables.iter().any(|(_, s)| eq_fold(s, n));
                if !in_set(&parent) || in_set(&child) {
                    continue;
                }
                if !t.cascade {
                    return Err(errf!(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "cannot truncate a table referenced in a foreign key constraint"
                    ));
                }
                self.c.send_notice(&format!("truncate cascades to table \"{child}\""))?;
                tables.push((quote_ident(&child), child));
                added = true;
            }
            if !added {
                break;
            }
        }

        let mut steps = Vec::new();
        for (_, stored) in &tables {
            steps.push("DELETE FROM ".to_string() + &quote_ident(stored));
        }
        if t.restart {
            for (_, stored) in &tables {
                if self.has_sqlite_sequence()? {
                    steps.push(
                        "DELETE FROM main.sqlite_sequence WHERE lower(name) = lower(".to_string()
                            + &quote_literal(stored)
                            + ")",
                    );
                }
                steps.push(
                    "UPDATE _overlite_sequences SET last_value = start_value, is_called = 0 WHERE seqname IN \
                     (SELECT seqname FROM _overlite_defaults WHERE lower(tablename) = lower("
                        .to_string()
                        + &quote_literal(stored)
                        + "))",
                );
            }
        }
        self.exec("SAVEPOINT _truncate", &[])?;
        // The tables go one by one, and a key between two of them is checked at
        // the end, when both are empty.
        let _ = self.exec("PRAGMA defer_foreign_keys = ON", &[]);
        for st in &steps {
            log_query("truncate", st);
            if let Err(e) = self.exec(st, &[]) {
                let _ = self.exec("ROLLBACK TO _truncate", &[]);
                let _ = self.exec("RELEASE _truncate", &[]);
                let _ = self.exec("PRAGMA defer_foreign_keys = OFF", &[]);
                return Err(e);
            }
        }
        let released = self.exec("RELEASE _truncate", &[]);
        let _ = self.exec("PRAGMA defer_foreign_keys = OFF", &[]);
        released.map(|_| ())
    }

    fn has_sqlite_sequence(&mut self) -> Result<bool> {
        let q = "SELECT 1 FROM main.sqlite_master WHERE name = 'sqlite_sequence'";
        Ok(!self.exec(q, &[])?.rows.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_forms() {
        let p = |s: &str| tree::read(s, truncate_plan).unwrap();
        let t = p(r#"TRUNCATE TABLE ONLY "a", s."B" *, c RESTART IDENTITY CASCADE;"#);
        let names: Vec<&str> = t.tables.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(names, [r#""a""#, r#""s"."B""#, r#""c""#]);
        assert_eq!(t.tables[1].1, "s.B");
        assert!(t.restart && t.cascade);
        let t = p("truncate a continue identity restrict");
        assert!(!t.restart && !t.cascade);
    }
}
