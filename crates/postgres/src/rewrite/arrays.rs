//! Array constructors and membership tests.

use crate::auth::rls::balanced_paren;
use crate::auth::rls::top_level_keyword;
use crate::ddl::sequences::{read_paren_args, split_top_level};
use crate::rewrite::casts::operand_before;
use crate::rewrite::catalog::index_word_call;
use crate::rewrite::distinct_on::index_top_level_word;
use crate::rewrite::operators::operand_after;
use crate::rewrite::splice;
use crate::rewrite::strings::map_outside_strings;
use crate::rewrite::tokens::code_tokens;
use overlite_core::lazy_regex;
use overlite_core::text::{replace_all, replace_all_func};
use overlite_engine::code_only;

// Matches Postgres' ARRAY(subquery) constructor, which SQLite lacks.
lazy_regex!(RE_ARRAY_SUBQUERY, r"(?i)\barray\s*\(\s*(select\b)");

/// ARRAY(SELECT x ...) is the JSON array of the rows' x, in their order:
/// `overlite_array((SELECT json_group_array(_a) FROM (SELECT x AS _a ...)))`, the call
/// telling the engine the column is an array. A subquery of more than one column,
/// which Postgres refuses, is left a plain scalar subquery.
pub(crate) fn rewrite_array_subquery(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut from = 0;
    loop {
        let code = code_only(&sql);
        let Some(m) = RE_ARRAY_SUBQUERY.captures(&code[from..]) else {
            return sql;
        };
        let (Some(whole), Some(select)) = (m.get(0), m.get(1)) else {
            return sql;
        };
        let (start, select) = (from + whole.start(), from + select.start());
        let Some(open) = sql[start..select].find('(').map(|o| start + o) else {
            return sql;
        };
        let Some((inner, end)) = read_paren_args(&sql, open) else {
            return sql;
        };
        let list_start = select - open - 1 + "select".len();
        let list_end = [
            "from",
            "where",
            "group",
            "having",
            "order",
            "limit",
            "union",
            "except",
            "intersect",
        ]
        .iter()
        .filter_map(|kw| top_level_keyword(&inner[list_start..], kw).map(|p| list_start + p))
        .min()
        .unwrap_or(inner.len());
        let list = &inner[list_start..list_end];
        if split_top_level(list).len() != 1 {
            sql = splice(&sql, start, select, "(");
            from = start + 1;
            continue;
        }
        // The item without its label, if it has one.
        let item = match last_as(list) {
            Some(at) => &list[..at],
            None => list,
        };
        let repl = [
            "overlite_array((SELECT json_group_array(_a) FROM (SELECT",
            item.trim_end(),
            " AS _a ",
            inner[list_end..].trim_start(),
            ")))",
        ]
        .concat();
        sql = splice(&sql, start, end, &repl);
        from = start + "overlite_array(".len();
    }
}

/// Where the AS of a select-list item's label is, outside parentheses.
fn last_as(item: &str) -> Option<usize> {
    let toks = code_tokens(item);
    let n = toks.len();
    (n >= 2 && toks[n - 2].depth == 0 && toks[n - 2].is_word(item, "as")).then(|| toks[n - 2].span.start)
}

// Matches "= ANY (ARRAY[...])" / "<> ALL (ARRAY[...])", the array
// membership tests psql and pg_dump use, mapping them to SQLite IN / NOT IN.
lazy_regex!(RE_ANY_ARRAY, r"(?is)\s*=\s*any\s*\(\s*array\s*\[([^\]]*)\]\s*\)");
lazy_regex!(
    RE_ALL_ARRAY,
    r"(?is)\s*(?:<>|!=)\s*all\s*\(\s*array\s*\[([^\]]*)\]\s*\)"
);

