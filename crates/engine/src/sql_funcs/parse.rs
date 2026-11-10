//! Reading the parts of a CREATE FUNCTION out of its text.

use crate::resolve::schema_qualify::{is_ident_byte, skip_literal};
use crate::sql_funcs::SqlFunc;
use crate::sql_funcs::rewrite::{balanced_args, split_top_level_args};
use overlite_core::text::{slice, split_words, trim_chars};

/// Parses a CREATE FUNCTION into a SqlFunc (name, param names, arity, body,
/// language). It handles a dollar-quoted or single-quoted body and the two
/// keyword orders (AS ... LANGUAGE ... / LANGUAGE ... AS ...).
pub(crate) fn parse_create_function(sql: &str) -> Option<SqlFunc> {
    let mut f = SqlFunc::default();
    let low = sql.to_ascii_lowercase();
    let fi = index_word(&low, "function")?;
    let s = sql.as_bytes();
    // name: after FUNCTION up to '(' or whitespace.
    let mut p = skip_spaces2(sql, fi + "function".len());
    let ns = p;
    while p < s.len() && s[p] != b'(' && !is_space2(s[p]) {
        p += 1;
    }
    f.name = trim_chars(sql[ns..p].trim(), "\"").to_string();
    if let Some(d) = f.name.rfind('.') {
        f.name = f.name[d + 1..].to_string(); // drop a schema qualifier
    }
    if f.name.is_empty() {
        return None;
    }
    // params: the first parenthesized group after the name.
    let op = index_byte_from(sql, b'(', p)?;
    let (params, after_params) = balanced_args(sql, op)?;
    (f.params, f.arity) = parse_params(params);
    f.args = collapse_spaces(params).trim().to_string();

    // body: dollar-quoted or single-quoted, or the SQL standard's BEGIN ATOMIC ... END
    // or RETURN expression, as pg_dump writes a function made with one.
    let (body, bs, be) = find_dollar_body(sql)
        .or_else(|| find_quoted_body(sql))
        .or_else(|| find_standard_body(sql, after_params))?;
    f.body = unqualified_params(body.trim(), &f.name, &f.params);

    // language: search outside the body region (default order puts it after).
    let region = format!("{} {}", &sql[..bs], &sql[be..]);
    f.lang = word_after_keyword(&region, "language");
    // SECURITY DEFINER, or EXTERNAL SECURITY DEFINER; the last one said wins.
    let low_region = region.to_ascii_lowercase();
    let words = split_words(&low_region);
    for w in words.windows(2) {
        if w[0] == "security" && (w[1] == "definer" || w[1] == "invoker") {
            f.secdef = w[1] == "definer";
        }
    }
    // return type: between RETURNS and the AS/LANGUAGE that follows (before body).
    f.rettype = extract_returns(&sql[after_params..bs]);
    Some(f)
}

/// Pulls the return type out of the "... RETURNS <type> [LANGUAGE ...] [AS]"
/// region between the parameter list and the body.
fn extract_returns(region: &str) -> String {
    let low = region.to_ascii_lowercase();
    let Some(i) = index_word(&low, "returns") else {
        return String::new();
    };
    let rest = &region[i + "returns".len()..];
    // Cut at a trailing LANGUAGE or AS keyword (whichever comes first).
    let mut end = rest.len();
    let lrest = rest.to_ascii_lowercase();
    for kw in ["language", "as"] {
        if let Some(k) = index_word(&lrest, kw)
            && k < end
        {
            end = k;
        }
    }
    collapse_spaces(&rest[..end]).trim().to_string()
}

/// Turns runs of whitespace into single spaces.
fn collapse_spaces(s: &str) -> String {
    split_words(s).join(" ")
}

/// Drops parameter names from an argument list, leaving the types (Postgres'
/// "identity arguments"): "price numeric, rate numeric" -> "numeric, numeric".
pub(crate) fn identity_args(args: &str) -> String {
    if args.trim().is_empty() {
        return String::new();
    }
    let parts = split_top_level_args(args);
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        let all = split_words(part.trim());
        let mut toks: &[&str] = &all;
        if !toks.is_empty() {
            match toks[0].to_lowercase().as_str() {
                "in" | "out" | "inout" | "variadic" => toks = &toks[1..],
                _ => {}
            }
        }
        if toks.len() >= 2 {
            // "<name> <type...>": drop the name
            toks = &toks[1..];
        }
        out.push(toks.join(" "));
    }
    out.join(", ")
}

/// Extracts the name and (if given) arity from DROP FUNCTION. arity is -1 when
/// no argument list is present.
pub(crate) fn parse_drop_function(sql: &str) -> Option<(String, i64)> {
    let low = sql.to_ascii_lowercase();
    let fi = index_word(&low, "function")?;
    let s = sql.as_bytes();
    let mut p = skip_spaces2(sql, fi + "function".len());
    if low[p..].starts_with("if exists") {
        p = skip_spaces2(sql, p + "if exists".len());
    }
    let ns = p;
    while p < s.len() && s[p] != b'(' && !is_space2(s[p]) && s[p] != b';' && s[p] != b',' {
        p += 1;
    }
    let mut name = trim_chars(sql[ns..p].trim(), "\"").to_string();
    if let Some(d) = name.rfind('.') {
        name = name[d + 1..].to_string();
    }
    if name.is_empty() {
        return None;
    }
    let mut ar = -1;
    if let Some(op) = index_byte_from(sql, b'(', p)
        && op == skip_spaces2(sql, p)
        && let Some((args, _)) = balanced_args(sql, op)
    {
        (_, ar) = parse_params(args);
    }
    Some((name, ar))
}

