//! TRUNCATE, which SQLite does not have: a DELETE of every row of each table, in one
//! savepoint, with the sequences of their columns restarted under RESTART IDENTITY
//! and the tables whose foreign keys point at them emptied too under CASCADE.

use crate::auth::rls::text_or_empty;
use crate::error::{Result, errf};
use crate::rewrite::tokens::{CodeTokKind, code_tokens};
use crate::session::Session;
use crate::session::utility::{first_word_upper, log_query};
use overlite_core::text::{eq_fold, quote_ident, quote_literal, quoted};
use overlite_core::{Command, SqlState};

/// What a TRUNCATE says.
#[derive(Debug, Default, PartialEq)]
struct Truncate {
    /// The tables as written, qualified or not.
    tables: Vec<String>,
    restart: bool,
    cascade: bool,
}

pub(crate) fn is_truncate(sql: &str) -> bool {
    first_word_upper(sql).eq_ignore_ascii_case("TRUNCATE")
}

/// Reads `TRUNCATE [TABLE] [ONLY] t [*] [, ...] [RESTART | CONTINUE IDENTITY]
/// [CASCADE | RESTRICT]`; None when it is something else.
fn parse_truncate(sql: &str) -> Option<Truncate> {
    let toks = code_tokens(sql);
    let word = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
    let punct = |i: usize, p: &str| toks.get(i).is_some_and(|t| t.is_punct(sql, p));
    if !word(0, "truncate") {
        return None;
    }
    let mut i = 1;
    if word(i, "table") {
        i += 1;
    }
    let mut out = Truncate::default();
    loop {
        if word(i, "only") {
            i += 1;
        }
        let first = toks.get(i)?;
        if first.kind == CodeTokKind::Punct {
            return None;
        }
        let mut end = first.span.end;
        i += 1;
        if punct(i, ".") && toks.get(i + 1).is_some_and(|t| t.kind != CodeTokKind::Punct) {
            end = toks[i + 1].span.end;
            i += 2;
        }
        out.tables.push(sql[first.span.start..end].to_string());
        if punct(i, "*") {
            i += 1;
        }
        if !punct(i, ",") {
            break;
        }
        i += 1;
    }
    while i < toks.len() {
        if (word(i, "restart") || word(i, "continue")) && word(i + 1, "identity") {
            out.restart = word(i, "restart");
            i += 2;
        } else if word(i, "cascade") || word(i, "restrict") {
            out.cascade = word(i, "cascade");
            i += 1;
        } else if punct(i, ";") {
            i += 1;
        } else {
            return None;
        }
    }
    Some(out)
}

impl Session {
    /// Runs a TRUNCATE; None when sql is not one.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_truncate(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_truncate(sql) {
            return None;
        }
        let Some(t) = parse_truncate(sql) else {
            return Some(Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in TRUNCATE")));
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
        for reference in &t.tables {
            let stored = self.stored_name(reference);
            let q = "SELECT name FROM main.sqlite_master WHERE type = 'table' AND lower(name) = lower(".to_string()
                + &quote_literal(&stored)
                + ")";
            let Some(row) = self.exec(&q, &[])?.rows.into_iter().next() else {
                return Err(errf!(
                    SqlState::UNDEFINED_TABLE,
                    "relation {:?} does not exist",
                    quoted(&crate::session::utility::unquote_ident(reference))
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
    fn parse_truncate_forms() {
        let p = |s: &str| parse_truncate(s).unwrap();
        assert_eq!(p("TRUNCATE t").tables, ["t"]);
        let t = p(r#"TRUNCATE TABLE ONLY "a", s."b" *, c RESTART IDENTITY CASCADE;"#);
        assert_eq!(t.tables, [r#""a""#, r#"s."b""#, "c"]);
        assert!(t.restart && t.cascade);
        let t = p("truncate a continue identity restrict");
        assert!(!t.restart && !t.cascade);
        assert_eq!(parse_truncate("TRUNCATE a b"), None);
        assert_eq!(parse_truncate("TRUNCATE"), None);
    }
}
