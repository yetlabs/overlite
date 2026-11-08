//! Postgres arrays over SQLite: a `type[]` column keeps its declared type
//! verbatim, and the value is a JSON array in a TEXT cell. On the wire we
//! advertise the proper array OID and render the JSON as Postgres' `{…}` text;
//! on input, ARRAY[…] and '{…}'::type[] become JSON.

use crate::ddl::sequences::{is_ident_part, read_paren_args};
use crate::rewrite::casts::{operand_before, read_type_name};
use crate::rewrite::catalog::index_word_call;
use crate::rewrite::strings::end_of_string_literal;
use crate::rewrite::{splice, utf8_string};
use crate::session::utility::first_word_upper;
use crate::types::{
    OID_BOOL, OID_BYTEA, OID_DATE, OID_FLOAT8, OID_INT2, OID_INT4, OID_INT8, OID_JSON, OID_JSONB, OID_NUMERIC,
    OID_TEXT, OID_TIMESTAMP, OID_UUID, OID_VARCHAR, encode_binary, oid_from_decl_type,
};
use overlite_core::text::{eq_fold, text_from_bytes, text_to_bytes};
use overlite_core::{Oid, Value, format_float};
use overlite_engine::{Json, assign_value, code_only, json_decode_first, json_marshal};
use std::collections::HashMap;
use std::sync::LazyLock;

// Array type OIDs (element OID -> array OID).
const OID_BOOL_ARR: Oid = Oid::new(1000);
const OID_BYTEA_ARR: Oid = Oid::new(1001);
pub(crate) const OID_INT2_ARR: Oid = Oid::new(1005);
const OID_INT4_ARR: Oid = Oid::new(1007);
pub(crate) const OID_TEXT_ARR: Oid = Oid::new(1009);
const OID_VARCHAR_ARR: Oid = Oid::new(1015);
pub(crate) const OID_INT8_ARR: Oid = Oid::new(1016);
pub(crate) const OID_FLOAT8_ARR: Oid = Oid::new(1022);
const OID_DATE_ARR: Oid = Oid::new(1182);
const OID_TIMESTP_ARR: Oid = Oid::new(1115);
const OID_NUMERIC_ARR: Oid = Oid::new(1231);
pub(crate) const OID_UUID_ARR: Oid = Oid::new(2951);
const OID_JSON_ARR: Oid = Oid::new(199);
const OID_JSONB_ARR: Oid = Oid::new(3807);

static ARRAY_OIDS: LazyLock<HashMap<Oid, bool>> = LazyLock::new(|| {
    HashMap::from([
        (OID_BOOL_ARR, true),
        (OID_BYTEA_ARR, true),
        (OID_INT2_ARR, true),
        (OID_INT4_ARR, true),
        (OID_TEXT_ARR, true),
        (OID_VARCHAR_ARR, true),
        (OID_INT8_ARR, true),
        (OID_FLOAT8_ARR, true),
        (OID_DATE_ARR, true),
        (OID_TIMESTP_ARR, true),
        (OID_NUMERIC_ARR, true),
        (OID_UUID_ARR, true),
        (OID_JSON_ARR, true),
        (OID_JSONB_ARR, true),
    ])
});

pub(crate) fn is_array_oid(oid: Oid) -> bool {
    ARRAY_OIDS.get(&oid).copied().unwrap_or(false)
}

/// Reports whether a declared type is a Postgres array (`type[]`).
pub(crate) fn is_array_decl(decl: &str) -> bool {
    decl.trim().ends_with("[]")
}

/// Maps a `type[]` declared type to its array OID via the element type's scalar
/// OID.
pub(crate) fn array_oid_for_decl(decl: &str) -> Oid {
    let d = decl.trim();
    let elem = d.strip_suffix("[]").unwrap_or(d).trim();
    let oid = oid_from_decl_type(elem);
    if oid == OID_INT8 {
        OID_INT8_ARR
    } else if oid == OID_BOOL {
        OID_BOOL_ARR
    } else if oid == OID_FLOAT8 {
        OID_FLOAT8_ARR
    } else if oid == OID_NUMERIC {
        OID_NUMERIC_ARR
    } else if oid == OID_UUID {
        OID_UUID_ARR
    } else if oid == OID_DATE {
        OID_DATE_ARR
    } else if oid == OID_TIMESTAMP {
        OID_TIMESTP_ARR
    } else if oid == OID_JSONB {
        OID_JSONB_ARR
    } else if oid == OID_BYTEA {
        OID_BYTEA_ARR
    } else {
        OID_TEXT_ARR
    }
}

