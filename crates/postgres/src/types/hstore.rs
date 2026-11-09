//! hstore values, stored as a JSON object.

use crate::auth::rls::text_or_empty;
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
