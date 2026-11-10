//! Postgres arrays over SQLite: a `type[]` column keeps its declared type
//! verbatim, and the value is a JSON array in a TEXT cell. On the wire we
//! advertise the proper array OID and render the JSON as Postgres' `{…}` text;
//! on input, ARRAY[…] and '{…}'::type[] become JSON.

use crate::types::range::{OID_TSTZRANGE, range_array_oid_for_decl, range_of_array};
use crate::types::{
    OID_BOOL, OID_BYTEA, OID_DATE, OID_FLOAT8, OID_HSTORE, OID_INT2, OID_INT4, OID_INT8, OID_JSON, OID_JSONB,
    OID_NUMERIC, OID_TEXT, OID_TIME, OID_TIMESTAMP, OID_TIMESTAMPTZ, OID_UUID, OID_VARCHAR, encode_binary,
    oid_from_decl_type,
};
use overlite_core::text::{eq_fold, text_from_bytes, text_to_bytes};
use overlite_core::{Oid, Value, format_float};
use overlite_engine::{Json, json_decode_first, json_marshal, pg_array_text};
use std::collections::HashMap;
use std::sync::LazyLock;

// Array type OIDs (element OID -> array OID).
const OID_BOOL_ARR: Oid = Oid::new(1000);
const OID_BYTEA_ARR: Oid = Oid::new(1001);
pub(crate) const OID_INT2_ARR: Oid = Oid::new(1005);
pub(crate) const OID_INT4_ARR: Oid = Oid::new(1007);
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
const OID_TIME_ARR: Oid = Oid::new(1183);
const OID_TIMESTAMPTZ_ARR: Oid = Oid::new(1185);
/// hstore[], as the catalog's pg_type gives hstore's typarray.
const OID_HSTORE_ARR: Oid = Oid::new(16401);
/// The arrays of the range types (types::range).
const RANGE_ARRAYS: [Oid; 6] = [
    Oid::new(3905),
    Oid::new(3927),
    Oid::new(3907),
    Oid::new(3909),
    Oid::new(3911),
    Oid::new(3913),
];

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
        (OID_TIME_ARR, true),
        (OID_TIMESTAMPTZ_ARR, true),
        (OID_HSTORE_ARR, true),
        (RANGE_ARRAYS[0], true),
        (RANGE_ARRAYS[1], true),
        (RANGE_ARRAYS[2], true),
        (RANGE_ARRAYS[3], true),
        (RANGE_ARRAYS[4], true),
        (RANGE_ARRAYS[5], true),
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
    if let Some(oid) = range_array_oid_for_decl(elem) {
        return oid;
    }
    if eq_fold(elem.trim_matches('"'), "hstore") {
        return OID_HSTORE_ARR;
    }
    match elem.trim_matches('"').to_ascii_lowercase().as_str() {
        "jsonb" => return OID_JSONB_ARR,
        "json" => return OID_JSON_ARR,
        _ => {}
    }
    let oid = oid_from_decl_type(elem);
    if oid == OID_TIMESTAMPTZ {
        OID_TIMESTAMPTZ_ARR
    } else if oid == OID_TIME {
        OID_TIME_ARR
    } else if oid == OID_INT8 {
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
pub(crate) fn json_array_to_pg_text(oid: Oid, v: &Value) -> Vec<u8> {
    // Text and bytes as they are, anything else empty.
    let s = match v {
        Value::Text(x) => std::borrow::Cow::Borrowed(x.as_str()),
        Value::Blob(x) => text_from_bytes(x),
        _ => std::borrow::Cow::Borrowed(""),
    };
    match json_decode_first(&s) {
        // A timestamptz's elements, and a tstzrange's bounds, in the session's zone.
        Some(Json::Arr(arr)) if matches!(array_elem_oid(oid), OID_TIMESTAMPTZ | OID_TSTZRANGE | OID_HSTORE) => {
            let elem = array_elem_oid(oid);
            pg_array_text(&in_session_zone(arr, elem)).into_bytes()
        }
        Some(Json::Arr(arr)) if matches!(oid, OID_JSONB_ARR | OID_JSON_ARR) => {
            pg_array_text(&json_elements(arr)).into_bytes()
        }
        Some(Json::Arr(arr)) => pg_array_text(&arr).into_bytes(),
        Some(Json::Null) => b"{}".to_vec(),
        _ => text_to_bytes(&s).into_owned(),
    }
}

/// The elements of a json or jsonb array, each as its JSON text: an element is held
/// as that text, a string, as Postgres' text form and a client give it, or (as ARRAY[]
/// made it before) as the JSON value itself.
pub(crate) fn json_elements(arr: Vec<Json>) -> Vec<Json> {
    arr.into_iter()
        .map(|j| match j {
            Json::Str(_) | Json::Null => j,
            other => Json::Str(json_marshal(&other)),
        })
        .collect()
}

/// The elements of a timestamptz or tstzrange array as their text in the session's
/// zone, and of an hstore array in hstore's text, at any depth.
fn in_session_zone(arr: Vec<Json>, elem: Oid) -> Vec<Json> {
    arr.into_iter()
        .map(|j| match j {
            Json::Arr(inner) => Json::Arr(in_session_zone(inner, elem)),
            // An hstore element, a JSON object, or the text of one, in hstore's form.
            Json::Obj(_) if elem == OID_HSTORE => Json::Str(crate::types::hstore::hstore_text(&json_marshal(&j))),
            Json::Str(s) if elem == OID_HSTORE => Json::Str(crate::types::hstore::hstore_text(&s)),
            Json::Str(s) => Json::Str(if elem == OID_TSTZRANGE {
                crate::types::range::range_text(elem, &s)
            } else {
                crate::types::binary_time::canonical_time_text(elem, &s)
            }),
            other => other,
        })
        .collect()
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
        OID_TIME_ARR => OID_TIME,
        OID_TIMESTAMPTZ_ARR => OID_TIMESTAMPTZ,
        OID_HSTORE_ARR => OID_HSTORE,
        o => range_of_array(o).unwrap_or(OID_TEXT),
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

// --- input rewrites ---------------------------------------------------------
