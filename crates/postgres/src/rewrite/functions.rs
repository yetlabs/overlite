//! SQL-standard function syntax: TRIM ... FROM, string_agg and the niladic functions.

use crate::rewrite::operators::operand_after;
use crate::rewrite::strings::map_outside_strings;
use crate::rewrite::{index_from, splice};
use overlite_core::lazy_regex;
use overlite_core::text::replace_all;

/// Converts SQL-standard trim syntax "trim([leading|trailing|both] chars from
/// expr)" into SQLite's function form (ltrim/rtrim/trim(expr, chars)). psql uses
/// it in its \d rule query.
pub(crate) fn rewrite_trim_from(sql: &str) -> String {
    let mut sql = sql.to_string();
    loop {
        let m = find_trim_from(&sql);
        if !m.ok {
            return sql;
        }
        let f = match m.kw.as_str() {
            "leading" => "ltrim",
            "trailing" => "rtrim",
            "both" => "trim",
            _ => "",
        };
        let repl = [f, "(", &sql[m.expr_start..m.expr_end], ", ", &m.chars, ")"].concat();
        sql = splice(&sql, m.start, m.call_end, &repl);
    }
}

#[derive(Debug, Default)]
struct TrimMatch {
    ok: bool,
    /// Span of the whole trim(...) call.
    start: usize,
    call_end: usize,
    expr_start: usize,
    expr_end: usize,
    kw: String,
    chars: String,
}

fn find_trim_from(sql: &str) -> TrimMatch {
    let low = sql.to_ascii_lowercase();
    let b = sql.as_bytes();
    let mut from = 0usize;
    loop {
        let Some(i) = index_from(&low, "trim(", from) else {
            return TrimMatch::default();
        };
        from = i + 5;

        let mut p = skip_spaces(sql, i + 5);
        let kw = read_trim_keyword(&low, p);
        if kw.is_empty() {
            continue; // ordinary trim(...)
        }
        p = skip_spaces(sql, p + kw.len());
        let (cs, ce) = operand_after(sql, p);
        if cs == ce {
            continue;
        }
        p = skip_spaces(sql, ce);
        if !has_word_at(&low, p, "from") {
            continue;
        }
        p = skip_spaces(sql, p + "from".len());

        // Consume expr up to the ')' that closes this trim( call.
        let mut depth = 1;
        let mut j = p;
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
            continue; // unbalanced
        }
        let mut expr_end = j;
        while expr_end > p && b[expr_end - 1] == b' ' {
            expr_end -= 1;
        }
        return TrimMatch {
            ok: true,
            start: i,
            call_end: j + 1,
            expr_start: p,
            expr_end,
            kw: kw.to_string(),
            chars: sql[cs..ce].to_string(),
        };
    }
}

fn read_trim_keyword(low: &str, p: usize) -> &'static str {
    for kw in ["leading", "trailing", "both"] {
        if has_word_at(low, p, kw) {
            return kw;
        }
    }
    ""
}

pub(crate) fn has_word_at(low: &str, p: usize, word: &str) -> bool {
    let l = low.as_bytes();
    let w = word.as_bytes();
    if p + w.len() > l.len() || &l[p..p + w.len()] != w {
        return false;
    }
    let n = p + w.len();
    n >= l.len() || l[n] == b' ' || l[n] == b'\t' || l[n] == b'\n' || l[n] == b'(' || l[n] == b'\''
}

pub(crate) fn skip_spaces(sql: &str, p: usize) -> usize {
    let b = sql.as_bytes();
    let mut p = p;
    while p < b.len() && (b[p] == b' ' || b[p] == b'\t' || b[p] == b'\n' || b[p] == b'\r') {
        p += 1;
    }
    p
}

// Matches SQL niladic special functions that Postgres allows without
// parentheses but SQLite requires as calls.
lazy_regex!(
    RE_NILADIC,
    r"(?i)\b(current_user|session_user|current_role|current_catalog|current_schema|current_database)\b"
);

