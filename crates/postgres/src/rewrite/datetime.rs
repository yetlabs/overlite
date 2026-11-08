//! now(), intervals and EXTRACT.

use crate::rewrite::catalog::is_word_byte;
use crate::rewrite::functions::{has_word_at, skip_spaces};
use crate::rewrite::strings::map_outside_strings;
use crate::rewrite::{index_from, splice};
use crate::session::utility::first_word_upper;
use overlite_core::lazy_regex;
use overlite_core::text::replace_all;
use overlite_engine::code_only;

// The Postgres "current timestamp" niladic functions. In a statement that runs they
// are overlite_now: the start of the transaction, or outside one SQLite's clock for
// the statement; statement_timestamp() is the statement's even in a transaction, and
// clock_timestamp() moves on as it is read. In DDL, where a DEFAULT keeps the text for
// other readers of the file, SQLite's datetime('now'). (CURRENT_DATE and CURRENT_TIME
// are SQLite keywords and pass through untouched.)
lazy_regex!(
    RE_NOW_FUNCS,
    r"(?i)\b(?:now|transaction_timestamp|statement_timestamp)\s*\(\s*\)"
);
lazy_regex!(RE_STATEMENT_FUNC, r"(?i)\bstatement_timestamp\s*\(\s*\)");
lazy_regex!(RE_CLOCK_FUNC, r"(?i)\bclock_timestamp\s*\(\s*\)");
lazy_regex!(RE_CURRENT_TIMESTAMP, r"(?i)\bcurrent_timestamp\b(?:\s*\(\s*\))?");

pub(crate) fn rewrite_now(sql: &str) -> String {
    if matches!(first_word_upper(sql).as_str(), "CREATE" | "ALTER") {
        return map_outside_strings(sql, |code| {
            let code = replace_all(&RE_CLOCK_FUNC, code, "datetime('now')");
            replace_all(&RE_NOW_FUNCS, &code, "datetime('now')")
        });
    }
    map_outside_strings(sql, |code| {
        let code = replace_all(&RE_CLOCK_FUNC, code, "overlite_clock_timestamp()");
        let code = if code.to_ascii_lowercase().contains("statement_timestamp") {
            replace_all(&RE_STATEMENT_FUNC, &code, "overlite_now(julianday('now'), 'statement')")
        } else {
            code
        };
        let code = replace_all(&RE_CURRENT_TIMESTAMP, &code, "overlite_now(julianday('now'))");
        replace_all(&RE_NOW_FUNCS, &code, "overlite_now(julianday('now'))")
    })
}

/// Turns "extract(field FROM ts)" into overlite_extract('field', ts), which the engine
/// implements: date_part's value as the numeric Postgres 14 gives. SQLite has no
/// EXTRACT.
pub(crate) fn rewrite_extract(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut low = code_only(&sql).to_ascii_lowercase();
    let mut from = 0usize;
    loop {
        let Some(i) = index_from(&low, "extract", from) else {
            return sql;
        };
        from = i + "extract".len();
        if i > 0 && is_word_byte(low.as_bytes()[i - 1]) {
            continue; // part of a longer identifier
        }
        let b = sql.as_bytes();
        let mut p = skip_spaces(&sql, i + "extract".len());
        if p >= b.len() || b[p] != b'(' {
            continue;
        }
        p = skip_spaces(&sql, p + 1);
        let field_start = p;
        while p < b.len() && is_word_byte(b[p]) {
            p += 1;
        }
        let field = &sql[field_start..p];
        let q = skip_spaces(&sql, p);
        if !has_word_at(&low, q, "from") {
            continue;
        }
        let expr_start = skip_spaces(&sql, q + "from".len());
        let mut depth = 1;
        let mut j = expr_start;
        while j < b.len() && depth > 0 {
            match b[j] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b'\'' => {
                    j += 1;
                    while j < b.len() && b[j] != b'\'' {
                        j += 1;
                    }
                }
                _ => {}
            }
            if depth == 0 {
                break;
            }
            j += 1;
        }
        if depth != 0 {
            continue;
        }
        let mut expr_end = j;
        while expr_end > expr_start && b[expr_end - 1] == b' ' {
            expr_end -= 1;
        }
        let repl = [
            "overlite_extract('",
            &field.to_lowercase(),
            "', ",
            &sql[expr_start..expr_end],
            ")",
        ]
        .concat();
        sql = splice(&sql, i, j + 1, &repl);
        low = code_only(&sql).to_ascii_lowercase();
        from = i + repl.len();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn rewrite_now_cases() {
        assert_rewrites(
            rewrite_now,
            &[
                ("SELECT now()", "SELECT overlite_now(julianday('now'))"),
                ("SELECT NOW( )", "SELECT overlite_now(julianday('now'))"),
                (
                    "SELECT transaction_timestamp()",
                    "SELECT overlite_now(julianday('now'))",
                ),
                ("SELECT clock_timestamp()", "SELECT overlite_clock_timestamp()"),
                ("SELECT CURRENT_TIMESTAMP", "SELECT overlite_now(julianday('now'))"),
                (
                    "CREATE TABLE t (a timestamptz DEFAULT now())",
                    "CREATE TABLE t (a timestamptz DEFAULT datetime('now'))",
                ),
                ("SELECT known", "SELECT known"),
            ],
        );
    }

    #[test]
    fn rewrite_extract_cases() {
        assert_rewrites(
            rewrite_extract,
            &[
                ("SELECT extract(year FROM d)", "SELECT overlite_extract('year', d)"),
                ("SELECT extract(MONTH from ts)", "SELECT overlite_extract('month', ts)"),
                (
                    "WHERE extract(dow FROM created) = 0",
                    "WHERE overlite_extract('dow', created) = 0",
                ),
                (
                    "SELECT extract(day FROM now())",
                    "SELECT overlite_extract('day', now())",
                ),
                ("SELECT a, b", "SELECT a, b"),
            ],
        );
    }
}