/// Splits a parameter list into per-slot names ("" when unnamed) and the arity.
fn parse_params(params: &str) -> (Vec<String>, i64) {
    if params.trim().is_empty() {
        return (Vec::new(), 0);
    }
    let parts = split_top_level_args(params);
    let mut names = vec![String::new(); parts.len()];
    for (i, part) in parts.iter().enumerate() {
        let all = split_words(part.trim());
        let mut toks: &[&str] = &all;
        if !toks.is_empty() {
            match toks[0].to_lowercase().as_str() {
                "in" | "out" | "inout" | "variadic" => toks = &toks[1..],
                _ => {}
            }
        }
        if toks.len() >= 2 {
            // "<name> <type> ...": the first token is the name
            names[i] = trim_chars(toks[0], "\"").to_string();
        }
    }
    let n = parts.len() as i64;
    (names, n)
}

/// The body of a $tag$...$tag$ quoted section and its span.
fn find_dollar_body(sql: &str) -> Option<(String, usize, usize)> {
    let s = sql.as_bytes();
    for i in 0..s.len() {
        if s[i] != b'$' {
            continue;
        }
        let mut j = i + 1;
        while j < s.len() && is_ident_byte(s[j]) {
            j += 1;
        }
        if j >= s.len() || s[j] != b'$' {
            continue;
        }
        let tag = &sql[i..=j]; // "$$" or "$name$"
        if let Some(k) = sql[j + 1..].find(tag) {
            return Some((sql[j + 1..j + 1 + k].to_string(), i, j + 1 + k + tag.len()));
        }
    }
    None
}

/// body with each parameter named through its function (`f.a`, as Postgres writes a
/// standard body back) named bare, as the call's substitution reads it.
fn unqualified_params(body: &str, name: &str, params: &[String]) -> String {
    let s = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            let j = skip_literal(body, i, c);
            out.push_str(&body[i..j]);
            i = j;
            continue;
        }
        if is_ident_byte(c) && (i == 0 || !is_ident_byte(s[i - 1]) && s[i - 1] != b'.') {
            let mut j = i;
            while j < s.len() && is_ident_byte(s[j]) {
                j += 1;
            }
            if body[i..j].eq_ignore_ascii_case(name) && s.get(j) == Some(&b'.') {
                let mut k = j + 1;
                while k < s.len() && is_ident_byte(s[k]) {
                    k += 1;
                }
                if params
                    .iter()
                    .any(|p| !p.is_empty() && p.eq_ignore_ascii_case(&body[j + 1..k]))
                {
                    out.push_str(&body[j + 1..k]);
                    i = k;
                    continue;
                }
            }
            out.push_str(&body[i..j]);
            i = j;
            continue;
        }
        let n = body[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&body[i..i + n]);
        i += n;
    }
    out
}

/// The body of a `BEGIN ATOMIC ... END` (its statements) or of a `RETURN expression`
/// (that expression's SELECT), after the parameters, and its span.
fn find_standard_body(sql: &str, from: usize) -> Option<(String, usize, usize)> {
    let low = sql[from..].to_ascii_lowercase();
    if let Some(b) = index_word(&low, "begin") {
        let a = skip_spaces2(&low, b + "begin".len());
        if low[a..].starts_with("atomic") {
            let start = a + "atomic".len();
            let end = low.rfind("end").filter(|&e| e >= start)?;
            let body = sql[from + start..from + end].trim().trim_end_matches(';').trim();
            return Some((body.to_string(), from + b, sql.len()));
        }
    }
    let r = index_word(&low, "return")?;
    let expr = sql[from + r + "return".len()..].trim().trim_end_matches(';').trim();
    Some((["SELECT ", expr].concat(), from + r, sql.len()))
}

/// The body of an `AS '...'` clause and its span.
fn find_quoted_body(sql: &str) -> Option<(String, usize, usize)> {
    let low = sql.to_ascii_lowercase();
    let s = sql.as_bytes();
    let mut from = 0;
    loop {
        let a = index_word(&low[from..], "as")? + from;
        let q = skip_spaces2(sql, a + 2);
        if q < s.len() && s[q] == b'\'' {
            let e = skip_literal(sql, q, b'\'');
            return Some((slice(sql, q + 1, e - 1).replace("''", "'"), q, e));
        }
        from = a + 2;
    }
}

/// The token following a whole-word keyword.
fn word_after_keyword(s: &str, keyword: &str) -> String {
    let low = s.to_ascii_lowercase();
    let Some(i) = index_word(&low, keyword) else {
        return String::new();
    };
    let b = s.as_bytes();
    let mut p = skip_spaces2(s, i + keyword.len());
    let start = p;
    while p < b.len() && is_ident_byte(b[p]) {
        p += 1;
    }
    s[start..p].to_lowercase()
}

/// Finds a whole-word (identifier-bounded) occurrence of word in low.
pub(crate) fn index_word(low: &str, word: &str) -> Option<usize> {
    let b = low.as_bytes();
    let mut from = 0;
    loop {
        let i = low[from..].find(word)? + from;
        let before = i == 0 || !is_ident_byte(b[i - 1]);
        let after = i + word.len() >= b.len() || !is_ident_byte(b[i + word.len()]);
        if before && after {
            return Some(i);
        }
        from = i + word.len();
    }
}

fn index_byte_from(s: &str, b: u8, from: usize) -> Option<usize> {
    s.as_bytes()[from..].iter().position(|&c| c == b).map(|i| from + i)
}

pub(crate) fn skip_spaces2(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let mut i = i;
    while i < b.len() && is_space2(b[i]) {
        i += 1;
    }
    i
}

#[inline]
fn is_space2(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}
