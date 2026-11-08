//! current_schema() and current_schemas() answer from the session search_path,
//! so they have to be resolved per statement: the SQLite functions they would
//! otherwise reach are registered globally and cannot see session state.

use crate::catalog::rls_cache::ascii_lower;
use crate::db_state::DbState;
use crate::resolve::schema_qualify::{builder_string, is_ident_byte, lowercase, skip_literal};
use crate::resolve::search_path::usable_search_path;
use overlite_core::text::quote_literal;

/// Replaces bare current_schema()/current_schemas() calls with the value this
/// session's search_path gives them.
pub(crate) fn resolve_current_schema(sp: Option<&[String]>, st: &DbState, query: &str) -> String {
    let lower = lowercase(query);
    if !lower.contains("current_schema") {
        return query.to_string();
    }
    let schema = current_schema(sp, st);

    let out = replace_call_outside_strings(query, "current_schemas", |args| {
        quote_literal(&current_schemas(sp, st, lowercase(args).contains("true")))
    });
    replace_call_outside_strings(&out, "current_schema", |_| {
        if schema.is_empty() {
            return "NULL".to_string();
        }
        quote_literal(&schema)
    })
}

/// The first schema in the search_path that exists, as Postgres defines it. With
/// no path, or one naming nothing that exists, the answer is public.
pub(crate) fn current_schema(sp: Option<&[String]>, st: &DbState) -> String {
    if let Some(s) = usable_search_path(st, sp).first() {
        return s.to_string();
    }
    "public".to_string()
}

/// Renders the path as a Postgres array literal, optionally with the implicit
/// pg_catalog in front.
fn current_schemas(sp: Option<&[String]>, st: &DbState, include_implicit: bool) -> String {
    let mut names: Vec<&str> = Vec::new();
    if include_implicit {
        names.push("pg_catalog");
    }
    names.extend(usable_search_path(st, sp));
    names.push("public");
    format!("{{{}}}", names.join(","))
}

/// Rewrites `name(...)` calls, passing the argument text to f. A match inside a
/// string or quoted identifier, or one that is part of a longer identifier or a
/// qualified name, is left alone.
pub(crate) fn replace_call_outside_strings(sql: &str, name: &str, mut f: impl FnMut(&str) -> String) -> String {
    let s = sql.as_bytes();
    let mut b: Vec<u8> = Vec::new();
    let lower = ascii_lower(sql);
    let low = lower.as_bytes();
    let nm = name.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            let j = skip_literal(sql, i, c);
            b.extend_from_slice(&s[i..j]);
            i = j;
            continue;
        }
        if !low[i..].starts_with(nm) {
            b.push(c);
            i += 1;
            continue;
        }
        if i > 0 && (is_ident_byte(s[i - 1]) || s[i - 1] == b'.') {
            b.push(c);
            i += 1;
            continue;
        }
        let mut j = i + nm.len();
        while j < s.len() && s[j] == b' ' {
            j += 1;
        }
        // current_schemas must not swallow a current_schema match and vice
        // versa: a longer identifier byte after the name means a different one.
        if j >= s.len() || s[j] != b'(' {
            b.push(c);
            i += 1;
            continue;
        }
        let Some((end, args)) = call_args(sql, j) else {
            b.push(c);
            i += 1;
            continue;
        };
        b.extend_from_slice(f(args).as_bytes());
        i = end;
    }
    builder_string(b)
}

/// The index just past the parenthesised argument list that starts at i, and
/// the text between the parentheses.
fn call_args(sql: &str, i: usize) -> Option<(usize, &str)> {
    let s = sql.as_bytes();
    let mut i = i;
    let mut depth = 0;
    let start = i + 1;
    while i < s.len() {
        match s[i] {
            b'\'' | b'"' => {
                i = skip_literal(sql, i, s[i]);
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((i + 1, &sql[start..i]));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}
