//! The canonical form of an array and of an hstore, as a write stores them (ARCHITECTURE.md,
//! "Storage format"): a JSON array whose elements are each in their type's form, and a
//! JSON object. Postgres' text form of either is read as its input function reads it,
//! and what that refuses is refused with its code.

use crate::builtins::casts::{CastType, assign, assignment_type, cast};
use crate::builtins::hstore::hstore_in;
use crate::builtins::json::{Json, json_decode_first, json_is_valid, json_number, json_quote};
use crate::error::{Error, Result};
use overlite_core::{SqlState, Value};

fn malformed(text: &str) -> Error {
    Error::with_state(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed array literal: \"{text}\""),
    )
}

/// An element as Postgres' array input reads it: its text, or NULL.
#[derive(Debug, PartialEq)]
enum Item {
    Text(String),
    Null,
    Nested(Vec<Item>),
}

/// Reads Postgres' text form of an array, `{a,"b c",NULL,{1,2}}`, as array_in does.
fn parse_text_array(text: &str) -> Result<Item> {
    let t = text.trim();
    // A dimension decoration, `[1:2]={a,b}`, says where the array starts: the elements
    // are what count here.
    let body = match t.strip_prefix('[') {
        Some(_) => t
            .split_once('=')
            .map(|(_, b)| b.trim())
            .ok_or_else(|| malformed(text))?,
        None => t,
    };
    let b = body.as_bytes();
    if b.first() != Some(&b'{') {
        return Err(malformed(text));
    }
    let (item, end) = parse_level(body, 0).ok_or_else(|| malformed(text))?;
    if !body[end..].trim().is_empty() {
        return Err(malformed(text));
    }
    Ok(item)
}

/// The array opening at i, and where it ends.
fn parse_level(s: &str, i: usize) -> Option<(Item, usize)> {
    let b = s.as_bytes();
    let mut items = Vec::new();
    let mut i = i + 1;
    let skip_ws = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    i = skip_ws(i);
    if b.get(i) == Some(&b'}') {
        return Some((Item::Nested(items), i + 1));
    }
    loop {
        i = skip_ws(i);
        match b.get(i)? {
            b'{' => {
                let (inner, end) = parse_level(s, i)?;
                items.push(inner);
                i = end;
            }
            b'"' => {
                let mut out = String::new();
                i += 1;
                loop {
                    match b.get(i)? {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' => {
                            let c = s[i + 1..].chars().next()?;
                            out.push(c);
                            i += 1 + c.len_utf8();
                        }
                        _ => {
                            let c = s[i..].chars().next()?;
                            out.push(c);
                            i += c.len_utf8();
                        }
                    }
                }
                items.push(Item::Text(out));
            }
            b',' | b'}' => return None,
            _ => {
                let mut out = String::new();
                while let Some(&c) = b.get(i) {
                    match c {
                        b',' | b'}' => break,
                        b'{' | b'"' => return None,
                        b'\\' => {
                            let c = s[i + 1..].chars().next()?;
                            out.push(c);
                            i += 1 + c.len_utf8();
                        }
                        _ => {
                            let c = s[i..].chars().next()?;
                            out.push(c);
                            i += c.len_utf8();
                        }
                    }
                }
                let word = out.trim_end();
                if word.eq_ignore_ascii_case("null") {
                    items.push(Item::Null);
                } else {
                    items.push(Item::Text(word.to_string()));
                }
            }
        }
        i = skip_ws(i);
        match b.get(i)? {
            b',' => i += 1,
            b'}' => return Some((Item::Nested(items), i + 1)),
            _ => return None,
        }
    }
}

/// Whether an element type's values are JSON numbers in a stored array.
fn numeric_element(element: &str) -> bool {
    matches!(
        CastType::parse(element),
        Some(
            CastType::Int2
                | CastType::Int4
                | CastType::Int8
                | CastType::Float4
                | CastType::Float8
                | CastType::Numeric(_)
        )
    )
}

fn boolean_element(element: &str) -> bool {
    matches!(element, "boolean" | "bool")
}