/// Renders a stored JSON array as Postgres' `{…}` array text. A value that isn't
/// a JSON array is passed through unchanged. Only the first JSON value of the
/// text is read, and a JSON null decodes to no elements.
pub(crate) fn json_array_to_pg_text(v: &Value) -> Vec<u8> {
    // Text and bytes as they are, anything else empty.
    let s = match v {
        Value::Text(x) => std::borrow::Cow::Borrowed(x.as_str()),
        Value::Blob(x) => text_from_bytes(x),
        _ => std::borrow::Cow::Borrowed(""),
    };
    match json_decode_first(&s) {
        Some(Json::Arr(arr)) => pg_array_text(&arr).into_bytes(),
        Some(Json::Null) => b"{}".to_vec(),
        _ => text_to_bytes(&s).into_owned(),
    }
}

/// The element type of an array type.
fn array_elem_oid(oid: Oid) -> Oid {
    match oid {
        OID_BOOL_ARR => OID_BOOL,
        OID_BYTEA_ARR => OID_BYTEA,
        OID_INT2_ARR => OID_INT2,
        OID_INT4_ARR => OID_INT4,
        OID_VARCHAR_ARR => OID_VARCHAR,
        OID_INT8_ARR => OID_INT8,
        OID_FLOAT8_ARR => OID_FLOAT8,
        OID_DATE_ARR => OID_DATE,
        OID_TIMESTP_ARR => OID_TIMESTAMP,
        OID_NUMERIC_ARR => OID_NUMERIC,
        OID_UUID_ARR => OID_UUID,
        OID_JSON_ARR => OID_JSON,
        OID_JSONB_ARR => OID_JSONB,
        _ => OID_TEXT,
    }
}

/// array_send of a stored array: the number of dimensions, whether any element is
/// NULL, the element type, then each dimension's size and lower bound, then the
/// elements in row order, each with its length, -1 for NULL. None for a value that
/// is no array or not a rectangular one.
pub(crate) fn array_binary(oid: Oid, v: &Value) -> Option<Vec<u8>> {
    let s = match v {
        Value::Text(x) => std::borrow::Cow::Borrowed(x.as_str()),
        Value::Blob(x) => text_from_bytes(x),
        _ => return None,
    };
    let arr = match json_decode_first(&s) {
        Some(Json::Arr(arr)) => arr,
        Some(Json::Null) => Vec::new(),
        _ => parse_pg_array(&s)?, // stored as a client bound it
    };
    let mut dims = Vec::new();
    let mut level = arr.first();
    let mut size = arr.len();
    loop {
        dims.push(size);
        match level {
            Some(Json::Arr(inner)) => {
                size = inner.len();
                level = inner.first();
            }
            _ => break,
        }
    }
    let mut leaves = Vec::new();
    flatten_array(&arr, &dims, &mut leaves)?;
    if leaves.is_empty() {
        dims.clear();
    }

    let elem = array_elem_oid(oid);
    let mut out = Vec::with_capacity(12 + dims.len() * 8 + leaves.len() * 8);
    out.extend_from_slice(&(dims.len() as u32).to_be_bytes());
    let has_null = leaves.iter().any(|e| matches!(e, Json::Null));
    out.extend_from_slice(&(has_null as u32).to_be_bytes());
    out.extend_from_slice(&elem.get().to_be_bytes());
    for d in &dims {
        out.extend_from_slice(&(*d as u32).to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
    }
    for e in leaves {
        let Some(value) = array_elem_value(e) else {
            out.extend_from_slice(&(-1i32).to_be_bytes());
            continue;
        };
        let bytes = encode_binary(elem, &value)?;
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(&bytes);
    }
    Some(out)
}

/// Collects the elements in row order; None when a sub-array is not the size of
/// its dimension.
fn flatten_array<'a>(arr: &'a [Json], dims: &[usize], out: &mut Vec<&'a Json>) -> Option<()> {
    if arr.len() != dims[0] {
        return None;
    }
    for e in arr {
        match e {
            Json::Arr(inner) => flatten_array(inner, dims.get(1..).filter(|d| !d.is_empty())?, out)?,
            _ if dims.len() > 1 => return None,
            _ => out.push(e),
        }
    }
    Some(())
}

