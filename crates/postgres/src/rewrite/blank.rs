//! A statement that carries no executable SQL — a lone comment, say — is not the
//! same as one the engine can run. Postgres answers it with EmptyQueryResponse,
//! and clients rely on that: pgx's Ping sends the bare comment `-- ping`. The engine must never see these.

use crate::error::Result;
use crate::rewrite::strings::normalize_literals;
use overlite_engine::blank_comments;
use std::borrow::Cow;

/// Reports whether sql contains nothing executable: only whitespace, semicolons
/// and comments.
pub(crate) fn is_blank_statement(sql: &str) -> bool {
    executable_start(sql) == sql.len()
}

/// A statement as everything after the door reads it: the comments in front gone,
/// every other comment turned to spaces, and E'', U&'' and continued strings made
/// plain (normalize_literals). A comment is whitespace to Postgres, so a quote, a
/// `$n` or a keyword in one must not count to the parameter count, the routing
/// classifiers, the fast path or a rewrite pass. Doing it once here keeps each of
/// those from having to know comments.
///
/// # Errors
///
/// What normalize_literals refuses.
pub(crate) fn statement_text(sql: &str) -> Result<Cow<'_, str>> {
    let sql = strip_leading_comments(sql);
    match blank_comments(sql) {
        Cow::Borrowed(sql) => normalize_literals(sql),
        Cow::Owned(sql) => Ok(Cow::Owned(normalize_literals(&sql)?.into_owned())),
    }
}

/// Drops the whitespace, semicolons and comments in front of a statement.
/// Everything that routes a statement looks at its first words, and migration
/// tools put a comment above each one.
pub(crate) fn strip_leading_comments(sql: &str) -> &str {
    &sql[executable_start(sql)..]
}

/// Returns the index of the first character that is not whitespace, a semicolon
/// or part of a comment, or sql.len() when there is none.
fn executable_start(sql: &str) -> usize {
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' || c == 0x0b || c == 0x0c || c == b';' {
            i += 1;
        } else if c == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            i = skip_line_comment(sql, i);
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let (j, closed) = skip_block_comment(sql, i);
            if !closed {
                // Unterminated: let the parser reject it, as Postgres does,
                // rather than passing it off as an empty statement.
                return i;
            }
            i = j;
        } else {
            return i;
        }
    }
    i
}

/// Returns the index of the newline ending the `--` comment at i, or the end of
/// the string.
pub(crate) fn skip_line_comment(sql: &str, i: usize) -> usize {
    match sql[i..].find('\n') {
        Some(j) => i + j + 1,
        None => sql.len(),
    }
}

/// Returns the index just past the `/* */` comment at i, and whether it was
/// closed. Postgres nests these, so an inner /* must be matched before the
/// outer */.
pub(crate) fn skip_block_comment(sql: &str, i: usize) -> (usize, bool) {
    let b = sql.as_bytes();
    let mut i = i;
    let mut depth = 0;
    while i < b.len() {
        if b[i..].starts_with(b"/*") {
            depth += 1;
            i += 2;
        } else if b[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return (i, true);
            }
        } else {
            i += 1;
        }
    }
    (b.len(), false)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn blank_statements() {
        for sql in [
            "",
            "   ",
            "\t\n",
            ";",
            " ; ; ",
            "-- ping",
            "--",
            "-- ping;",
            "--ping\n--again",
            "/* c */",
            "/* multi\nline */",
            "/* outer /* inner */ */",
            " -- one\n /* two */ ; ",
        ] {
            assert!(is_blank_statement(sql), "{sql:?}");
        }
        for sql in [
            "SELECT 1",
            "-- ping\nSELECT 1",
            "/* c */ SELECT 1",
            "SELECT 1 -- trailing",
            "SELECT '-- not a comment'",
            "SELECT '/* not a comment */'",
            r#"SELECT "--""#,
            "/* unterminated",
        ] {
            assert!(!is_blank_statement(sql), "{sql:?}");
        }
        for (input, want) in [
            ("SELECT 1", "SELECT 1"),
            ("-- CreateSchema\nCREATE SCHEMA x", "CREATE SCHEMA x"),
            ("/* a */ /* b */ BEGIN", "BEGIN"),
            (" -- one\n /* two */ ; SELECT 1", "SELECT 1"),
            ("SELECT 1 -- trailing", "SELECT 1 -- trailing"),
            ("/* unterminated", "/* unterminated"),
            ("-- only", ""),
        ] {
            assert_eq!(strip_leading_comments(input), want);
        }
    }
}