/// One element, given as text, in its stored JSON form.
fn element_json(text: &str, element: &str) -> Result<String> {
    if boolean_element(element) {
        return match overlite_core::parse_bool(text) {
            Some(b) => Ok(b.to_string()),
            None => Err(Error::with_state(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("invalid input syntax for type boolean: \"{text}\""),
            )),
        };
    }
    let value = Value::Text(text.to_string());
    let fitted = match (assignment_type(element), CastType::parse(element)) {
        (Some(decl), _) if !decl.ends_with("[]") => assign(&value, &decl)?,
        (_, Some(ty)) => cast(&value, &ty)?,
        _ => value,
    };
    Ok(match fitted {
        Value::Null => "null".to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) if f.is_finite() => json_number(f),
        Value::Float(f) => json_quote(&f.to_string()),
        Value::Bool(b) => b.to_string(),
        Value::Text(s) if numeric_element(element) && !s.chars().any(char::is_alphabetic) => s,
        Value::Text(s) => json_quote(&s),
        Value::Blob(b) => json_quote(&format!("\\x{}", hex::encode(b))),
        other @ Value::Time(_) => json_quote(&other.to_string()),
    })
}

fn item_json(item: &Item, element: &str) -> Result<String> {
    match item {
        Item::Null => Ok("null".to_string()),
        Item::Text(t) => element_json(t, element),
        Item::Nested(items) => {
            let parts: Result<Vec<String>> = items.iter().map(|i| item_json(i, element)).collect();
            Ok(["[", &parts?.join(","), "]"].concat())
        }
    }
}

/// A decoded JSON element in its stored form.
fn json_element(j: &Json, element: &str) -> Result<String> {
    match j {
        Json::Null => Ok("null".to_string()),
        Json::Arr(items) => {
            let parts: Result<Vec<String>> = items.iter().map(|i| json_element(i, element)).collect();
            Ok(["[", &parts?.join(","), "]"].concat())
        }
        Json::Str(s) => element_json(s, element),
        Json::Number(n) => element_json(n, element),
        Json::Num(f) => element_json(&json_number(*f), element),
        Json::Bool(b) if boolean_element(element) => Ok(b.to_string()),
        Json::Bool(b) => element_json(&b.to_string(), element),
        Json::Obj(_) => Ok(crate::builtins::json::json_marshal(j)),
    }
}

/// An array as it is stored: a JSON array of elements in their type's form. Text in
/// Postgres' array form is read as array_in reads it; JSON, as this server writes an
/// ARRAY[...], is checked and its elements fitted.
///
/// # Errors
///
/// 22P02 for what is no array, and an element's own error for an element its type
/// does not read.
pub(crate) fn canonical_array(v: &Value, element: &str) -> Result<Value> {
    let element = element.trim().trim_matches('"');
    let Value::Text(text) = v else {
        return Ok(v.clone());
    };
    let t = text.trim();
    if t.starts_with('[') && json_is_valid(t) {
        let Some(json @ Json::Arr(_)) = json_decode_first(t) else {
            return Err(malformed(text));
        };
        return json_element(&json, element).map(Value::Text);
    }
    let item = parse_text_array(text)?;
    item_json(&item, element).map(Value::Text)
}

fn hstore_error(text: &str, at: usize) -> Error {
    match text[at..].chars().next() {
        None => Error::with_state(
            SqlState::SYNTAX_ERROR,
            "syntax error in hstore: unexpected end of string",
        ),
        Some(c) => Error::with_state(
            SqlState::SYNTAX_ERROR,
            format!("syntax error in hstore, near \"{c}\" at position {at}"),
        ),
    }
}

