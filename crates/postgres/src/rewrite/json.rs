//! JSON constructors, paths and the containment and existence operators.

use crate::auth::rls::balanced_paren;
use crate::rewrite::arrays::extend_past_call;
use crate::rewrite::casts::operand_before;
use crate::rewrite::catalog::index_word_call;
use crate::rewrite::functions::skip_spaces;
use crate::rewrite::operators::operand_after;
use crate::rewrite::splice;
use crate::rewrite::strings::map_outside_strings;
use overlite_core::lazy_regex;
use overlite_core::text::replace_all_func;
use overlite_engine::code_only;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Maps Postgres JSON/JSONB builder/aggregate functions onto their SQLite json1
/// equivalents. The -> and ->> operators, plus json_extract/json_object/
/// json_array/json_type/json_set/json_insert, are identical in SQLite.
static JSON_FUNC_MAP: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("json_build_object", "json_object"),
        ("jsonb_build_object", "json_object"),
        ("json_build_array", "json_array"),
        ("jsonb_build_array", "json_array"),
        ("json_object_agg", "json_group_object"),
        ("jsonb_object_agg", "json_group_object"),
        ("json_agg", "json_group_array"),
        ("jsonb_agg", "json_group_array"),
        ("json_typeof", "json_type"),
        ("jsonb_typeof", "json_type"),
        ("jsonb_array_length", "json_array_length"),
        ("to_json", "json_quote"),
        ("to_jsonb", "json_quote"),
        ("jsonb_pretty", "json"),
        ("jsonb_set", "json_set"),
        ("jsonb_insert", "json_insert"),
    ])
});

lazy_regex!(
    RE_JSON_FUNC,
    r"(?i)\b(json_build_object|jsonb_build_object|json_build_array|jsonb_build_array|json_object_agg|jsonb_object_agg|json_agg|jsonb_agg|json_typeof|jsonb_typeof|jsonb_array_length|to_jsonb|to_json|jsonb_pretty|jsonb_set|jsonb_insert)\s*\("
);

/// The functions whose value is jsonb, which keep that type through a cast: the select
/// list says so, and the JSONB collation compares it.
const JSONB_FUNCS: [&str; 5] = [
    "jsonb_build_object",
    "jsonb_build_array",
    "jsonb_agg",
    "jsonb_object_agg",
    "to_jsonb",
];

/// Wraps each call of a function whose value is jsonb in a cast to jsonb.
fn cast_jsonb_calls(sql: &str) -> String {
    let mut sql = sql.to_string();
    for name in JSONB_FUNCS {
        let mut from = 0;
        loop {
            let low = code_only(&sql).to_ascii_lowercase();
            let Some(at) = index_word_call(&low, name, from) else {
                break;
            };
            let open = at + low[at..].find('(').unwrap_or(0);
            let end = balanced_paren(&sql, open).1;
            let repl = ["overlite_cast(", &sql[at..end], ", 'jsonb')"].concat();
            sql = splice(&sql, at, end, &repl);
            from = at + "overlite_cast(".len() + name.len();
        }
    }
    sql
}

pub(crate) fn rewrite_json_funcs(sql: &str) -> String {
    let lower = sql.to_ascii_lowercase();
    let sql = if JSONB_FUNCS.iter().any(|f| lower.contains(f)) {
        &cast_jsonb_calls(sql)
    } else {
        sql
    };
    map_outside_strings(sql, |code| {
        replace_all_func(&RE_JSON_FUNC, code, |m| {
            let name = m.split_once('(').map_or(m, |(name, _)| name).trim().to_lowercase();
            if let Some(repl) = JSON_FUNC_MAP.get(name.as_str()) {
                return [*repl, "("].concat();
            }
            m.to_string()
        })
    })
}

