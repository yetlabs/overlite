//! Finding the calls of stored LANGUAGE sql functions, and a body as a call inlines it.

use crate::blank::blank_comments;
use crate::db_state::DbState;
use crate::resolve::schema_qualify::{builder_string, is_ident_byte, lowercase, skip_literal};
use crate::sql_funcs::SqlFunc;
use crate::sql_funcs::parse::skip_spaces2;
use overlite_core::sync::RwLockExt;
use std::collections::HashMap;

/// A call of a stored function's name: `name(args)`, from start (its qualifier, when
/// it has one) to end (past the closing parenthesis).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionCall {
    /// Where the call begins, its qualifier included.
    pub start: usize,
    /// Just past its closing parenthesis.
    pub end: usize,
    /// As written, without its qualifier.
    pub name: String,
    /// Each argument's text, as written.
    pub args: Vec<String>,
}

/// The words after which `name (` is not a call but a table's or a view's column
/// list: `INSERT INTO t (a, b)`, `REFERENCES t (id)`, `CREATE INDEX i ON t (a)`, `WITH
/// x (a) AS`.
const NOT_A_CALL_AFTER: &[&str] = &[
    "into",
    "table",
    "references",
    "on",
    "index",
    "view",
    "exists",
    "trigger",
    "type",
    "with",
    "recursive",
    "sequence",
    "function",
    "procedure",
    "aggregate",
    "routine",
    "prepare",
    "execute",
    "copy",
    "analyze",
    "vacuum",
];

/// The outermost calls in sql of a name a stored function has, that skip does not
/// refuse, in order. The arguments of a call are its own: a call inside them is the
/// caller's to find. Literals, quoted names and dollar-quoted strings are skipped.
pub(crate) fn function_calls(st: &DbState, sql: &str, skip: &dyn Fn(&str) -> bool) -> Vec<FunctionCall> {
    let names = st.func_names.read_locked().clone();
    let mut out = Vec::new();
    if names.is_empty() || !sql.contains('(') {
        return out;
    }
    let s = sql.as_bytes();
    let mut prev_word: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            i = skip_literal(sql, i, c);
            prev_word = None;
            continue;
        }
        if c == b'$'
            && let Some(end) = dollar_quoted_end(sql, i)
        {
            i = end;
            prev_word = None;
            continue;
        }
        if is_ident_byte(c) && (i == 0 || !is_ident_byte(s[i - 1])) && !c.is_ascii_digit() {
            let mut j = i;
            while j < s.len() && is_ident_byte(s[j]) {
                j += 1;
            }
            let word = &sql[i..j];
            let k = skip_spaces2(sql, j);
            let after = prev_word.is_some_and(|(a, b)| NOT_A_CALL_AFTER.contains(&lowercase(&sql[a..b]).as_ref()));
            if k < s.len()
                && s[k] == b'('
                && !after
                && names.contains(lowercase(word).as_ref())
                && !skip(word)
                && let Some((args, end)) = balanced_args(sql, k)
            {
                out.push(FunctionCall {
                    start: qualifier_start(sql, i),
                    end,
                    name: word.to_string(),
                    args: split_top_level_args(args).into_iter().map(str::to_string).collect(),
                });
                i = end;
                prev_word = None;
                continue;
            }
            prev_word = Some((i, j));
            i = j;
            continue;
        }
        if !c.is_ascii_whitespace() && c != b'.' {
            prev_word = None;
        }
        i += 1;
    }
    out
}

/// Where the name at i begins with its qualifier: `public.f`, `"s".f`.
fn qualifier_start(sql: &str, i: usize) -> usize {
    let s = sql.as_bytes();
    if i == 0 || s[i - 1] != b'.' {
        return i;
    }
    let mut j = i - 1;
    if j > 0 && s[j - 1] == b'"' {
        let mut k = j - 1;
        while k > 0 && s[k - 1] != b'"' {
            k -= 1;
        }
        return k.saturating_sub(1);
    }
    while j > 0 && is_ident_byte(s[j - 1]) {
        j -= 1;
    }
    j
}

/// The end of a dollar-quoted string opening at i (`$$` or `$tag$`), None when i
/// opens none (a parameter `$1`).
fn dollar_quoted_end(sql: &str, i: usize) -> Option<usize> {
    let s = sql.as_bytes();
    let mut j = i + 1;
    if s.get(j).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    while j < s.len() && is_ident_byte(s[j]) {
        j += 1;
    }
    if s.get(j) != Some(&b'$') {
        return None;
    }
    let tag = &sql[i..=j];
    let close = sql[j + 1..].find(tag)?;
    Some(j + 1 + close + tag.len())
}

/// A function's body as a call inlines it: each parameter, $n and by name, replaced
/// by its argument, wrapped in parentheses.
pub fn inline_function_body(params: &[String], body: &str, args: &[&str]) -> String {
    let f = SqlFunc {
        params: params.to_vec(),
        body: body.to_string(),
        ..Default::default()
    };
    substitute_body(&f, args)
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
