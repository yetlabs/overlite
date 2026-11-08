//! A Query message may carry several statements separated by semicolons, and
//! Postgres runs them one after another, each with its own result, inside one
//! implicit transaction: the first failure stops the rest and undoes what came
//! before. Migration tools depend on that — Prisma applies a whole migration file
//! as one Query.

use crate::ddl::alter_actions::split_alter_actions;
use crate::ddl::database::is_database_ddl;
use crate::error::Result;
use crate::rewrite::blank::{
    is_blank_statement, skip_block_comment, skip_line_comment, statement_text, strip_leading_comments,
};
use crate::rewrite::catalog::is_word_byte;
use crate::rewrite::default_expr::skip_quoted;
use crate::rewrite::index_from;
use crate::session::Session;
use crate::session::utility::{SavepointKind, TxControl};
use overlite_core::SqlState;
use overlite_core::text::{split_words, text_from_bytes, text_to_bytes, trim_end_chars};
use overlite_engine::code_only;

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

/// Cuts sql at the semicolons that end statements, leaving alone those inside
/// string literals, quoted identifiers, dollar-quoted bodies and comments. Pieces
/// with nothing executable in them are dropped.
fn split_statements(sql: &str) -> Vec<String> {
    let s = sql.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let add = |out: &mut Vec<String>, st: &str| {
        if !is_blank_statement(st) {
            out.push(st.trim().to_string());
        }
    };
    let mut start = 0;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' {
            if i > 0 && (s[i - 1] == b'e' || s[i - 1] == b'E') && (i < 2 || !is_word_byte(s[i - 2])) {
                i = skip_escape_string(sql, i) - 1;
            } else {
                i = skip_quoted(sql, i, b'\'') - 1;
            }
        } else if c == b'"' {
            i = skip_quoted(sql, i, b'"') - 1;
        } else if c == b'-' && i + 1 < s.len() && s[i + 1] == b'-' {
            i = skip_line_comment(sql, i) - 1;
        } else if c == b'/' && i + 1 < s.len() && s[i + 1] == b'*' {
            let (j, _) = skip_block_comment(sql, i);
            i = j - 1;
        } else if c == b'$' {
            let tag = dollar_tag(sql, i);
            if !tag.is_empty() {
                match index_from(sql, tag, i + tag.len()) {
                    Some(j) => i = j + tag.len() - 1,
                    None => i = s.len() - 1,
                }
            }
        } else if c == b';' && !open_body(&sql[start..i]) {
            // (a semicolon with the body open is inside a trigger's BEGIN … END)
            add(&mut out, &sql[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    add(&mut out, &sql[start..]);
    out
}

/// Reports whether stmt is a CREATE TRIGGER, FUNCTION or PROCEDURE whose BEGIN …
/// END body (SQLite's trigger form, or BEGIN ATOMIC) is still open, so a semicolon
/// at its end belongs to the body. CASE … END is counted too, since the same END
/// closes it.
fn open_body(stmt: &str) -> bool {
    // A BEGIN or an END in a comment or a literal opens and closes nothing.
    let up = code_only(strip_leading_comments(stmt)).to_uppercase();
    let all = split_words(&up);
    let mut f: &[&str] = &all;
    if f.len() < 2 || f[0] != "CREATE" {
        return false;
    }
    f = &f[1..];
    if f.len() > 2 && f[0] == "OR" && f[1] == "REPLACE" {
        f = &f[2..];
    }
    if f.len() > 1 && (f[0] == "TEMP" || f[0] == "TEMPORARY") {
        f = &f[1..];
    }
    if f.is_empty() || (f[0] != "TRIGGER" && f[0] != "FUNCTION" && f[0] != "PROCEDURE") {
        return false;
    }
    let low = stmt.to_ascii_lowercase();
    let s = stmt.as_bytes();
    let mut depth = 0;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            i = skip_quoted(stmt, i, c) - 1;
        } else if c == b'-' && i + 1 < s.len() && s[i + 1] == b'-' {
            i = skip_line_comment(stmt, i) - 1;
        } else if c == b'/' && i + 1 < s.len() && s[i + 1] == b'*' {
            let (j, _) = skip_block_comment(stmt, i);
            i = j - 1;
        } else if c == b'$' {
            let tag = dollar_tag(stmt, i);
            if !tag.is_empty() {
                match index_from(stmt, tag, i + tag.len()) {
                    Some(j) => i = j + tag.len() - 1,
                    None => return false,
                }
            }
        } else if is_word_byte(c) && (i == 0 || !is_word_byte(s[i - 1])) {
            let mut j = i;
            while j < s.len() && is_word_byte(s[j]) {
                j += 1;
            }
            match &low[i..j] {
                "begin" | "case" => depth += 1,
                "end" => depth -= 1,
                _ => {}
            }
            i = j - 1;
        }
        i += 1;
    }
    depth > 0
}

/// Returns the $tag$ (or $$) opening a dollar-quoted string at i, or "" when the
/// $ starts something else, such as the parameter $1.
fn dollar_tag(sql: &str, i: usize) -> &str {
    let s = sql.as_bytes();
    if i > 0 && is_word_byte(s[i - 1]) {
        return ""; // part of an identifier, e.g. a$b
    }
    let mut j = i + 1;
    while j < s.len() && s[j] != b'$' {
        if !is_word_byte(s[j]) || (j == i + 1 && s[j].is_ascii_digit()) {
            return "";
        }
        j += 1;
    }
    if j >= s.len() {
        return "";
    }
    &sql[i..=j]
}

/// Returns the index just past the E'…' string whose quote is at i, where a
/// backslash escapes the character after it.
fn skip_escape_string(sql: &str, i: usize) -> usize {
    let s = sql.as_bytes();
    let mut j = i + 1;
    while j < s.len() {
        match s[j] {
            b'\\' => j += 1,
            b'\'' => {
                if j + 1 < s.len() && s[j + 1] == b'\'' {
                    j += 2;
                    continue;
                }
                return j + 1;
            }
            _ => {}
        }
        j += 1;
    }
    s.len()
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
