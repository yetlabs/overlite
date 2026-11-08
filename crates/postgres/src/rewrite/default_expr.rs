//! SQLite only accepts a bare literal or a keyword like CURRENT_TIMESTAMP as a
//! column default; anything computed has to be parenthesised. Postgres needs no
//! parentheses, so `DEFAULT now()` and `DEFAULT gen_random_uuid()` reached SQLite
//! as a syntax error on the opening bracket.

use crate::rewrite::catalog::is_word_byte;
use crate::rewrite::utf8_string;

/// Wraps a function-call default in the parentheses SQLite requires. A default
/// that is already parenthesised, a literal, or a bare keyword is left alone.
///
/// The scan walks the whole statement rather than only its non-string parts: the
/// call's own arguments are usually string literals (`datetime('now')`), so
/// splitting on them would hide the call.
pub(crate) fn rewrite_default_expr(sql: &str) -> String {
    if !sql.to_uppercase().contains("DEFAULT") {
        return sql.to_string();
    }
    let s = sql.as_bytes();
    let mut b: Vec<u8> = Vec::with_capacity(s.len() + 16);
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            let j = skip_quoted(sql, i, c);
            b.extend_from_slice(&s[i..j]);
            i = j;
            continue;
        }
        if (c == b'd' || c == b'D')
            && is_default_keyword(sql, i)
            && let Some((wrapped, end)) = wrap_default_call(sql, i)
        {
            b.extend_from_slice(wrapped.as_bytes());
            i = end;
            continue;
        }
        b.push(c);
        i += 1;
    }
    utf8_string(b)
}

/// Reports whether the word DEFAULT starts at i.
fn is_default_keyword(sql: &str, i: usize) -> bool {
    const KW: &[u8] = b"DEFAULT";
    let s = sql.as_bytes();
    if i + KW.len() > s.len() || !s[i..i + KW.len()].eq_ignore_ascii_case(KW) {
        return false;
    }
    if i > 0 && is_word_byte(s[i - 1]) {
        return false;
    }
    i + KW.len() == s.len() || !is_word_byte(s[i + KW.len()])
}

/// Renders `DEFAULT f(...)` as `DEFAULT (f(...))`, returning the index just past
/// the call. None when the default is not a call.
fn wrap_default_call(sql: &str, i: usize) -> Option<(String, usize)> {
    let s = sql.as_bytes();
    let mut j = i + "DEFAULT".len();
    while j < s.len() && (s[j] == b' ' || s[j] == b'\t' || s[j] == b'\n' || s[j] == b'\r') {
        j += 1;
    }
    let name = j;
    while j < s.len() && is_word_byte(s[j]) {
        j += 1;
    }
    if j == name {
        return None; // not an identifier: a literal, or already "("
    }
    let mut k = j;
    while k < s.len() && s[k] == b' ' {
        k += 1;
    }
    if k >= s.len() || s[k] != b'(' {
        return None; // a bare keyword such as CURRENT_TIMESTAMP
    }
    let close = match_paren(sql, k)?;
    Some(([&sql[i..name], "(", &sql[name..close], ")"].concat(), close))
}

/// Returns the index just past the parenthesis that closes the one at i.
fn match_paren(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = i;
    let mut depth = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i = skip_quoted(s, i, b'\'');
                continue;
            }
            b'"' => {
                i = skip_quoted(s, i, b'"');
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Returns the index just past the quoted run starting at i.
pub(crate) fn skip_quoted(s: &str, i: usize, q: u8) -> usize {
    let b = s.as_bytes();
    let mut i = i + 1;
    while i < b.len() {
        if b[i] == q {
            if i + 1 < b.len() && b[i + 1] == q {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn rewrite_default_expr_cases() {
        assert_rewrites(
            rewrite_default_expr,
            &[
                (
                    "CREATE TABLE t (at timestamptz DEFAULT datetime('now'))",
                    "CREATE TABLE t (at timestamptz DEFAULT (datetime('now')))",
                ),
                (
                    "CREATE TABLE t (id uuid DEFAULT gen_random_uuid())",
                    "CREATE TABLE t (id uuid DEFAULT (gen_random_uuid()))",
                ),
                (
                    "ALTER TABLE t ADD COLUMN at text DEFAULT datetime('now')",
                    "ALTER TABLE t ADD COLUMN at text DEFAULT (datetime('now'))",
                ),
                (
                    "CREATE TABLE t (a text DEFAULT lower('X'), b text DEFAULT upper('y'))",
                    "CREATE TABLE t (a text DEFAULT (lower('X')), b text DEFAULT (upper('y')))",
                ),
                (
                    "CREATE TABLE t (at text DEFAULT (datetime('now')))",
                    "CREATE TABLE t (at text DEFAULT (datetime('now')))",
                ),
                (
                    "CREATE TABLE t (at text DEFAULT CURRENT_TIMESTAMP)",
                    "CREATE TABLE t (at text DEFAULT CURRENT_TIMESTAMP)",
                ),
                ("CREATE TABLE t (n int DEFAULT 0)", "CREATE TABLE t (n int DEFAULT 0)"),
                (
                    "CREATE TABLE t (s text DEFAULT 'x')",
                    "CREATE TABLE t (s text DEFAULT 'x')",
                ),
                (
                    "CREATE TABLE t (s text DEFAULT NULL)",
                    "CREATE TABLE t (s text DEFAULT NULL)",
                ),
                (
                    "INSERT INTO t VALUES ('DEFAULT now()')",
                    "INSERT INTO t VALUES ('DEFAULT now()')",
                ),
            ],
        );
    }
}