/// An element as the Value of its text, which encode_binary reads by the type.
fn array_elem_value(e: &Json) -> Option<Value> {
    Some(match e {
        Json::Null => return None,
        Json::Bool(x) => Value::Bool(*x),
        Json::Number(x) => Value::Text(x.clone()),
        Json::Num(x) => Value::Text(format_float(*x)),
        Json::Str(x) => Value::Text(x.clone()),
        other => Value::Text(json_marshal(other)),
    })
}

/// Reads Postgres' `{...}` array text, the form an array bound as text is stored in.
fn parse_pg_array(s: &str) -> Option<Vec<Json>> {
    let chars: Vec<char> = s.trim().chars().collect();
    let (arr, end) = parse_pg_array_at(&chars, 0)?;
    (end == chars.len()).then_some(arr)
}

/// The array opening at i, and the position after it.
fn parse_pg_array_at(c: &[char], mut i: usize) -> Option<(Vec<Json>, usize)> {
    if c.get(i) != Some(&'{') {
        return None;
    }
    i += 1;
    let mut out = Vec::new();
    if c.get(i) == Some(&'}') {
        return Some((out, i + 1));
    }
    loop {
        match c.get(i)? {
            '{' => {
                let (inner, end) = parse_pg_array_at(c, i)?;
                out.push(Json::Arr(inner));
                i = end;
            }
            '"' => {
                let mut v = String::new();
                i += 1;
                while *c.get(i)? != '"' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    v.push(*c.get(i)?);
                    i += 1;
                }
                out.push(Json::Str(v));
                i += 1;
            }
            _ => {
                let start = i;
                while !matches!(c.get(i)?, ',' | '}') {
                    i += 1;
                }
                let v: String = c[start..i].iter().collect();
                let v = v.trim();
                out.push(if eq_fold(v, "null") {
                    Json::Null
                } else {
                    Json::Str(v.to_string())
                });
            }
        }
        match c.get(i)? {
            ',' => i += 1,
            '}' => return Some((out, i + 1)),
            _ => return None,
        }
    }
}

fn pg_array_text(arr: &[Json]) -> String {
    let parts: Vec<String> = arr.iter().map(pg_array_elem).collect();
    ["{", &parts.join(","), "}"].concat()
}

fn pg_array_elem(e: &Json) -> String {
    match e {
        Json::Null => "NULL".to_string(),
        Json::Bool(x) => {
            if *x {
                "t".to_string()
            } else {
                "f".to_string()
            }
        }
        Json::Number(x) => x.clone(),
        Json::Num(x) => format_float(*x),
        Json::Str(x) => quote_pg_array_elem(x),
        Json::Arr(x) => pg_array_text(x),
        Json::Obj(_) => quote_pg_array_elem(""),
    }
}

/// Double-quotes an element when it would otherwise be ambiguous (empty, contains
/// a delimiter/quote/space, or looks like NULL).
fn quote_pg_array_elem(s: &str) -> String {
    if s.is_empty() || eq_fold(s, "null") || s.contains(['{', '}', ',', '"', '\\', ' ']) {
        let s = s.replace('\\', "\\\\").replace('"', "\\\"");
        return ["\"", &s, "\""].concat();
    }
    s.to_string()
}

// --- input rewrites ---------------------------------------------------------

/// Turns ARRAY[…] constructors into json_array(…), so the stored value is a JSON
/// array. Nested ARRAY[…] are handled by the same pass.
pub(crate) fn rewrite_array_literal(sql: &str) -> String {
    if !sql.to_lowercase().contains("array") {
        return sql.to_string();
    }
    let low = sql.to_ascii_lowercase();
    let s = sql.as_bytes();
    let mut b: Vec<u8> = Vec::with_capacity(s.len() + 32);
    let mut stack: Vec<bool> = Vec::new(); // true = an ARRAY[ bracket (emit ")" on close)
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' {
            let j = end_of_string_literal(sql, i);
            b.extend_from_slice(&s[i..j]);
            i = j;
            continue;
        }
        if (c | 0x20) == b'a' && match_word_at(&low, i, "array") {
            let mut k = i + "array".len();
            while k < s.len() && (s[k] == b' ' || s[k] == b'\t' || s[k] == b'\n') {
                k += 1;
            }
            if k < s.len() && s[k] == b'[' {
                b.extend_from_slice(b"json_array(");
                stack.push(true);
                i = k + 1;
                continue;
            }
        }
        match c {
            b'[' => {
                stack.push(false);
                b.push(b'[');
            }
            b']' => {
                if let Some(arr) = stack.pop()
                    && arr
                {
                    b.push(b')');
                    i += 1;
                    continue;
                }
                b.push(b']');
            }
            _ => b.push(c),
        }
        i += 1;
    }
    utf8_string(b)
}

