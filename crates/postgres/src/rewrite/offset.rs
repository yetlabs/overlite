//! Postgres takes OFFSET on its own, and before or after LIMIT. SQLite only has
//! OFFSET as a part of LIMIT (`LIMIT n OFFSET m`, in that order), so it rejects a
//! bare `OFFSET m` with a syntax error. ORMs lean on the bare form: Prisma ends
//! nearly every read it sends with `OFFSET $n`, subqueries included.

use crate::rewrite::blank::{skip_block_comment, skip_line_comment};
use crate::rewrite::catalog::is_word_byte;
use crate::rewrite::default_expr::skip_quoted;
use crate::rewrite::splice;
use overlite_core::text::trim_end_chars;

#[derive(Debug)]
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Gives each OFFSET that has no LIMIT in front of it the LIMIT SQLite requires:
/// the one written after it, moved in front (`OFFSET 5 LIMIT 10` becomes `LIMIT
/// 10 OFFSET 5`), or else `LIMIT -1`, which SQLite reads as no limit at all. Each
/// parenthesized level is a query of its own.
pub(crate) fn rewrite_offset(sql: &str) -> String {
    let low = sql.to_ascii_lowercase();
    if !low.contains("offset") {
        return sql.to_string();
    }
    let s = sql.as_bytes();
    let mut edits: Vec<Edit> = Vec::new();
    let mut limited: Vec<bool> = vec![false]; // per paren depth: the query there has its LIMIT
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            i = skip_quoted(sql, i, c) - 1;
        } else if c == b'-' && i + 1 < s.len() && s[i + 1] == b'-' {
            i = skip_line_comment(sql, i) - 1;
        } else if c == b'/' && i + 1 < s.len() && s[i + 1] == b'*' {
            let (j, _) = skip_block_comment(sql, i);
            i = j - 1;
        } else if c == b'(' {
            limited.push(false);
        } else if c == b')' {
            if limited.len() > 1 {
                limited.pop();
            }
        } else if is_clause_word(&low, i, "limit") {
            if let Some(level) = limited.last_mut() {
                *level = true;
            }
        } else if is_clause_word(&low, i, "offset") {
            let d = limited.len() - 1;
            if !limited[d] {
                limited[d] = true;
                let off_end = clause_end(sql, &low, i + "offset".len());
                if is_clause_word(&low, off_end, "limit") {
                    let lim_end = clause_end(sql, &low, off_end + "limit".len());
                    let span = &sql[i..lim_end];
                    let trail = &span[trim_end_chars(span, " \t\r\n").len()..];
                    let text = [sql[off_end..lim_end].trim(), " ", sql[i..off_end].trim(), trail].concat();
                    edits.push(Edit {
                        start: i,
                        end: lim_end,
                        text,
                    });
                    i = lim_end - 1;
                } else {
                    edits.push(Edit {
                        start: i,
                        end: i,
                        text: "LIMIT -1 ".to_string(),
                    });
                }
            }
        }
        i += 1;
    }
    let mut sql = sql.to_string();
    for e in edits.iter().rev() {
        sql = splice(&sql, e.start, e.end, &e.text);
    }
    sql
}

/// Reports whether the keyword word starts at i as a clause, not as part of a
/// longer name, a column reached through a qualifier (t.offset) or an output
/// label (AS offset).
fn is_clause_word(low: &str, i: usize, word: &str) -> bool {
    let l = low.as_bytes();
    if !l[i..].starts_with(word.as_bytes()) {
        return false;
    }
    let n = i + word.len();
    if n < l.len() && (is_word_byte(l[n]) || l[n] == b'"') {
        return false;
    }
    if i > 0 && (is_word_byte(l[i - 1]) || l[i - 1] == b'.' || l[i - 1] == b'$' || l[i - 1] == b'"') {
        return false;
    }
    let before = trim_end_chars(&low[..i], " \t\r\n");
    !before.ends_with(" as") && before != "as"
}

/// Returns where the clause whose expression starts at from ends: at the next
/// clause keyword of the same query, the paren that closes it, a semicolon, or
/// the end of the statement.
fn clause_end(sql: &str, low: &str, from: usize) -> usize {
    let s = sql.as_bytes();
    let mut depth = 0;
    let mut i = from;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            i = skip_quoted(sql, i, c) - 1;
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            if depth == 0 {
                return i;
            }
            depth -= 1;
        } else if c == b';' && depth == 0 {
            return i;
        } else if depth == 0 {
            for w in [
                "limit",
                "offset",
                "fetch",
                "for",
                "union",
                "intersect",
                "except",
                "returning",
            ] {
                if is_clause_word(low, i, w) {
                    return i;
                }
            }
        }
        i += 1;
    }
    s.len()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn rewrite_offset_cases() {
        assert_rewrites(
            rewrite_offset,
            &[
                (
                    r#"SELECT "id" FROM "User" WHERE "role" IN ($1) OFFSET $2"#,
                    r#"SELECT "id" FROM "User" WHERE "role" IN ($1) LIMIT -1 OFFSET $2"#,
                ),
                (
                    r#"SELECT COUNT(*) FROM (SELECT "id" FROM "User" WHERE 1=1 OFFSET $1) AS "sub""#,
                    r#"SELECT COUNT(*) FROM (SELECT "id" FROM "User" WHERE 1=1 LIMIT -1 OFFSET $1) AS "sub""#,
                ),
                ("SELECT a FROM t OFFSET 5 LIMIT 10", "SELECT a FROM t LIMIT 10 OFFSET 5"),
                (
                    "SELECT a FROM (SELECT a FROM t OFFSET 1 LIMIT 2) s",
                    "SELECT a FROM (SELECT a FROM t LIMIT 2 OFFSET 1) s",
                ),
                (
                    "SELECT a FROM (SELECT a FROM t LIMIT 3) s OFFSET 1",
                    "SELECT a FROM (SELECT a FROM t LIMIT 3) s LIMIT -1 OFFSET 1",
                ),
                (
                    "SELECT a FROM (SELECT a FROM t OFFSET 1) s LIMIT 3",
                    "SELECT a FROM (SELECT a FROM t LIMIT -1 OFFSET 1) s LIMIT 3",
                ),
                ("SELECT a FROM t LIMIT 10 OFFSET 5", "SELECT a FROM t LIMIT 10 OFFSET 5"),
                ("SELECT t.offset FROM t", "SELECT t.offset FROM t"),
                ("SELECT 1 AS offset", "SELECT 1 AS offset"),
                (r#"SELECT "offset" FROM t"#, r#"SELECT "offset" FROM t"#),
                ("SELECT 'x OFFSET 1' FROM t", "SELECT 'x OFFSET 1' FROM t"),
                ("SELECT offset_ms FROM t", "SELECT offset_ms FROM t"),
                (
                    "SELECT a FROM t ORDER BY a OFFSET 2;",
                    "SELECT a FROM t ORDER BY a LIMIT -1 OFFSET 2;",
                ),
            ],
        );
    }
}
