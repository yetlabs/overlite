//! Inlining stored LANGUAGE sql functions at their call sites.

use crate::blank::blank_comments;
use crate::resolve::schema_qualify::{builder_string, is_ident_byte, lowercase, skip_literal};
use crate::sql_funcs::parse::skip_spaces2;
use crate::sql_funcs::{SqlFunc, any_func_named, have_functions, lookup_func};
use std::collections::HashMap;

/// Inlines calls to stored LANGUAGE sql functions. A call f(x, y) becomes
/// "(<body with x,y substituted>)". It loops so a function whose body calls
/// another function is fully expanded; a small cap guards recursion.
pub(crate) fn rewrite_sql_functions(query: &str) -> String {
    if !have_functions() || !query.contains('(') {
        return query.to_string();
    }
    let mut query = query.to_string();
    for _ in 0..16 {
        let (next, changed) = inline_functions_once(&query);
        if !changed {
            return next;
        }
        query = next;
    }
    query
}

fn inline_functions_once(sql: &str) -> (String, bool) {
    let s = sql.as_bytes();
    let mut b: Vec<u8> = Vec::new();
    let mut changed = false;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            let j = skip_literal(sql, i, c);
            b.extend_from_slice(&s[i..j]);
            i = j;
            continue;
        }
        if is_ident_byte(c) && (i == 0 || !is_ident_byte(s[i - 1])) {
            let mut j = i;
            while j < s.len() && is_ident_byte(s[j]) {
                j += 1;
            }
            let word = &sql[i..j];
            let k = skip_spaces2(sql, j);
            if k < s.len()
                && s[k] == b'('
                && any_func_named(word)
                && let Some((args, end)) = balanced_args(sql, k)
            {
                let parts = split_top_level_args(args);
                if let Some(f) = lookup_func(word, parts.len() as i64) {
                    b.push(b'(');
                    b.extend_from_slice(substitute_body(&f, &parts).as_bytes());
                    b.push(b')');
                    i = end;
                    changed = true;
                    continue;
                }
            }
            b.extend_from_slice(word.as_bytes());
            i = j;
            continue;
        }
        b.push(c);
        i += 1;
    }
    (builder_string(b), changed)
}

/// Replaces the function's parameters in its body with the call arguments: $1..$n
/// positionally and each named parameter by name, each wrapped in parentheses to
/// preserve precedence.
fn substitute_body(f: &SqlFunc, args: &[&str]) -> String {
    let wrapped: Vec<String> = args.iter().map(|a| format!("({})", a.trim())).collect();
    let mut by_name: HashMap<String, &str> = HashMap::new();
    for (i, name) in f.params.iter().enumerate() {
        if !name.is_empty() && i < wrapped.len() {
            by_name.insert(lowercase(name).into_owned(), &wrapped[i]);
        }
    }

    let mut b: Vec<u8> = Vec::new();
    // A comment in the body is nothing: a quote in it would hide a parameter, and a
    // `--` at its end would comment out the parenthesis the body is wrapped in.
    let body = blank_comments(&f.body);
    let body = body.as_ref();
    let s = body.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            let j = skip_literal(body, i, c);
            b.extend_from_slice(&s[i..j]);
            i = j;
            continue;
        }
        if c == b'$' && i + 1 < s.len() && s[i + 1] >= b'1' && s[i + 1] <= b'9' {
            let mut j = i + 1;
            while j < s.len() && s[j] >= b'0' && s[j] <= b'9' {
                j += 1;
            }
            let n = atoi_safe(&body[i + 1..j]);
            if n >= 1 && n <= wrapped.len() as i64 {
                b.extend_from_slice(wrapped[n as usize - 1].as_bytes());
                i = j;
                continue;
            }
        }
        if is_ident_byte(c) && (i == 0 || !is_ident_byte(s[i - 1])) {
            let mut j = i;
            while j < s.len() && is_ident_byte(s[j]) {
                j += 1;
            }
            let word = &body[i..j];
            if let Some(repl) = by_name.get(lowercase(word).as_ref()) {
                b.extend_from_slice(repl.as_bytes());
                i = j;
                continue;
            }
            b.extend_from_slice(word.as_bytes());
            i = j;
            continue;
        }
        b.push(c);
        i += 1;
    }
    builder_string(b)
}

/// The text between the parenthesis at open and its match, and the index just
/// past the closing parenthesis.
pub(crate) fn balanced_args(s: &str, open: usize) -> Option<(&str, usize)> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => i = skip_literal(s, i, b[i]) - 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[open + 1..i], i + 1));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Splits on commas outside parentheses/strings. An all-blank input yields no
/// parts (a zero-arg call).
pub(crate) fn split_top_level_args(s: &str) -> Vec<&str> {
    if s.trim().is_empty() {
        return Vec::new();
    }
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => i = skip_literal(s, i, b[i]) - 1,
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

pub(crate) fn atoi_safe(s: &str) -> i64 {
    let mut n: i64 = 0;
    for &c in s.as_bytes() {
        if !c.is_ascii_digit() {
            return -1;
        }
        n = n.wrapping_mul(10).wrapping_add((c - b'0') as i64);
    }
    n
}