/// The array type of a cast starting at ts, past `::` and its spaces: the element
/// type as `assign` reads it, lower case, with its modifier (`timestamp with time
/// zone`, `double precision`, `varchar(20)`), and the index past the brackets. None
/// for a type that is no array.
fn array_cast_type(sql: &str, ts: usize) -> Option<(String, usize)> {
    let (name, mut m) = read_type_name(sql, ts)?;
    let b = sql.as_bytes();
    let mut brackets = 0;
    // `[]`, and the size Postgres takes and ignores, `[2]`.
    while b.get(m) == Some(&b'[') {
        let close = m + sql[m..].find(']')?;
        if !sql[m + 1..close].bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        m = close + 1;
        brackets += 1;
    }
    if brackets == 0 || name.is_empty() {
        return None;
    }
    let elem = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    Some((elem, m))
}

/// The text of a string literal, `'it''s'` read as `it's`.
fn literal_text(lit: &str) -> String {
    lit[1..lit.len() - 1].replace("''", "'")
}

/// A cast of a literal to an array: the literal read as array_in reads it, each element
/// by its type, so that a quoted element keeps its commas and backslashes (an E''
/// string of jsonb). In a statement that runs, a call: it refuses what the type does not
/// read with its code, and tells the result's type. In DDL, where a call has no place
/// (a DEFAULT), the JSON array it gives.
fn array_literal_cast(lit: &str, elem: &str, ddl: bool) -> String {
    let decl = [elem, "[]"].concat();
    if ddl && let Ok(Value::Text(json)) = assign_value(&Value::Text(literal_text(lit)), &decl) {
        return ["'", &json.replace('\'', "''"), "'"].concat();
    }
    ["overlite_assign(", lit, ", '", &decl.replace('\'', "''"), "')"].concat()
}

/// Turns '{…}'::type[] literals, and a literal in parentheses before one, into the array
/// they are: see array_literal_cast.
pub(crate) fn rewrite_array_cast(sql: &str) -> String {
    if !sql.contains("::") {
        return sql.to_string();
    }
    let ddl = ddl_statement(sql);
    let s = sql.as_bytes();
    let skip_ws = |mut k: usize| {
        while k < s.len() && matches!(s[k], b' ' | b'\t' | b'\n' | b'\r') {
            k += 1;
        }
        k
    };
    let mut b: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c != b'\'' {
            b.push(c);
            i += 1;
            continue;
        }
        let j = end_of_string_literal(sql, i);
        let lit = &sql[i..j];
        // ('{…}')::type[], as Django writes each array of a bulk insert.
        let mut k = skip_ws(j);
        let mut paren = false;
        if s.get(k) == Some(&b')') && b.iter().rev().find(|c| !c.is_ascii_whitespace()) == Some(&b'(') {
            paren = true;
            k = skip_ws(k + 1);
        }
        if lit.len() >= 2
            && s.get(k) == Some(&b':')
            && s.get(k + 1) == Some(&b':')
            && let Some((elem, m)) = array_cast_type(sql, skip_ws(k + 2))
        {
            b.extend_from_slice(array_literal_cast(lit, &elem, ddl).as_bytes());
            if paren {
                b.push(b')');
            }
            i = m;
            continue;
        }
        b.extend_from_slice(lit.as_bytes());
        i = j;
    }
    utf8_string(b)
}

/// An `::type[]` cast of what is not a literal. Of an expression in parentheses or a
/// parameter, a call that reads the array and makes each element its type, so that
/// `(a)::int[]` rounds a float's elements and a parameter in Postgres' text form is
/// read. Of anything else, a column or a call, nothing: the value is already a JSON
/// array, or a catalog's vector that unnest reads as it is.
pub(crate) fn rewrite_array_cast_drop(sql: &str) -> String {
    if !sql.contains("::") {
        return sql.to_string();
    }
    let mut sql = sql.to_string();
    let mut i = 0;
    while i + 1 < sql.len() {
        let s = sql.as_bytes();
        if s[i] == b'\'' {
            i = end_of_string_literal(&sql, i);
            continue;
        }
        if s[i] != b':' || s[i + 1] != b':' {
            i += 1;
            continue;
        }
        let mut m = i + 2;
        while m < s.len() && (s[m] == b' ' || s[m] == b'\t') {
            m += 1;
        }
        let Some((elem, end)) = array_cast_type(&sql, m) else {
            i += 2;
            continue;
        };
        let (os, oe) = operand_before(&sql, i);
        let operand = &sql[os..oe];
        if os < oe && (operand.starts_with('(') || operand.starts_with('$')) && !ddl_statement(&sql) {
            let call = ["overlite_assign(", operand, ", '", &elem.replace('\'', "''"), "[]')"].concat();
            sql = [&sql[..os], &call, &sql[end..]].concat();
            i = os + call.len();
        } else {
            sql = [&sql[..i], &sql[end..]].concat();
        }
    }
    sql
}