/// Past a key or a value of hstore text at i, quoted or bare.
fn hstore_word(text: &str, mut i: usize) -> Result<usize> {
    let b = text.as_bytes();
    if b.get(i) == Some(&b'"') {
        i += 1;
        loop {
            match b.get(i) {
                None => return Err(hstore_error(text, i)),
                Some(b'"') => return Ok(i + 1),
                Some(b'\\') => i += 2,
                Some(_) => i += 1,
            }
        }
    }
    let start = i;
    while let Some(&c) = b.get(i) {
        if c.is_ascii_whitespace() || c == b',' || c == b'=' || c == b'"' {
            break;
        }
        if c == b'\\' {
            i += 1;
        }
        i += 1;
    }
    if i == start {
        return Err(hstore_error(text, i.min(text.len())));
    }
    Ok(i.min(text.len()))
}

/// An hstore as it is stored: a JSON object. Its text form is read as hstore_in reads
/// it; a JSON object, as stored, stays.
///
/// # Errors
///
/// 42601 for text hstore_in does not read, as Postgres reports it.
pub(crate) fn canonical_hstore(v: &Value) -> Result<Value> {
    let Value::Text(text) = v else {
        return Ok(v.clone());
    };
    let t = text.trim();
    if t.starts_with('{') && json_is_valid(t) {
        return Ok(v.clone());
    }
    let b = text.as_bytes();
    let ws = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut i = ws(0);
    while i < b.len() {
        i = ws(hstore_word(text, i)?);
        if !text[i..].starts_with("=>") {
            return Err(hstore_error(text, i));
        }
        i = ws(i + 2);
        i = ws(hstore_word(text, i)?);
        match b.get(i) {
            None => break,
            Some(b',') => i = ws(i + 1),
            Some(_) => return Err(hstore_error(text, i)),
        }
    }
    Ok(Value::Text(hstore_in(text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(text: &str, element: &str) -> Result<String> {
        match canonical_array(&Value::Text(text.to_string()), element)? {
            Value::Text(s) => Ok(s),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arrays() {
        assert_eq!(arr("{1,2}", "integer").unwrap(), "[1,2]");
        assert_eq!(
            arr(r#"{a,"b c",NULL,"NULL"}"#, "text").unwrap(),
            r#"["a","b c",null,"NULL"]"#
        );
        assert_eq!(arr("{{1,2},{3,4}}", "int").unwrap(), "[[1,2],[3,4]]");
        assert_eq!(arr("{}", "text").unwrap(), "[]");
        assert_eq!(arr("{t,FALSE}", "boolean").unwrap(), "[true,false]");
        assert_eq!(
            arr("{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}", "uuid").unwrap(),
            r#"["a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"]"#
        );
        assert_eq!(arr(r#"["1",2]"#, "integer").unwrap(), "[1,2]");
        assert_eq!(arr("[1,2]", "text").unwrap(), r#"["1","2"]"#);
        assert_eq!(arr("{1.555}", "numeric(10,2)").unwrap(), "[1.56]");
        assert_eq!(arr(r#"{"a\"b"}"#, "text").unwrap(), r#"["a\"b"]"#);
        for (bad, code) in [("{1,", "22P02"), ("abc", "22P02"), ("{a}", "22P02"), ("{1}}", "22P02")] {
            let e = arr(bad, "integer").unwrap_err();
            assert_eq!(e.sql_state().as_str(), code, "{bad}");
        }
        assert_eq!(
            arr("{1,", "integer").unwrap_err().message(),
            "malformed array literal: \"{1,\""
        );
    }

    #[test]
    fn hstores() {
        let h = |s: &str| canonical_hstore(&Value::Text(s.to_string()));
        assert_eq!(
            h(r#""a"=>"1", b=>NULL"#).unwrap(),
            Value::Text(r#"{"a":"1","b":null}"#.into())
        );
        assert_eq!(h("").unwrap(), Value::Text("{}".into()));
        assert_eq!(
            h("a=>").unwrap_err().message(),
            "syntax error in hstore: unexpected end of string"
        );
        assert_eq!(h("a=>b,").unwrap(), Value::Text(r#"{"a":"b"}"#.into()));
        assert_eq!(
            h("a b").unwrap_err().message(),
            "syntax error in hstore, near \"b\" at position 2"
        );
    }
}