/// Not string-literal-wrapped: the ARRAY[...] elements are themselves string
/// literals, so the match legitimately spans quotes.
pub(crate) fn rewrite_any_array(sql: &str) -> String {
    let sql = replace_all(&RE_ANY_ARRAY, sql, " IN ($1)");
    replace_all(&RE_ALL_ARRAY, &sql, " NOT IN ($1)")
}

// Matches "= ANY (subquery)". SQLite has no ANY; IN is close
// enough for the (non-executing) catalog queries that use it.
lazy_regex!(RE_ANY_OPERATOR, r"(?i)\s*=\s*any\s*\(");

pub(crate) fn rewrite_any_operator(sql: &str) -> String {
    map_outside_strings(sql, |code| replace_all(&RE_ANY_OPERATOR, code, " IN ("))
}

// Matches a Postgres array subscript directly attached to an
// identifier (e.g. prattrs[s]). Requiring no space before "[" avoids touching
// SQLite's [ident] quoting. Non-empty brackets only: `x[1]` is a subscript, but
// `text[]` is an array-type declaration. Captures the array expr and the index.
// The array may be qualified, `c.confkey[1]`, and its parts quoted.
lazy_regex!(
    RE_ARRAY_SUBSCRIPT,
    r#"((?:(?:"[^"]+"|[A-Za-z_]\w*)\.)*(?:"[^"]+"|[A-Za-z_]\w*))\[([^\]]+)\]"#
);

/// Maps `arr[i]` (1-based) onto json_extract over the JSON-array storage,
/// guarded by json_valid so a non-array/NULL value yields NULL.
pub(crate) fn rewrite_array_subscript(sql: &str) -> String {
    map_outside_strings(sql, |code| {
        replace_all_func(&RE_ARRAY_SUBSCRIPT, code, |m| {
            let Some(sm) = RE_ARRAY_SUBSCRIPT.captures(m) else {
                return m.to_string();
            };
            let arr = sm.get(1).map_or("", |x| x.as_str());
            let idx = sm.get(2).map_or("", |x| x.as_str());
            [
                "CASE WHEN json_valid(",
                arr,
                ") THEN json_extract(",
                arr,
                ", '$[' || ((",
                idx,
                ")-1) || ']') END",
            ]
            .concat()
        })
    })
}

/// Maps the Postgres overlap operator "a && b" (ranges that share a point, or
/// arrays with a common element) onto overlite_overlaps(a, b).
pub(crate) fn rewrite_overlaps(sql: &str) -> String {
    let mut sql = sql.to_string();
    loop {
        let Some(idx) = index_overlap(&sql) else { return sql };
        let (ls, le) = operand_before(&sql, idx);
        let (rs, re) = operand_after(&sql, idx + 2);
        if ls == le || rs == re {
            return sql; // malformed; avoid looping
        }
        let re = extend_past_call(&sql, re);
        let repl = ["overlite_overlaps(", &sql[ls..le], ", ", &sql[rs..re], ")"].concat();
        sql = splice(&sql, ls, re, &repl);
    }
}

/// Extends an operand end past a following function call's argument list:
/// operand_after stops at the "(" of a call like int4range(1,10), leaving the
/// parens dangling, so pull them in.
pub(crate) fn extend_past_call(sql: &str, re: usize) -> usize {
    let b = sql.as_bytes();
    let mut re = re;
    while re < b.len() {
        let mut p = re;
        while p < b.len() && (b[p] == b' ' || b[p] == b'\t') {
            p += 1;
        }
        if p < b.len() && b[p] == b'(' {
            re = balanced_paren(sql, p).1;
            continue;
        }
        break;
    }
    re
}

/// Finds the first "&&" operator outside a string literal.
fn index_overlap(sql: &str) -> Option<usize> {
    let b = sql.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] == b'\'' {
            in_str = !in_str;
            i += 1;
            continue;
        }
        if !in_str && b[i] == b'&' && b[i + 1] == b'&' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Turns array_to_string(array(SELECT expr FROM ... ORDER BY ...), sep) into
