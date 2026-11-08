//! hstore values, stored as a JSON object.

use crate::auth::rls::text_or_empty;
use crate::ddl::sequences::is_ident_part;
use crate::rewrite::strings::end_of_string_literal;
use crate::rewrite::utf8_string;
use overlite_core::Value;
use overlite_core::text::text_to_bytes;
use overlite_engine::{Json, hstore_in, json_marshal, json_unmarshal};

/// Renders a stored JSON object as Postgres hstore text (`"k"=>"v", …`), keys
/// sorted for a stable form. A value that isn't a JSON object passes through.
pub(crate) fn hstore_text(json_str: &str) -> String {
    // Null is an empty map.
    let Some(v) = json_unmarshal(json_str) else {
        return json_str.to_string();
    };
    let m = match &v {
        Json::Null => return String::new(),
        Json::Obj(m) => m,
        _ => return json_str.to_string(),
    };
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    let mut parts = Vec::with_capacity(keys.len());
    for k in keys {
        let mut val = "NULL".to_string();
        if m[k.as_str()] != Json::Null {
            val = hstore_quote_str(&hstore_scalar(&m[k.as_str()]));
        }
        parts.push([hstore_quote_str(k).as_str(), "=>", &val].concat());
    }
    parts.join(", ")
}

/// hstore_send of a stored JSON object: the number of pairs, then each key and value
/// with its length, -1 for a NULL value. A value bound as text is stored as sent, in
/// hstore's own spelling, and is read as that. None for what is neither.
pub(crate) fn hstore_binary(v: &Value) -> Option<Vec<u8>> {
    let stored = text_or_empty(v);
    let parsed = if let Some(parsed) = json_unmarshal(&stored) {
        parsed
    } else {
        let json = hstore_in(&stored);
        if json == "{}" && !stored.trim().is_empty() {
            return None;
        }
        json_unmarshal(&json)?
    };
    let m = match &parsed {
        Json::Null => return Some(vec![0; 4]),
        Json::Obj(m) => m,
        _ => return None,
    };
    fn put(out: &mut Vec<u8>, text: &str) {
        let bytes = text_to_bytes(text);
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    let mut out = (m.len() as u32).to_be_bytes().to_vec();
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort();
    for k in keys {
        put(&mut out, k);
        match &m[k.as_str()] {
            Json::Null => out.extend_from_slice(&(-1i32).to_be_bytes()),
            val => put(&mut out, &hstore_scalar(val)),
        }
    }
    Some(out)
}

fn hstore_quote_str(s: &str) -> String {
    let s = s.replace('\\', "\\\\").replace('"', "\\\"");
    ["\"", &s, "\""].concat()
}

fn hstore_scalar(v: &Json) -> String {
    if let Json::Str(s) = v {
        return s.clone();
    }
    json_marshal(v)
}

// hstore over SQLite: stored as a JSON object (see engine/hstore). A column
// declared `hstore` keeps that declared type (so we can format its output back
// to hstore text); the value flows through json1 for `->` and `?`.

/// Turns a `'k=>v'::hstore` literal into hstore_in('k=>v') (its JSON object), and
/// drops the cast on a non-literal (already JSON).
pub(crate) fn rewrite_hstore_cast(sql: &str) -> String {
    let low = sql.to_lowercase();
    if !low.contains("::hstore") && !low.contains(":: hstore") {
        return sql.to_string();
    }
    let s = sql.as_bytes();
    let mut b: Vec<u8> = Vec::with_capacity(s.len() + 16);
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' {
            let j = end_of_string_literal(sql, i);
            let lit = &s[i..j];
            let mut k = j;
            while k < s.len() && (s[k] == b' ' || s[k] == b'\t') {
                k += 1;
            }
            let m = hstore_cast_at(sql, k);
            if m > 0 {
                b.extend_from_slice(b"hstore_in(");
                b.extend_from_slice(lit);
                b.push(b')');
                i = m;
                continue;
            }
            b.extend_from_slice(lit);
            i = j;
            continue;
        }
        if c == b':' && i + 1 < s.len() && s[i + 1] == b':' {
            let m = hstore_cast_at(sql, i);
            if m > 0 {
                // expr::hstore -> drop cast
                i = m;
                continue;
            }
        }
        b.push(c);
        i += 1;
    }
    utf8_string(b)
}

/// Reports the index past a `::hstore` starting at pos, or 0.
fn hstore_cast_at(sql: &str, pos: usize) -> usize {
    let s = sql.as_bytes();
    if pos + 1 >= s.len() || s[pos] != b':' || s[pos + 1] != b':' {
        return 0;
    }
    let mut k = pos + 2;
    while k < s.len() && (s[k] == b' ' || s[k] == b'\t') {
        k += 1;
    }
    if sql[k..].to_lowercase().starts_with("hstore") {
        let e = k + "hstore".len();
        if e >= s.len() || !is_ident_part(s[e]) {
            return e;
        }
    }
    0
}