fn ddl_statement(sql: &str) -> bool {
    matches!(first_word_upper(sql).as_str(), "CREATE" | "ALTER")
}

/// Maps `x = ANY(arr)` / `x <> ALL(arr)` onto a json_each membership subquery
/// over the JSON-array storage, for a non-literal array operand (literal
/// ARRAY[…] is handled earlier by rewrite_any_array).
pub(crate) fn rewrite_any_array_expr(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut from = 0usize;
    loop {
        let low = code_only(&sql).to_ascii_lowercase();
        let mut idx: Option<usize> = None;
        let mut word = "";
        if let Some(ia) = index_word_call(&low, "any", from) {
            idx = Some(ia);
            word = "any";
        }
        if let Some(il) = index_word_call(&low, "all", from)
            && idx.is_none_or(|i| il < i)
        {
            idx = Some(il);
            word = "all";
        }
        let Some(idx) = idx else { return sql };
        let s = sql.as_bytes();
        let mut open = idx + word.len();
        while open < s.len() && s[open] == b' ' {
            open += 1;
        }
        let Some((arg, end)) = read_paren_args(&sql, open) else {
            from = idx + word.len();
            continue;
        };
        // A real subquery operand stays an IN (…) (handled by rewrite_any_operator);
        // only an array-valued operand becomes a json_each membership.
        let at = arg.trim().to_lowercase();
        if at.starts_with("select") || at.starts_with("(select") || at.starts_with("values") {
            from = idx + word.len();
            continue;
        }
        // The comparison operator just before ANY/ALL.
        let mut j = idx as isize - 1;
        while j >= 0 && (s[j as usize] == b' ' || s[j as usize] == b'\t') {
            j -= 1;
        }
        let oe = (j + 1) as usize;
        while j >= 0 && matches!(s[j as usize], b'=' | b'<' | b'>' | b'!') {
            j -= 1;
        }
        let j1 = (j + 1) as usize;
        let op = &sql[j1..oe];
        if !((word == "any" && op == "=") || (word == "all" && (op == "<>" || op == "!="))) {
            from = idx + word.len();
            continue;
        }
        let mut repl = "IN";
        if word == "all" {
            repl = "NOT IN";
        }
        // overlite_array_json reads the array whatever form it came in: a stored
        // JSON array, or Postgres' '{a,b}' text, which is what a client binds an
        // array parameter as (node-postgres always). Anything else is no elements,
        // an empty set rather than a "malformed JSON" error.
        let a = arg.trim();
        let new_expr = [
            " ",
            repl,
            " (SELECT value FROM json_each(overlite_array_json(",
            a,
            ")))",
        ]
        .concat();
        sql = splice(&sql, j1, end, &new_expr);
        from = j1 + new_expr.len();
    }
}

/// Reports whether w starts at index i in low on a word boundary.
fn match_word_at(low: &str, i: usize, w: &str) -> bool {
    let l = low.as_bytes();
    let wb = w.as_bytes();
    if i + wb.len() > l.len() || &l[i..i + wb.len()] != wb {
        return false;
    }
    i == 0 || !is_ident_part(l[i - 1])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::hstore::hstore_text;

    #[test]
    fn json_outputs() {
        assert_eq!(
            json_array_to_pg_text(&Value::Text(r#"[1,"a b",null,true,[2.50]]"#.into())),
            br#"{1,"a b",NULL,t,{2.50}}"#.to_vec()
        );
        assert_eq!(json_array_to_pg_text(&Value::Text("nope".into())), b"nope".to_vec());
        assert_eq!(
            hstore_text(r#"{"b":1.0,"a":"x","c":null,"d":[1,"<"]}"#),
            r#""a"=>"x", "b"=>"1", "c"=>NULL, "d"=>"[1,\"\\u003c\"]""#
        );
    }
}
