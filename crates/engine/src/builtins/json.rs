//! The JSON functions, and the JSON they rely on: decoding (numbers as f64, last
//! duplicate key wins, lone surrogates become U+FFFD), decoding an array to the
//! raw text of its elements, and encoding.

use overlite_core::format_float;
use overlite_core::text::{push_escaped, valid_utf8};
use std::collections::HashMap;

const JSON_MAX_DEPTH: usize = 10000;

/// A decoded JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    /// A number as its literal text: only `json_decode_first` gives these.
    Number(String),
    Arr(Vec<Json>),
    Obj(HashMap<String, Json>),
}

#[derive(Debug)]
struct JsonParser<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
    depth: usize,
    /// A value out of range for float64: syntactically fine, but Unmarshal
    /// reports it once decoding ends.
    range_err: bool,
    use_number: bool,
}

impl<'a> JsonParser<'a> {
    fn new(s: &'a str) -> Self {
        JsonParser {
            s,
            b: s.as_bytes(),
            pos: 0,
            depth: 0,
            range_err: false,
            use_number: false,
        }
    }

    fn ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\r' | b'\n') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn literal(&mut self, word: &str) -> Option<()> {
        if self.b[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Some(())
        } else {
            None
        }
    }

    /// Scans a number; returns its text.
    fn number(&mut self) -> Option<&'a str> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek()? {
            b'0' => self.pos += 1,
            b'1'..=b'9' => self.digits(),
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !self.peek()?.is_ascii_digit() {
                return None;
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !self.peek()?.is_ascii_digit() {
                return None;
            }
            self.digits();
        }
        Some(&self.s[start..self.pos])
    }

    fn digits(&mut self) {
        while self.pos < self.b.len() && self.b[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = self.b.get(self.pos..self.pos + 4)?;
        let mut v = 0u32;
        for &c in h {
            v = v * 16 + (c as char).to_digit(16)?;
        }
        self.pos += 4;
        Some(v)
    }

    /// Scans a string at the opening quote and decodes it.
    fn string(&mut self) -> Option<String> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        let mut start = self.pos;
        loop {
            let c = self.peek()?;
            match c {
                b'"' => {
                    out.push_str(&self.s[start..self.pos]);
                    self.pos += 1;
                    // Decoding coerces bytes that are not UTF-8 to U+FFFD.
                    return Some(valid_utf8(&out).into_owned());
                }
                0..=0x1f => return None,
                b'\\' => {
                    out.push_str(&self.s[start..self.pos]);
                    self.pos += 1;
                    let e = self.peek()?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let r = self.hex4()?;
                            let c = self.surrogate(r);
                            push_escaped(&mut out, c.encode_utf8(&mut [0; 4]));
                        }
                        _ => return None,
                    }
                    start = self.pos;
                }
                _ => self.pos += 1,
            }
        }
    }

    /// Resolves a \u escape: a valid pair joins, a lone half is U+FFFD.
    fn surrogate(&mut self, r: u32) -> char {
        if !(0xD800..0xE000).contains(&r) {
            return char::from_u32(r).unwrap_or('\u{FFFD}');
        }
        if r < 0xDC00 && self.b[self.pos..].starts_with(b"\\u") {
            let save = self.pos;
            self.pos += 2;
            if let Some(r2) = self.hex4()
                && (0xDC00..0xE000).contains(&r2)
            {
                let joined = (((r - 0xD800) << 10) | (r2 - 0xDC00)) + 0x10000;
                return char::from_u32(joined).unwrap_or('\u{FFFD}');
            }
            self.pos = save;
        }
        '\u{FFFD}'
    }

    /// Scans a value at any non-space byte, into `any`.
    fn value(&mut self) -> Option<Json> {
        self.ws();
        match self.peek()? {
            b'{' => {
                self.enter()?;
                let mut m = HashMap::new();
                self.ws();
                if self.peek()? == b'}' {
                    self.pos += 1;
                    self.depth -= 1;
                    return Some(Json::Obj(m));
                }
                loop {
                    self.ws();
                    if self.peek()? != b'"' {
                        return None;
                    }
                    let key = self.string()?;
                    self.ws();
                    if self.peek()? != b':' {
                        return None;
                    }
                    self.pos += 1;
                    let v = self.value()?;
                    m.insert(key, v);
                    self.ws();
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b'}' => {
                            self.pos += 1;
                            self.depth -= 1;
                            return Some(Json::Obj(m));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.enter()?;
                let mut a = Vec::new();
                self.ws();
                if self.peek()? == b']' {
                    self.pos += 1;
                    self.depth -= 1;
                    return Some(Json::Arr(a));
                }
                loop {
                    a.push(self.value()?);
                    self.ws();
                    match self.peek()? {
                        b',' => self.pos += 1,
                        b']' => {
                            self.pos += 1;
                            self.depth -= 1;
                            return Some(Json::Arr(a));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => Some(Json::Str(self.string()?)),
            b't' => self.literal("true").map(|()| Json::Bool(true)),
            b'f' => self.literal("false").map(|()| Json::Bool(false)),
            b'n' => self.literal("null").map(|()| Json::Null),
            _ => {
                let n = self.number()?;
                if self.use_number {
                    return Some(Json::Number(n.to_string()));
                }
                let f = n.parse::<f64>().unwrap_or(f64::INFINITY);
                if f.is_infinite() {
                    self.range_err = true;
                }
                Some(Json::Num(f))
            }
        }
    }

    fn enter(&mut self) -> Option<()> {
        self.pos += 1;
        self.depth += 1;
        if self.depth > JSON_MAX_DEPTH {
            return None;
        }
        Some(())
    }

    /// Whole-input check after the top-level value.
    fn finish(&mut self) -> Option<()> {
        self.ws();
        if self.pos == self.b.len() { Some(()) } else { None }
    }
}

/// Decodes the whole text; None is an error.
pub fn json_unmarshal(s: &str) -> Option<Json> {
    let mut p = JsonParser::new(s);
    let v = p.value()?;
    p.finish()?;
    if p.range_err {
        return None;
    }
    Some(v)
}

/// The keys of an object and the text of their values, as written. None if the text is
/// no object.
pub(crate) fn json_raw_object(s: &str) -> Option<Vec<(String, &str)>> {
    let mut p = JsonParser::new(s);
    p.use_number = true;
    p.ws();
    if p.peek()? != b'{' {
        return None;
    }
    p.pos += 1;
    let mut out = Vec::new();
    loop {
        p.ws();
        match p.peek()? {
            b'}' if out.is_empty() => break,
            b'"' => {}
            _ => return None,
        }
        let key = p.string()?;
        p.ws();
        if p.peek()? != b':' {
            return None;
        }
        p.pos += 1;
        p.ws();
        let start = p.pos;
        p.value()?;
        out.push((key, &s[start..p.pos]));
        p.ws();
        match p.peek()? {
            b',' => p.pos += 1,
            b'}' => break,
            _ => return None,
        }
    }
    p.pos += 1;
    p.finish()?;
    Some(out)
}

/// Whether the whole text is one JSON value, as json_in asks.
pub(crate) fn json_is_valid(s: &str) -> bool {
    let mut p = JsonParser::new(s);
    p.use_number = true;
    p.value().is_some() && p.finish().is_some()
}

/// Decodes only the first value of the text, keeping numbers as written. None is
/// an error.
pub fn json_decode_first(s: &str) -> Option<Json> {
    let mut p = JsonParser::new(s);
    p.use_number = true;
    p.value()
}

/// Decodes an array to the raw text of its elements. JSON null decodes without
/// error, to no elements. None is an error.
pub(crate) fn json_unmarshal_raw_array(s: &str) -> Option<Vec<&str>> {
    let mut p = JsonParser::new(s);
    p.ws();
    match p.peek()? {
        b'[' => {
            p.enter()?;
            let mut out = Vec::new();
            p.ws();
            if p.peek()? == b']' {
                p.pos += 1;
            } else {
                loop {
                    p.ws();
                    let start = p.pos;
                    p.value()?;
                    out.push(&s[start..p.pos]);
                    p.ws();
                    match p.peek()? {
                        b',' => p.pos += 1,
                        b']' => {
                            p.pos += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            p.finish()?;
            Some(out)
        }
        _ => {
            // Any other valid value is a type mismatch, except null.
            match p.value()? {
                Json::Null => {
                    p.finish()?;
                    Some(Vec::new())
                }
                _ => None,
            }
        }
    }
}

/// Encodes a JSON string, with <, > and & escaped.
pub(crate) fn json_quote(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let s = &*valid_utf8(s);
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let esc: Option<String> = match c {
            b'"' => Some("\\\"".into()),
            b'\\' => Some("\\\\".into()),
            0x08 => Some("\\b".into()),
            0x0c => Some("\\f".into()),
            b'\n' => Some("\\n".into()),
            b'\r' => Some("\\r".into()),
            b'\t' => Some("\\t".into()),
            0..=0x1f | b'<' | b'>' | b'&' => Some(format!(
                "\\u00{}{}",
                HEX[(c >> 4) as usize] as char,
                HEX[(c & 0xf) as usize] as char
            )),
            // U+2028 / U+2029 are E2 80 A8 / E2 80 A9.
            0xE2 if b[i + 1] == 0x80 && (b[i + 2] == 0xA8 || b[i + 2] == 0xA9) => {
                out.push_str(&s[start..i]);
                out.push_str(if b[i + 2] == 0xA8 { "\\u2028" } else { "\\u2029" });
                i += 3;
                start = i;
                continue;
            }
            _ => None,
        };
        match esc {
            Some(e) => {
                out.push_str(&s[start..i]);
                out.push_str(&e);
                i += 1;
                start = i;
            }
            None => i += 1,
        }
    }
    out.push_str(&s[start..]);
    out.push('"');
    out
}

/// Encodes a JSON number.
pub(crate) fn json_number(f: f64) -> String {
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        let sci = format!("{f:e}");
        let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
        return match exp.strip_prefix('-') {
            Some(digits) => format!("{mant}e-{digits}"),
            None => format!("{mant}e+{exp}"),
        };
    }
    format!("{f}")
}

/// Encodes a decoded value: compact, object keys sorted.
pub fn json_marshal(v: &Json) -> String {
    match v {
        Json::Null => "null".to_string(),
        Json::Bool(b) => b.to_string(),
        Json::Num(f) => json_number(*f),
        Json::Number(n) => n.clone(),
        Json::Str(s) => json_quote(s),
        Json::Arr(a) => {
            let parts: Vec<String> = a.iter().map(json_marshal).collect();
            format!("[{}]", parts.join(","))
        }
        Json::Obj(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", json_quote(k), json_marshal(&m[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// A decoded value printed as text, as `Value::display_string` prints a cell.
fn json_display_string(v: &Json) -> String {
    match v {
        Json::Null => "<nil>".to_string(),
        Json::Bool(b) => b.to_string(),
        Json::Num(f) => format_float(*f),
        Json::Number(n) => n.clone(),
        Json::Str(s) => s.clone(),
        Json::Arr(a) => {
            let parts: Vec<String> = a.iter().map(json_display_string).collect();
            format!("[{}]", parts.join(" "))
        }
        Json::Obj(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{}:{}", k, json_display_string(&m[*k])))
                .collect();
            format!("map[{}]", parts.join(" "))
        }
    }
}

/// Backs the jsonb `?` operator: does key exist as a top-level object key, or
/// (for an array) as a string element?
pub(crate) fn jsonb_exists(j: &str, key: &str) -> bool {
    let Some(v) = json_unmarshal(j) else {
        return false;
    };
    match v {
        Json::Obj(t) => t.contains_key(key),
        Json::Arr(t) => {
            for e in &t {
                if let Json::Str(s) = e
                    && s == key
                {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// jsonb_exists_any / jsonb_exists_all back `?|` / `?&`; keys arrives as a JSON
/// array.
pub(crate) fn jsonb_exists_any(j: &str, keys_json: &str) -> bool {
    for k in json_str_array(keys_json) {
        if jsonb_exists(j, &k) {
            return true;
        }
    }
    false
}

pub(crate) fn jsonb_exists_all(j: &str, keys_json: &str) -> bool {
    let keys = json_str_array(keys_json);
    for k in &keys {
        if !jsonb_exists(j, k) {
            return false;
        }
    }
    true
}

/// The keys of `?|` and `?&`: a JSON array, or Postgres' text form of an array, which an
/// untyped literal (`'{a,b}'`) or a parameter is in.
fn json_str_array(s: &str) -> Vec<String> {
    let s = match crate::builtins::arrays::array_as_json(&overlite_core::Value::Text(s.to_string())) {
        overlite_core::Value::Text(json) => json,
        _ => s.to_string(),
    };
    match json_unmarshal(&s) {
        Some(Json::Arr(arr)) => arr.iter().map(json_display_string).collect(),
        _ => Vec::new(),
    }
}

/// Postgres' jsonb @> jsonb (does a contain b?). Both arguments are JSON text;
/// it backs the json_contains() scalar the dialect layer emits for the @> / <@
/// operators. Invalid JSON is treated as non-containing.
pub(crate) fn jsonb_contains(a: &str, b: &str) -> bool {
    let Some(av) = json_unmarshal(a) else {
        return false;
    };
    let Some(bv) = json_unmarshal(b) else {
        return false;
    };
    json_contains(&av, &bv)
}

/// Whether a contains b, matching Postgres jsonb semantics: objects match on
/// keys (recursively), arrays require every b-element to be contained in some
/// a-element, and a scalar is contained in an array holding it.
fn json_contains(a: &Json, b: &Json) -> bool {
    match b {
        Json::Obj(bv) => {
            let Json::Obj(am) = a else {
                return false;
            };
            for (k, v) in bv {
                match am.get(k) {
                    Some(av) if json_contains(av, v) => {}
                    _ => return false,
                }
            }
            true
        }
        Json::Arr(bv) => {
            let Json::Arr(aa) = a else {
                return false;
            };
            for be in bv {
                if !any_contains(aa, be) {
                    return false;
                }
            }
            true
        }
        _ => {
            // Scalar.
            if let Json::Arr(aa) = a {
                return any_contains(aa, b);
            }
            a == b
        }
    }
}

fn any_contains(arr: &[Json], b: &Json) -> bool {
    for e in arr {
        if json_contains(e, b) {
            return true;
        }
    }
    false
}

/// The whole text as one JSON value with numbers as written, None for what is no JSON.
fn jsonb_value(s: &str) -> Option<Json> {
    let mut p = JsonParser::new(s);
    p.use_number = true;
    let v = p.value()?;
    p.finish()?;
    Some(v)
}

/// Where a kind of value sorts in jsonb's order: Object > Array > Boolean > Number >
/// String > Null.
fn jsonb_rank(v: &Json) -> u8 {
    match v {
        Json::Null => 0,
        Json::Str(_) => 1,
        Json::Num(_) | Json::Number(_) => 2,
        Json::Bool(_) => 3,
        Json::Arr(_) => 4,
        Json::Obj(_) => 5,
    }
}

/// An object's keys in jsonb's storage order: shorter first, then by their bytes.
fn jsonb_keys(m: &HashMap<String, Json>) -> Vec<&String> {
    let mut keys: Vec<&String> = m.keys().collect();
    keys.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes())));
    keys
}

fn jsonb_number(v: &Json) -> String {
    match v {
        Json::Number(n) => n.clone(),
        Json::Num(f) => json_number(*f),
        _ => String::new(),
    }
}

/// Two decoded values in jsonb's order: by kind, then numbers by value, strings by
/// their bytes, arrays and objects by their size and then element by element (an
/// object's pairs in storage order, key then value).
fn jsonb_order(a: &Json, b: &Json) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = jsonb_rank(a).cmp(&jsonb_rank(b));
    if rank != Ordering::Equal {
        return rank;
    }
    match (a, b) {
        (Json::Str(x), Json::Str(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Json::Bool(x), Json::Bool(y)) => x.cmp(y),
        (Json::Arr(x), Json::Arr(y)) => x.len().cmp(&y.len()).then_with(|| {
            x.iter()
                .zip(y)
                .map(|(p, q)| jsonb_order(p, q))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        }),
        (Json::Obj(x), Json::Obj(y)) => x.len().cmp(&y.len()).then_with(|| {
            for (kx, ky) in jsonb_keys(x).into_iter().zip(jsonb_keys(y)) {
                let o = kx
                    .len()
                    .cmp(&ky.len())
                    .then_with(|| kx.as_bytes().cmp(ky.as_bytes()))
                    .then_with(|| jsonb_order(&x[kx], &y[ky]));
                if o.is_ne() {
                    return o;
                }
            }
            Ordering::Equal
        }),
        (Json::Null, Json::Null) => Ordering::Equal,
        _ => crate::builtins::decimal::dec_cmp_str(&jsonb_number(a), &jsonb_number(b)).cmp(&0),
    }
}

/// The JSONB collation: two texts compared as the jsonb values they hold, as Postgres
/// compares jsonb, so that `{"a": 1}` equals `{"a":1}` and 14 is past 2. A text that is
/// no JSON compares by its bytes.
pub(crate) fn jsonb_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    match (jsonb_value(a), jsonb_value(b)) {
        (Some(x), Some(y)) => jsonb_order(&x, &y),
        _ => a.as_bytes().cmp(b.as_bytes()),
    }
}

/// A value as `->>` and `#>>` give it: a string's text, a number as written, true or
/// false, NULL for null, and JSON for the rest.
fn jsonb_text(v: Json) -> Option<String> {
    match v {
        Json::Null => None,
        Json::Str(s) => Some(s),
        Json::Bool(b) => Some(b.to_string()),
        other => Some(json_marshal(&other)),
    }
}

/// One step of a path into v: a key of an object, or an index of an array, from its end
/// when negative.
fn jsonb_step(v: Json, key: &str, index_only: bool) -> Option<Json> {
    match v {
        Json::Obj(mut m) if !index_only => m.remove(key),
        Json::Arr(mut a) => {
            let i: i64 = key.trim().parse().ok()?;
            let len = a.len() as i64;
            let i = if i < 0 { len + i } else { i };
            (0..len).contains(&i).then(|| a.swap_remove(i as usize))
        }
        _ => None,
    }
}

fn malformed_json(s: &str) -> crate::error::Error {
    crate::error::Error::with_state(
        overlite_core::SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type json: \"{s}\""),
    )
}

/// `json ->> key` as Postgres has it: always text. A text key reads an object's member,
/// an integer an array's element.
///
/// # Errors
///
/// 22P02 for a value that is no JSON.
pub(crate) fn jsonb_arrow_text(
    json: &overlite_core::Value,
    key: &overlite_core::Value,
) -> crate::error::Result<overlite_core::Value> {
    use overlite_core::Value;
    if json.is_null() || key.is_null() {
        return Ok(Value::Null);
    }
    let text = json.to_string();
    let v = jsonb_value(&text).ok_or_else(|| malformed_json(&text))?;
    let step = match key {
        Value::Int(i) => jsonb_step(v, &i.to_string(), true),
        other => jsonb_step(v, &other.to_string(), false),
    };
    Ok(step.and_then(jsonb_text).map_or(Value::Null, Value::Text))
}

/// `json #> path` and `json #>> path`: the value at a path given as a text array
/// (`{a,1,b}`), each element a key of an object or an index of an array; JSON, or text
/// as `->>` gives it.
///
/// # Errors
///
/// 22P02 for a value that is no JSON, or a path that is no array.
pub(crate) fn jsonb_path(
    json: &overlite_core::Value,
    path: &overlite_core::Value,
    as_text: bool,
) -> crate::error::Result<overlite_core::Value> {
    use overlite_core::Value;
    if json.is_null() || path.is_null() {
        return Ok(Value::Null);
    }
    let text = json.to_string();
    let mut v = jsonb_value(&text).ok_or_else(|| malformed_json(&text))?;
    let keys = match crate::builtins::arrays::array_as_json(path) {
        Value::Text(arr) => match json_unmarshal(&arr) {
            Some(Json::Arr(items)) => items,
            _ => Vec::new(),
        },
        _ => {
            return Err(crate::error::Error::with_state(
                overlite_core::SqlState::INVALID_TEXT_REPRESENTATION,
                format!("malformed array literal: \"{path}\""),
            ));
        }
    };
    for k in keys {
        let key = match k {
            Json::Str(s) => s,
            Json::Null => return Ok(Value::Null),
            other => json_marshal(&other),
        };
        match jsonb_step(v, &key, false) {
            Some(next) => v = next,
            None => return Ok(Value::Null),
        }
    }
    Ok(if as_text {
        jsonb_text(v).map_or(Value::Null, Value::Text)
    } else {
        Value::Text(json_marshal(&v))
    })
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    use overlite_core::text::text_to_bytes;

    #[test]
    fn a_lone_surrogate_is_a_replacement_character() {
        assert_eq!(
            json_decode_first(r#""\ud800x""#),
            Some(Json::Str("\u{fffd}x".to_string()))
        );
        assert_eq!(json_decode_first(r#""😀""#), Some(Json::Str("\u{1f600}".to_string())));
    }

    #[test]
    fn nesting_stops_at_ten_thousand() {
        let nest = |d: usize| "[".repeat(d) + &"]".repeat(d);
        // A connection's thread has 8 MiB, as server.rs gives it.
        let check = move || {
            assert!(json_decode_first(&nest(10000)).is_some());
            assert!(json_decode_first(&nest(10001)).is_none());
        };
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(check)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn use_number_keeps_the_literal_and_reads_one_value() {
        assert_eq!(
            json_decode_first("[1E+2] trailing"),
            Some(Json::Arr(vec![Json::Number("1E+2".to_string())]))
        );
    }

    #[test]
    fn a_plane_16_escape_stays_that_character() {
        let esc = format!("\"{0}udbff{0}udf80\"", char::from(92));
        let Some(Json::Str(s)) = json_decode_first(&esc) else {
            panic!()
        };
        assert_eq!(&*text_to_bytes(&s), "\u{10ff80}".as_bytes());
    }
}
