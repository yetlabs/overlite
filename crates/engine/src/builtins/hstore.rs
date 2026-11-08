//! hstore over SQLite: a key/value map stored as a JSON object in a TEXT cell, so
//! the existing json1 machinery (-> access, ? existence) works on it and the file
//! stays plain-SQLite readable. Input parses the `k=>v` text form to JSON; output
//! renders JSON back to hstore text.

use crate::builtins::json::{Json, json_marshal, json_number, json_quote, json_unmarshal};
use overlite_core::text::{eq_fold, slice};
use std::collections::HashMap;

/// Parses an hstore literal ("a=>1, b=>2") into a JSON object.
pub fn hstore_in(s: &str) -> String {
    let mut m: HashMap<String, Option<String>> = HashMap::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        i = skip_ws(s, i);
        if i >= b.len() {
            break;
        }
        let Some((key, ni)) = hstore_token(s, i) else {
            break;
        };
        i = skip_ws(s, ni);
        if i + 1 >= b.len() || b[i] != b'=' || b[i + 1] != b'>' {
            break;
        }
        i = skip_ws(s, i + 2);
        let (val, ni2, is_null) = hstore_value(s, i);
        i = ni2;
        if is_null {
            m.insert(key, None);
        } else {
            m.insert(key, Some(val));
        }
        i = skip_ws(s, i);
        if i < b.len() && b[i] == b',' {
            i += 1;
        }
    }
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    let parts: Vec<String> = keys
        .iter()
        .map(|k| {
            let v = match &m[*k] {
                Some(v) => json_quote(v),
                None => "null".to_string(),
            };
            format!("{}:{}", json_quote(k), v)
        })
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// Reads a quoted or bare key token.
fn hstore_token(s: &str, mut i: usize) -> Option<(String, usize)> {
    let b = s.as_bytes();
    if i >= b.len() {
        return None;
    }
    if b[i] == b'"' {
        return Some(hstore_quoted(s, i));
    }
    let start = i;
    while i < b.len() && b[i] != b' ' && b[i] != b'\t' && b[i] != b'=' && b[i] != b',' {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((s[start..i].to_string(), i))
}

/// Reads a value, reporting the unquoted NULL keyword.
fn hstore_value(s: &str, mut i: usize) -> (String, usize, bool) {
    let b = s.as_bytes();
    if i < b.len() && b[i] == b'"' {
        let (v, ni) = hstore_quoted(s, i);
        return (v, ni, false);
    }
    let start = i;
    while i < b.len() && b[i] != b',' && b[i] != b' ' && b[i] != b'\t' {
        i += 1;
    }
    let tok = slice(s, start, i);
    if eq_fold(tok, "null") {
        return (String::new(), i, true);
    }
    (tok.to_string(), i, false)
}

fn hstore_quoted(s: &str, mut i: usize) -> (String, usize) {
    let p = s.as_bytes();
    i += 1; // opening quote
    let mut b: Vec<u8> = Vec::new();
    while i < p.len() {
        let c = p[i];
        if c == b'\\' && i + 1 < p.len() {
            b.push(p[i + 1]);
            i += 2;
            continue;
        }
        if c == b'"' {
            return (String::from_utf8_lossy(&b).into_owned(), i + 1);
        }
        b.push(c);
        i += 1;
    }
    (String::from_utf8_lossy(&b).into_owned(), i)
}

fn skip_ws(s: &str, mut i: usize) -> usize {
    let b = s.as_bytes();
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n') {
        i += 1;
    }
    i
}

fn sorted_keys(m: &HashMap<String, Json>) -> Vec<&String> {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    keys
}

/// The object in json_text; anything else is an empty map.
fn hstore_map(json_text: &str) -> HashMap<String, Json> {
    match json_unmarshal(json_text) {
        Some(Json::Obj(m)) => m,
        _ => HashMap::new(),
    }
}

fn val_to_string(v: &Json) -> String {
    match v {
        Json::Str(x) => x.clone(),
        Json::Bool(true) => "true".to_string(),
        Json::Bool(false) => "false".to_string(),
        Json::Num(x) => trim_num(*x),
        other => json_marshal(other),
    }
}

fn trim_num(f: f64) -> String {
    json_number(f)
}

/// The keys as a JSON array (the array storage format), sorted by key.
pub(crate) fn hstore_keys(json_text: &str) -> String {
    let m = hstore_map(json_text);
    let keys = sorted_keys(&m);
    let parts: Vec<String> = keys.iter().map(|k| json_quote(k)).collect();
    format!("[{}]", parts.join(","))
}

/// The values as a JSON array (the array storage format), sorted by key.
pub(crate) fn hstore_vals(json_text: &str) -> String {
    let m = hstore_map(json_text);
    let keys = sorted_keys(&m);
    let parts: Vec<String> = keys.iter().map(|k| json_quote(&val_to_string(&m[*k]))).collect();
    format!("[{}]", parts.join(","))
}
