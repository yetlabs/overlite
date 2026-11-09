//! A Query message may carry several statements separated by semicolons, and
//! Postgres runs them one after another, each with its own result, inside one
//! implicit transaction: the first failure stops the rest and undoes what came
//! before. Migration tools depend on that — Prisma applies a whole migration file
//! as one Query.

use crate::ddl::alter_actions::split_alter_actions;
use crate::ddl::database::is_database_ddl;
use crate::error::Result;
use crate::rewrite::blank::{is_blank_statement, statement_text};
use crate::session::Session;
use crate::session::utility::{SavepointKind, TxControl};
use overlite_core::SqlState;
use overlite_core::text::{text_from_bytes, text_to_bytes, trim_end_chars};
use pg_query::protobuf::Token;

impl Session {
    /// Runs a Query message, statement by statement.
    pub(crate) fn handle_query(&mut self, body: &[u8]) -> Result<()> {
        if body
            .split_last()
            .is_none_or(|(end, text)| *end != 0 || text.contains(&0))
        {
            return self
                .c
                .send_error(SqlState::PROTOCOL_VIOLATION, "invalid message format");
        }
        // Each with whether it goes on a statement split at its ALTER TABLE actions.
        let mut stmts: Vec<(String, bool)> = Vec::new();
        let text = text_from_bytes(body);
        for st in split_statements(trim_end_chars(&text, "\x00")) {
            let actions = split_alter_actions(&st).into_iter().enumerate();
            stmts.extend(actions.map(|(i, action)| (action, i > 0)));
        }
        if stmts.len() <= 1 {
            return self.handle_simple_query(body);
        }

        // Statements that manage transactions themselves (or, like CREATE
        // DATABASE, refuse to run inside one) opt the batch out of the implicit
        // transaction; each then runs on its own, as before.
        let mut implicit = !self.in_tx();
        for (st, _) in &stmts {
            // What normalize_literals refuses fails when the statement runs.
            let Ok(st) = statement_text(st) else { continue };
            if TxControl::of(&st).is_some() || SavepointKind::of(&st).is_some() || is_database_ddl(&st) {
                implicit = false;
            }
        }
        if implicit && let Err(err) = self.apply_tx_control(TxControl::Begin) {
            return self.c.send_error(err.sql_state(), err.message());
        }

        let mut failed = false;
        for (st, later_action) in &stmts {
            self.c.errored = false;
            self.later_action = *later_action;
            self.handle_simple_query(&text_to_bytes(st))?;
            if self.c.errored {
                failed = true;
                break;
            }
        }

        self.later_action = false;
        if implicit
            && self.in_tx()
            && let Err(err) = self.end_tx(!failed)
            && !failed
        {
            return self.send_exec_error(&err);
        }
        Ok(())
    }
}

/// Cuts sql at the semicolons that end statements, as Postgres' scanner reads it, so
/// that one inside a string literal of any kind, a quoted name, a dollar-quoted body
/// or a comment ends nothing; nor does one inside the BEGIN ... END body of a CREATE
/// TRIGGER (SQLite's form) or a function's BEGIN ATOMIC. Pieces with nothing
/// executable in them are dropped. A text the scanner refuses (a literal never
/// closed) is one statement, which its parse refuses.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let add = |out: &mut Vec<String>, st: &str| {
        if !is_blank_statement(st) {
            out.push(st.trim().to_string());
        }
    };
    let Ok(scanned) = pg_query::scan(sql) else {
        add(&mut out, sql);
        return out;
    };
    let toks: Vec<(Token, usize, usize)> = scanned
        .tokens
        .iter()
        .map(|t| {
            (
                Token::try_from(t.token).unwrap_or(Token::Nul),
                usize::try_from(t.start).unwrap_or(0),
                usize::try_from(t.end).unwrap_or(0),
            )
        })
        .collect();
    let mut start = 0;
    // Where the current statement's tokens begin, and how deep in a body it is.
    let mut first = 0;
    let mut depth = 0i32;
    for (k, &(t, at, end)) in toks.iter().enumerate() {
        match t {
            Token::BeginP | Token::Case if has_body(&toks[first..k]) => depth += 1,
            Token::EndP if depth > 0 => depth -= 1,
            Token::Ascii59 if depth == 0 => {
                add(&mut out, &sql[start..at]);
                start = end;
                first = k + 1;
            }
            _ => {}
        }
    }
    add(&mut out, &sql[start..]);
    out
}

/// Whether the tokens of a statement so far open CREATE [OR REPLACE] [TEMP] TRIGGER,
/// FUNCTION or PROCEDURE, whose BEGIN ... END body holds statements of its own (CASE
/// ... END is counted too, since the same END closes it).
fn has_body(toks: &[(Token, usize, usize)]) -> bool {
    let mut k = toks.iter().map(|t| t.0);
    if k.next() != Some(Token::Create) {
        return false;
    }
    let mut next = k.next();
    if next == Some(Token::Or) {
        k.next();
        next = k.next();
    }
    if matches!(next, Some(Token::Temp | Token::Temporary)) {
        next = k.next();
    }
    matches!(next, Some(Token::Trigger | Token::Function | Token::Procedure))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn split_statements_cases() {
        let cases: &[(&str, &[&str])] = &[
            ("SELECT 1", &["SELECT 1"]),
            ("SELECT 1; SELECT 2;", &["SELECT 1", "SELECT 2"]),
            (
                "-- a\nCREATE SCHEMA x;\n\n-- b\nCREATE TABLE t (a int);\n",
                &["-- a\nCREATE SCHEMA x", "-- b\nCREATE TABLE t (a int)"],
            ),
            ("SELECT 'a;b'; SELECT 2", &["SELECT 'a;b'", "SELECT 2"]),
            ("SELECT 'it''s;'; SELECT 2", &["SELECT 'it''s;'", "SELECT 2"]),
            (r"SELECT E'\';'; SELECT 2", &[r"SELECT E'\';'", "SELECT 2"]),
            (
                r#"SELECT "a;b" FROM t; SELECT 2"#,
                &[r#"SELECT "a;b" FROM t"#, "SELECT 2"],
            ),
            ("SELECT 1 -- x;y\n; SELECT 2", &["SELECT 1 -- x;y", "SELECT 2"]),
            ("SELECT /* ; */ 1; SELECT 2", &["SELECT /* ; */ 1", "SELECT 2"]),
            (
                "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql; SELECT f()",
                &[
                    "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql",
                    "SELECT f()",
                ],
            ),
            (
                "DO $body$ BEGIN; END $body$; SELECT 2",
                &["DO $body$ BEGIN; END $body$", "SELECT 2"],
            ),
            ("SELECT $1; SELECT $2", &["SELECT $1", "SELECT $2"]),
            ("; -- only comments ;", &[]),
            (
                "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN INSERT INTO a VALUES (CASE WHEN 1 THEN 'x' END); UPDATE a SET m = 'y'; END; SELECT 2",
                &[
                    "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN INSERT INTO a VALUES (CASE WHEN 1 THEN 'x' END); UPDATE a SET m = 'y'; END",
                    "SELECT 2",
                ],
            ),
            (
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END; SELECT f()",
                &[
                    "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; END",
                    "SELECT f()",
                ],
            ),
            ("BEGIN; SELECT 1; END", &["BEGIN", "SELECT 1", "END"]),
        ];
        for (input, want) in cases {
            assert_eq!(split_statements(input), *want, "input {input:?}");
        }
    }
}