/// Appends "()" to a bare niladic function so SQLite treats it as a call,
/// skipping matches that are quoted, qualified, or already called. It runs
/// outside string literals so a match inside a string is left alone.
pub(crate) fn rewrite_niladic_funcs(sql: &str) -> String {
    map_outside_strings(sql, |code| {
        let c = code.as_bytes();
        let mut b = String::with_capacity(code.len());
        let mut last = 0;
        for loc in RE_NILADIC.find_iter(code) {
            b.push_str(&code[last..loc.end()]);
            last = loc.end();
            if loc.start() > 0 {
                let p = c[loc.start() - 1];
                if p == b'"' || p == b'.' {
                    continue; // quoted identifier or qualified name
                }
            }
            let mut j = loc.end();
            while j < c.len() && c[j] == b' ' {
                j += 1;
            }
            if j < c.len() && c[j] == b'(' {
                continue; // already a call
            }
            b.push_str("()");
        }
        b.push_str(&code[last..]);
        b
    })
}

// Maps Postgres string_agg() onto SQLite's group_concat().
lazy_regex!(RE_STRING_AGG, r"(?i)\bstring_agg\s*\(");

// string_agg(DISTINCT x, sep): SQLite takes DISTINCT with one argument only.
lazy_regex!(RE_STRING_AGG_DISTINCT, r"(?i)\bstring_agg\s*\(\s*distinct\b");

pub(crate) fn rewrite_string_agg(sql: &str) -> String {
    map_outside_strings(sql, |code| {
        let code = replace_all(&RE_STRING_AGG_DISTINCT, code, "overlite_string_agg_distinct(");
        replace_all(&RE_STRING_AGG, &code, "group_concat(")
    })
}

