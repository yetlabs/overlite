//! Full-text search syntax SQLite has no words for: `@@`, which becomes ts_match with
//! the tsvector first whichever side it was on; a `||` with a tsvector on a side, which
//! becomes ts_concat; and casts to tsvector and tsquery, which read their input, and
//! to regconfig, which keeps the name.

use crate::rewrite::arrays::extend_past_call;
use crate::rewrite::casts::operand_before;
use crate::rewrite::operators::operand_after;
use overlite_engine::blank_literals;

/// Functions that give a tsvector.
const VECTOR_FUNCS: &[&str] = &["to_tsvector", "setweight", "strip", "ts_concat", "tsvector_in"];
/// Functions that give a tsquery.
const QUERY_FUNCS: &[&str] = &[
    "to_tsquery",
    "plainto_tsquery",
    "phraseto_tsquery",
    "websearch_to_tsquery",
    "tsquery_in",
];

fn contains_ci(s: &str, needle: &str) -> bool {
    s.as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// `@@`, a tsvector's `||` and `length`, and the casts of full-text search.
pub(crate) fn rewrite_fts(sql: &str) -> String {
    let code = blank_literals(sql);
    let casts = ["tsvector", "tsquery", "regconfig"]
        .iter()
        .any(|t| contains_ci(&code, t));
    let vectors =
        (code.contains("||") || contains_ci(&code, "length(")) && VECTOR_FUNCS.iter().any(|f| contains_ci(&code, f));
    if !code.contains("@@") && !casts && !vectors {
        return sql.to_string();
    }
    let sql = if casts { rewrite_casts(sql) } else { sql.to_string() };
    let sql = rewrite_concat(&sql);
    let sql = rewrite_length(&sql);
    rewrite_match(&sql)
}

/// `length` of what is plainly a tsvector counts its lexemes: ts_length. A column's
/// is the engine's to see (resolve/tsvector.rs).
fn rewrite_length(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut from = 0;
    loop {
        let code = blank_literals(&sql).to_ascii_lowercase();
        let Some(at) = code.get(from..).and_then(|c| c.find("length(")).map(|i| from + i) else {
            return sql;
        };
        from = at + 7;
        let b = code.as_bytes();
        if at > 0 && (b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_') {
            continue;
        }
        if calls(&sql[at + 7..], VECTOR_FUNCS) {
            sql = [&sql[..at], "ts_length(", &sql[at + 7..]].concat();
        }
    }
}

/// Whether an operand is a call of one of funcs, perhaps in parentheses.
fn calls(operand: &str, funcs: &[&str]) -> bool {
    let t = operand.trim_start().trim_start_matches(['(', ' ', '\t', '\n']);
    let name: String = t
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    funcs.iter().any(|f| name.eq_ignore_ascii_case(f)) && t[name.len()..].trim_start().starts_with('(')
}

/// `CAST(x AS tsvector)` is tsvector_in(x), `AS tsquery` tsquery_in(x), `AS regconfig`
/// x itself.
fn rewrite_casts(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut from = 0;
    loop {
        let code = blank_literals(&sql).to_ascii_uppercase();
        let Some(at) = code[from..].find("CAST(").map(|i| from + i) else {
            return sql;
        };
        from = at + 5;
        let b = code.as_bytes();
        if at > 0 && (b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_') {
            continue;
        }
        let mut depth = 0;
        let mut close = None;
        for (i, c) in b.iter().enumerate().skip(at + 4) {
            match c {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { return sql };
        let inner = &code[at + 5..close];
        let Some(as_at) = inner.rfind(" AS ") else { continue };
        let ty = inner[as_at + 4..].trim().trim_matches('"').to_ascii_lowercase();
        let ty = ty.strip_prefix("pg_catalog.").unwrap_or(&ty).to_string();
        let expr = sql[at + 5..at + 5 + as_at].to_string();
        let repl = match ty.as_str() {
            "tsvector" => format!("tsvector_in({expr})"),
            "tsquery" => format!("tsquery_in({expr})"),
            "regconfig" => format!("({expr})"),
            _ => continue,
        };
        sql = [&sql[..at], &repl, &sql[close + 1..]].concat();
        from = at;
    }
}

/// The next operator op outside a literal from from.
fn find_op(sql: &str, op: &str, from: usize) -> Option<usize> {
    let code = blank_literals(sql);
    code.get(from..)?.find(op).map(|i| from + i)
}

/// `a || b` with a tsvector on a side: ts_concat(a, b).
fn rewrite_concat(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut from = 0;
    while let Some(at) = find_op(&sql, "||", from) {
        from = at + 2;
        let (ls, le) = operand_before(&sql, at);
        let (rs, re) = operand_after(&sql, at + 2);
        if ls == le || rs == re {
            continue;
        }
        let re = extend_past_call(&sql, re);
        let (left, right) = (&sql[ls..le], &sql[rs..re]);
        if !calls(left, VECTOR_FUNCS) && !calls(right, VECTOR_FUNCS) {
            continue;
        }
        let repl = format!("ts_concat({left}, {right})");
        sql = [&sql[..ls], &repl, &sql[re..]].concat();
        from = ls;
    }
    sql
}

/// `v @@ q` and `q @@ v`: ts_match(v, q).
fn rewrite_match(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut from = 0;
    while let Some(at) = find_op(&sql, "@@", from) {
        from = at + 2;
        let (ls, le) = operand_before(&sql, at);
        let (rs, re) = operand_after(&sql, at + 2);
        if ls == le || rs == re {
            continue;
        }
        let re = extend_past_call(&sql, re);
        let (mut left, mut right) = (&sql[ls..le], &sql[rs..re]);
        if calls(left, QUERY_FUNCS) && !calls(right, QUERY_FUNCS) {
            std::mem::swap(&mut left, &mut right);
        }
        let repl = format!("ts_match({left}, {right})");
        sql = [&sql[..ls], &repl, &sql[re..]].concat();
        from = ls;
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_syntax() {
        for (sql, want) in [
            (
                "SELECT id FROM t WHERE (title || body) @@ (to_tsquery('bread'))",
                "SELECT id FROM t WHERE ts_match((title || body), (to_tsquery('bread')))",
            ),
            (
                "SELECT to_tsquery('a') @@ v FROM t",
                "SELECT ts_match(v, to_tsquery('a')) FROM t",
            ),
            (
                "SELECT setweight(to_tsvector('a'), 'A') || setweight(to_tsvector('b || c'), 'B')",
                "SELECT ts_concat(setweight(to_tsvector('a'), 'A'), setweight(to_tsvector('b || c'), 'B'))",
            ),
            (
                "SELECT to_tsvector(CAST('simple' AS regconfig), 'x'), CAST('a:1' AS tsvector), CAST('a & b' AS tsquery)",
                "SELECT to_tsvector(('simple'), 'x'), tsvector_in('a:1'), tsquery_in('a & b')",
            ),
            ("SELECT a || b FROM t", "SELECT a || b FROM t"),
            (
                "SELECT length(CAST('a b' AS tsvector)), length('x')",
                "SELECT ts_length(tsvector_in('a b')), length('x')",
            ),
            ("SELECT '@@'", "SELECT '@@'"),
        ] {
            assert_eq!(rewrite_fts(sql), want, "{sql}");
        }
    }
}