/// Turns the Postgres path operators "expr #> path" and "#>>" into the engine's
/// overlite_jsonb_path, which walks keys of objects and indexes of arrays as the value
/// it meets has them, and gives JSON or text. (-> and ->> are SQLite's, ->> made to
/// give text by the engine.)
pub(crate) fn rewrite_json_path(sql: &str) -> String {
    let mut sql = sql.to_string();
    loop {
        let Some((idx, op)) = index_hash_arrow(&sql) else {
            return sql;
        };
        let (ls, le) = json_operand_before(&sql, idx);
        let (rs, re) = operand_after(&sql, idx + op.len());
        if ls == le || rs == re {
            return sql; // unexpected shape; bail rather than loop
        }
        let re = extend_past_call(&sql, re);
        let text = if op == "#>>" { "1" } else { "0" };
        let repl = [
            "overlite_jsonb_path(",
            &sql[ls..le],
            ", ",
            &sql[rs..re],
            ", ",
            text,
            ")",
        ]
        .concat();
        sql = splice(&sql, ls, re, &repl);
    }
}

/// The operand before a JSON operator at idx, taking in the `->` and `->>` that reach
/// it: in `v -> 'd' @> x` it is `v -> 'd'`.
pub(crate) fn json_operand_before(sql: &str, idx: usize) -> (usize, usize) {
    let (mut ls, le) = operand_before(sql, idx);
    loop {
        let before = sql[..ls].trim_end();
        let op = if before.ends_with("->>") {
            3
        } else if before.ends_with("->") {
            2
        } else {
            return (ls, le);
        };
        let (prev, _) = operand_before(sql, before.len() - op);
        if prev >= ls {
            return (ls, le);
        }
        ls = prev;
    }
}

/// Gives what `->`, `#>` and a cast to jsonb yield the JSONB collation, which compares
/// and sorts the texts as the jsonb values they are: `(v -> 'k') = '{"l": "m"}'`,
/// `(v -> 'c') > '2'::jsonb`, an ORDER BY or DISTINCT over a key. SQLite gives `->`
/// minified JSON text, which compared as text told `{"l":"m"}` from `{"l": "m"}` and
/// put 14 before 2.
pub(crate) fn rewrite_jsonb_collate(sql: &str) -> String {
    if !sql.contains("->") && !sql.contains("overlite_jsonb_path(") && !sql.contains("'jsonb')") {
        return sql.to_string();
    }
    let mut sql = sql.to_string();
    let mut i = 0;
    while i + 1 < sql.len() {
        let b = sql.as_bytes();
        if b[i] == b'\'' {
            i = crate::rewrite::strings::end_of_string_literal(&sql, i);
            continue;
        }
        // A call whose value is jsonb.
        for call in ["overlite_jsonb_path(", "overlite_cast("] {
            if b[i..].starts_with(call.as_bytes()) && (i == 0 || !is_word_byte(b[i - 1])) {
                let open = i + call.len() - 1;
                let end = balanced_paren(&sql, open).1;
                let args = &sql[open..end];
                let jsonb = if call == "overlite_cast(" {
                    args.ends_with(", 'jsonb')")
                } else {
                    args.ends_with(", 0)")
                };
                if jsonb && !followed_by_collate(&sql, end) {
                    let repl = ["(", &sql[i..end], " COLLATE JSONB)"].concat();
                    sql = splice(&sql, i, end, &repl);
                    // Past the call's name: its arguments may hold more.
                    i += 1;
                    break;
                }
            }
        }
        let b = sql.as_bytes();
        if b[i] != b'-' || b.get(i + 1) != Some(&b'>') || b.get(i + 2) == Some(&b'>') {
            i += 1;
            continue;
        }
        // A chain of ->, which a ->> ends.
        let (ls, le) = json_operand_before(&sql, i);
        let end = chain_end(&sql, i);
        if ls == le || end == i + 2 {
            i += 2;
            continue;
        }
        let repl = ["(", &sql[ls..end], ") COLLATE JSONB"].concat();
        let len = repl.len();
        sql = splice(&sql, ls, end, &repl);
        i = ls + len;
    }
    sql
}