/// The SQL/JSON constructors as the build functions: `JSON_ARRAY(a, b [NULL ON NULL]
/// [RETURNING t])` and `JSON_OBJECT(k VALUE v, k2 : v2 [ABSENT ON NULL] [WITH UNIQUE
/// KEYS] [RETURNING t])`, which Django writes for Postgres 16. JSON_ARRAY leaves NULLs
/// out unless NULL ON NULL, JSON_OBJECT keeps them unless ABSENT ON NULL, as Postgres'
/// defaults are. A JSON_OBJECT with none of these words is the older function and is
/// left alone.
pub(crate) fn rewrite_sql_json_constructors(sql: &str) -> String {
    let low = sql.to_ascii_lowercase();
    if !low.contains("json_array") && !low.contains("json_object") {
        return sql.to_string();
    }
    let mut sql = sql.to_string();
    for _ in 0..64 {
        let toks = crate::rewrite::tokens::code_tokens(&sql);
        let mut done = true;
        for i in (0..toks.len()).rev() {
            let array = toks[i].is_word(&sql, "json_array");
            if !(array || toks[i].is_word(&sql, "json_object"))
                || !toks.get(i + 1).is_some_and(|t| t.is_punct(&sql, "("))
            {
                continue;
            }
            if i > 0 && toks[i - 1].is_punct(&sql, ".") {
                continue;
            }
            let open = i + 1;
            let Some(close) =
                (open + 1..toks.len()).find(|&j| toks[j].depth == toks[open].depth && toks[j].is_punct(&sql, ")"))
            else {
                continue;
            };
            let depth = toks[open].depth + 1;
            let at = |j: usize, w: &str| toks.get(j).is_some_and(|t| t.depth == depth && t.is_word(&sql, w));
            // The clauses after the arguments, and where the arguments end.
            let mut end = close;
            let mut absent = array;
            let mut absent_set = false;
            let mut keyword = false;
            let mut j = open + 1;
            while j < close {
                if at(j, "returning") {
                    end = end.min(j);
                    keyword = true;
                } else if (at(j, "null") || at(j, "absent")) && at(j + 1, "on") && at(j + 2, "null") {
                    end = end.min(j);
                    keyword = true;
                    absent = at(j, "absent");
                    absent_set = true;
                    j += 2;
                } else if (at(j, "with") || at(j, "without")) && at(j + 1, "unique") {
                    end = end.min(j);
                    keyword = true;
                } else if !array
                    && (at(j, "value")
                        || (toks[j].depth == depth
                            && toks[j].is_punct(&sql, ":")
                            && !toks.get(j + 1).is_some_and(|t| t.is_punct(&sql, ":"))
                            && !toks[j - 1].is_punct(&sql, ":")))
                {
                    keyword = true;
                }
                j += 1;
            }
            // JSON_ARRAY is Postgres' constructor even without its words; a JSON_OBJECT
            // without them is the older function of text arrays.
            if !keyword && !array {
                continue;
            }
            if !keyword && end == open + 1 {
                continue;
            }
            let _ = absent_set;
            let args_text = if end > open + 1 {
                sql[toks[open + 1].span.start..toks[end - 1].span.end].to_string()
            } else {
                String::new()
            };
            let args = crate::ddl::sequences::split_top_level(&args_text);
            let args: Vec<String> = args
                .into_iter()
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect();
            let call = if array {
                let inner = ["jsonb_build_array(", &args.join(", "), ")"].concat();
                if absent && !args.is_empty() {
                    ["overlite_json_absent(", &inner, ")"].concat()
                } else {
                    inner
                }
            } else {
                let mut pairs = Vec::new();
                for a in &args {
                    let ac = overlite_engine::code_only(a);
                    let low = ac.to_ascii_lowercase();
                    let split = low
                        .match_indices(" value ")
                        .next()
                        .map(|(p, _)| (p, p + 7))
                        .or_else(|| {
                            ac.find(':')
                                .filter(|&p| !ac[p..].starts_with("::") && !ac[..p].ends_with(':'))
                                .map(|p| (p, p + 1))
                        });
                    let Some((k, v)) = split else { return sql };
                    pairs.push([a[..k].trim(), ", ", a[v..].trim()].concat());
                }
                let inner = ["jsonb_build_object(", &pairs.join(", "), ")"].concat();
                if absent && absent_set && !pairs.is_empty() {
                    ["overlite_json_absent(", &inner, ")"].concat()
                } else {
                    inner
                }
            };
            sql = [&sql[..toks[i].span.start], call.as_str(), &sql[toks[close].span.end..]].concat();
            done = false;
            break;
        }
        if done {
            break;
        }
    }
    sql
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn sql_json_constructors() {
        let cases = [
            (
                r#"SELECT JSON_ARRAY(("t"."a")::text, (1)::integer NULL ON NULL RETURNING JSONB) FROM t"#,
                r#"SELECT jsonb_build_array(("t"."a")::text, (1)::integer) FROM t"#,
            ),
            ("SELECT JSON_ARRAY( RETURNING JSONB)", "SELECT jsonb_build_array()"),
            (
                "SELECT JSON_OBJECT(('name')::text VALUE \"a\".\"n\", ('x')::text VALUE JSON_OBJECT( RETURNING JSONB) RETURNING JSONB)",
                "SELECT jsonb_build_object(('name')::text, \"a\".\"n\", ('x')::text, jsonb_build_object())",
            ),
            (
                "SELECT json_array(1, 2)",
                "SELECT overlite_json_absent(jsonb_build_array(1, 2))",
            ),
            ("SELECT json_object('a', 1)", "SELECT json_object('a', 1)"),
        ];
        for (sql, want) in cases {
            assert_eq!(rewrite_sql_json_constructors(sql), want, "{sql}");
        }
    }

    #[test]
    fn rewrite_niladic_funcs_cases() {
        assert_rewrites(
            rewrite_niladic_funcs,
            &[
                ("SELECT current_user", "SELECT current_user()"),
                (
                    "SELECT current_schema, current_user",
                    "SELECT current_schema(), current_user()",
                ),
                ("SELECT current_user()", "SELECT current_user()"),
                (r#"SELECT x AS "current_user""#, r#"SELECT x AS "current_user""#),
                ("SELECT n.current_user", "SELECT n.current_user"),
                ("SELECT 'current_user'", "SELECT 'current_user'"),
                (
                    "WHERE rolname = current_user AND a=1",
                    "WHERE rolname = current_user() AND a=1",
                ),
            ],
        );
    }

    #[test]
    fn rewrite_trim_from_cases() {
        assert_rewrites(
            rewrite_trim_from,
            &[
                ("trim(trailing ';' from x)", "rtrim(x, ';')"),
                ("trim(leading ' ' from name)", "ltrim(name, ' ')"),
                ("trim(both '-' from a)", "trim(a, '-')"),
                (
                    "trim(trailing ';' from pg_get_ruledef(r.oid))",
                    "rtrim(pg_get_ruledef(r.oid), ';')",
                ),
                ("trim(x)", "trim(x)"),
                (
                    "SELECT trim(both '.' from a), b FROM t",
                    "SELECT trim(a, '.'), b FROM t",
                ),
            ],
        );
    }
}