/// (SELECT group_concat(_v, sep) FROM (SELECT expr AS _v FROM ... ORDER BY ...))
/// — the shape psql \dT+ uses to list an enum's elements.
pub(crate) fn rewrite_array_to_string_subquery(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut low = sql.to_ascii_lowercase();
    let mut from = 0usize;
    loop {
        let Some(i) = index_word_call(&low, "array_to_string", from) else {
            return sql;
        };
        let b = sql.as_bytes();
        let mut open = i + "array_to_string".len();
        while open < b.len() && b[open] == b' ' {
            open += 1;
        }
        let Some((args, end)) = read_paren_args(&sql, open) else {
            return sql;
        };
        let parts = split_top_level(&args);
        let Some((inner, sep)) = array_subquery_arg(&parts) else {
            from = end;
            continue;
        };
        let body = &inner.trim()["select".len()..];
        let aliased = match index_top_level_word(body, 0, "from") {
            Some(fi) => ["SELECT ", body[..fi].trim(), " AS _v ", &body[fi..]].concat(),
            None => ["SELECT ", body.trim(), " AS _v"].concat(),
        };
        let repl = ["(SELECT group_concat(_v, ", &sep, ") FROM (", &aliased, "))"].concat();
        sql = splice(&sql, i, end, &repl);
        low = sql.to_ascii_lowercase();
        from = i + repl.len();
    }
}

/// Pulls the "array(SELECT ...)" inner query and the separator out of an
/// array_to_string argument list.
fn array_subquery_arg(parts: &[String]) -> Option<(String, String)> {
    if parts.len() < 2 {
        return None;
    }
    let arr = parts[0].trim();
    if !arr.to_lowercase().starts_with("array") {
        return None;
    }
    let ab = arr.as_bytes();
    let mut ap = "array".len();
    while ap < ab.len() && ab[ap] == b' ' {
        ap += 1;
    }
    if ap >= ab.len() || ab[ap] != b'(' {
        return None;
    }
    let (body, _) = read_paren_args(arr, ap)?;
    if !body.trim().to_lowercase().starts_with("select") {
        return None;
    }
    Some((body.clone(), parts[1].trim().to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::{assert_rewrites, rewrite};

    #[test]
    fn rewrite_any_array_cases() {
        assert_rewrites(
            rewrite,
            &[
                (
                    "WHERE relkind OPERATOR(pg_catalog.=) ANY (array['r', 'S', 'v'])",
                    "WHERE relkind IN ('r', 'S', 'v')",
                ),
                ("WHERE x <> ALL (ARRAY[1, 2])", "WHERE x NOT IN (1, 2)"),
            ],
        );
    }

    #[test]
    fn rewrite_array_and_any() {
        assert_rewrites(
            rewrite_array_subquery,
            &[
                (
                    "array(select rolname from pg_roles)",
                    "overlite_array((SELECT json_group_array(_a) FROM (SELECT rolname AS _a from pg_roles)))",
                ),
                (
                    "SELECT ARRAY(SELECT a AS x FROM t ORDER BY a DESC)",
                    "SELECT overlite_array((SELECT json_group_array(_a) FROM (SELECT a AS _a FROM t ORDER BY a DESC)))",
                ),
                ("ARRAY(SELECT a, b FROM t)", "(SELECT a, b FROM t)"),
                (
                    "SELECT array_to_string(ARRAY(SELECT a FROM t), ',')",
                    "SELECT array_to_string(overlite_array((SELECT json_group_array(_a) FROM (SELECT a AS _a FROM t))), ',')",
                ),
            ],
        );
        assert_rewrites(
            rewrite_any_operator,
            &[
                ("WHERE oid = any (pol.polroles)", "WHERE oid IN (pol.polroles)"),
                ("WHERE oid=ANY(pol.polroles)", "WHERE oid IN (pol.polroles)"),
            ],
        );
    }
}