/// Where the chain of `->` with one at op ends.
fn chain_end(sql: &str, op: usize) -> usize {
    let mut op = op;
    loop {
        // A negative index, `-> -1`.
        let at = skip_spaces(sql, op + 2);
        let from = if sql.as_bytes().get(at) == Some(&b'-') {
            at + 1
        } else {
            op + 2
        };
        let (rs, re) = operand_after(sql, from);
        if rs == re {
            return op + 2;
        }
        let re = extend_past_call(sql, re);
        let next = skip_spaces(sql, re);
        let b = sql.as_bytes();
        if b.get(next) == Some(&b'-') && b.get(next + 1) == Some(&b'>') && b.get(next + 2) != Some(&b'>') {
            op = next;
            continue;
        }
        return re;
    }
}

/// Whether a COLLATE stands right after end: a call already given its collation.
fn followed_by_collate(sql: &str, end: usize) -> bool {
    sql[end..]
        .trim_start()
        .get(..7)
        .is_some_and(|w| w.eq_ignore_ascii_case("collate"))
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// Finds the first #> or #>> operator outside a string literal.
fn index_hash_arrow(sql: &str) -> Option<(usize, &'static str)> {
    let b = sql.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i + 1 < b.len() {
        match b[i] {
            b'\'' => in_str = !in_str,
            b'#' if !in_str && b[i + 1] == b'>' => {
                if i + 2 < b.len() && b[i + 2] == b'>' {
                    return Some((i, "#>>"));
                }
                return Some((i, "#>"));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Maps the jsonb containment operators "a @> b" and "a <@ b" onto the
/// json_contains(container, contained) function the engine provides.
pub(crate) fn rewrite_json_contains(sql: &str) -> String {
    let mut sql = sql.to_string();
    loop {
        let Some((idx, op)) = index_json_contain(&sql) else {
            return sql;
        };
        let (ls, le) = json_operand_before(&sql, idx);
        let (rs, re) = operand_after(&sql, idx + op.len());
        if ls == le || rs == re {
            return sql; // malformed; avoid looping
        }
        let re = extend_past_call(&sql, re); // include a right-hand function call's args
        let left = &sql[ls..le];
        let right = &sql[rs..re];
        let mut repl = ["json_contains(", left, ", ", right, ")"].concat();
        if op == "<@" {
            // a <@ b: b contains a
            repl = ["json_contains(", right, ", ", left, ")"].concat();
        }
        sql = splice(&sql, ls, re, &repl);
    }
}

/// Maps the jsonb key-existence operators `?` / `?|` / `?&` onto engine
/// functions, before SQLite mistakes `?` for a bind parameter.
pub(crate) fn rewrite_json_exists(sql: &str) -> String {
    let mut sql = sql.to_string();
    loop {
        let Some((idx, op)) = index_json_exists(&sql) else {
            return sql;
        };
        let (ls, le) = json_operand_before(&sql, idx);
        let (rs, mut re) = operand_after(&sql, idx + op.len());
        if ls == le || rs == re {
            return sql;
        }
        // Include a function call's argument list (operand_after stops at the "(").
        let b = sql.as_bytes();
        while re < b.len() {
            let mut p = re;
            while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
                p += 1;
            }
            if p < b.len() && b[p] == b'(' {
                re = balanced_paren(&sql, p).1;
                continue;
            }
            break;
        }
        let f = match op {
            "?|" => "jsonb_exists_any",
            "?&" => "jsonb_exists_all",
            _ => "jsonb_exists",
        };
        let repl = [f, "(", &sql[ls..le], ", ", &sql[rs..re], ")"].concat();
        sql = splice(&sql, ls, re, &repl);
    }
}

/// Finds the first ? / ?| / ?& operator outside a string literal.
fn index_json_exists(sql: &str) -> Option<(usize, &'static str)> {
    let b = sql.as_bytes();
    let mut in_str = false;
    for i in 0..b.len() {
        match b[i] {
            b'\'' => in_str = !in_str,
            b'?' => {
                if in_str {
                    continue;
                }
                if i + 1 < b.len() && b[i + 1] == b'|' {
                    return Some((i, "?|"));
                }
                if i + 1 < b.len() && b[i + 1] == b'&' {
                    return Some((i, "?&"));
                }
                return Some((i, "?"));
            }
            _ => {}
        }
    }
    None
}

/// Finds the first @> or <@ operator outside a string literal.
fn index_json_contain(sql: &str) -> Option<(usize, &'static str)> {
    let b = sql.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i + 1 < b.len() {
        match b[i] {
            b'\'' => in_str = !in_str,
            b'@' => {
                if !in_str && b[i + 1] == b'>' {
                    return Some((i, "@>"));
                }
            }
            b'<' if !in_str && b[i + 1] == b'@' => {
                return Some((i, "<@"));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn rewrite_jsonb_collate_cases() {
        assert_rewrites(
            rewrite_jsonb_collate,
            &[
                (
                    "SELECT id FROM t WHERE (v -> 'k') = '{}' ORDER BY v -> 'a' -> 0 DESC",
                    "SELECT id FROM t WHERE ((v -> 'k') COLLATE JSONB) = '{}' ORDER BY (v -> 'a' -> 0) COLLATE JSONB DESC",
                ),
                (
                    "SELECT \"t\".\"v\" -> 'a' ->> 'b', x FROM t",
                    "SELECT (\"t\".\"v\" -> 'a') COLLATE JSONB ->> 'b', x FROM t",
                ),
                (
                    "SELECT overlite_cast('1', 'jsonb'), overlite_jsonb_path(v, '{a}', 1) FROM t",
                    "SELECT (overlite_cast('1', 'jsonb') COLLATE JSONB), overlite_jsonb_path(v, '{a}', 1) FROM t",
                ),
                ("SELECT 'a -> b'", "SELECT 'a -> b'"),
            ],
        );
    }

    #[test]
    fn rewrite_json_funcs_cases() {
        assert_rewrites(
            rewrite_json_funcs,
            &[
                (
                    "SELECT jsonb_build_object('a', 1)",
                    "SELECT overlite_cast(json_object('a', 1), 'jsonb')",
                ),
                ("SELECT json_build_array(1, 2)", "SELECT json_array(1, 2)"),
                (
                    "SELECT jsonb_agg(x)",
                    "SELECT overlite_cast(json_group_array(x), 'jsonb')",
                ),
                (
                    "SELECT jsonb_object_agg(k, v)",
                    "SELECT overlite_cast(json_group_object(k, v), 'jsonb')",
                ),
                ("SELECT jsonb_typeof(doc)", "SELECT json_type(doc)"),
                ("SELECT jsonb_array_length(doc)", "SELECT json_array_length(doc)"),
                ("SELECT to_jsonb(x)", "SELECT overlite_cast(json_quote(x), 'jsonb')"),
                ("SELECT json_extract(doc, '$.a')", "SELECT json_extract(doc, '$.a')"),
                ("SELECT doc -> 'k', doc ->> 'k'", "SELECT doc -> 'k', doc ->> 'k'"),
            ],
        );
    }

    #[test]
    fn rewrite_json_path_cases() {
        assert_rewrites(
            rewrite_json_path,
            &[
                (
                    "SELECT doc #> '{a,b}' FROM t",
                    "SELECT overlite_jsonb_path(doc, '{a,b}', 0) FROM t",
                ),
                (
                    "WHERE doc #>> '{tags,0}' = 'x'",
                    "WHERE overlite_jsonb_path(doc, '{tags,0}', 1) = 'x'",
                ),
                ("SELECT a FROM t", "SELECT a FROM t"),
            ],
        );
    }
}
