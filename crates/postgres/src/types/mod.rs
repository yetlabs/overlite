//! Postgres type OIDs and value encoding.

pub(crate) mod array;
mod binary_time;
mod bool_text;
pub(crate) mod hstore;
pub(crate) mod numeric;
pub(crate) mod param_decode;
pub(crate) mod param_types;
pub(crate) mod range;
pub(crate) mod timestamptz;

use crate::auth::rls::text_or_empty;
use crate::types::array::{
    OID_FLOAT8_ARR, OID_INT2_ARR, OID_INT8_ARR, OID_TEXT_ARR, array_binary, array_oid_for_decl, is_array_decl,
    is_array_oid, json_array_to_pg_text,
};
use crate::types::binary_time::{
    canonical_time_text, encode_binary_date, encode_binary_timestamp, encode_binary_wall_time, is_far_zone_time,
    is_time_oid, parse_stored_time, parse_stored_wall,
};
use crate::types::hstore::{hstore_binary, hstore_text};
use crate::types::numeric::numeric_binary;
use crate::types::param_decode::bytea_hex;
use crate::types::timestamptz::is_timestamptz_decl;
use overlite_core::text::{eq_fold, quoted, text_to_bytes, unescape_vec};
use overlite_core::{Column, Oid, Timestamp, Value, format_float, lazy_regex, parse_bool};
use overlite_engine::{Json, blank_literals, interval_parts, json_decode_first, json_marshal};
use std::collections::HashSet;
use std::fmt;
use std::sync::LazyLock;

/// How a value is laid out on the wire. Only 0 means text: the server reads any other
/// format code as binary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Format {
    #[default]
    Text,
    Binary,
}

impl Format {
    pub(crate) fn from_code(code: i32) -> Format {
        if code == 0 { Format::Text } else { Format::Binary }
    }
}

/// How much of a stored value an error quotes.
const MAX_VALUE_IN_ERROR: usize = 64;

const OID_BOOL: Oid = Oid::new(16);
pub(crate) const OID_BYTEA: Oid = Oid::new(17);
pub(crate) const OID_INT8: Oid = Oid::new(20);
const OID_INT2: Oid = Oid::new(21);
pub(crate) const OID_INT4: Oid = Oid::new(23);
pub(crate) const OID_TEXT: Oid = Oid::new(25);
const OID_JSON: Oid = Oid::new(114);
const OID_FLOAT8: Oid = Oid::new(701);
const OID_BPCHAR: Oid = Oid::new(1042);
const OID_VARCHAR: Oid = Oid::new(1043);
pub(crate) const OID_DATE: Oid = Oid::new(1082);
pub(crate) const OID_TIME: Oid = Oid::new(1083);
pub(crate) const OID_NUMERIC: Oid = Oid::new(1700);
pub(crate) const OID_TIMESTAMP: Oid = Oid::new(1114);
pub(crate) const OID_TIMESTAMPTZ: Oid = Oid::new(1184);
const OID_INTERVAL: Oid = Oid::new(1186);
const OID_UUID: Oid = Oid::new(2950);
const OID_JSONB: Oid = Oid::new(3802);
/// hstore has a per-database OID in Postgres; a fixed sentinel here.
pub(crate) const OID_HSTORE: Oid = Oid::new(16400);

/// A declared type we can advertise faithfully, before value sampling.
fn strict_decl_oid(decl: &str) -> Option<Oid> {
    let d = decl.to_uppercase();
    if d.trim() == "BYTEA" {
        return Some(OID_BYTEA);
    }
    if d.is_empty() {
        return None;
    }
    if d.contains("BOOL") {
        return Some(OID_BOOL);
    }
    if d.contains("JSONB") {
        return Some(OID_JSONB);
    }
    if d.contains("JSON") {
        return Some(OID_JSON);
    }
    if d.contains("UUID") {
        return Some(OID_UUID);
    }
    if d.contains("VARCHAR") || d.contains("CHARACTER VARYING") {
        return Some(OID_VARCHAR);
    }
    if d.contains("CHAR") || d.contains("BPCHAR") {
        return Some(OID_BPCHAR);
    }
    if d.trim() == "TIME" || d.contains("TIME WITHOUT TIME ZONE") {
        return Some(OID_TIME);
    }
    if d.trim_start().starts_with("INTERVAL") {
        return Some(OID_INTERVAL);
    }
    None
}

/// Picks date/time/timestamp based on a column's declared type.
fn time_oid(decl: &str) -> Oid {
    let d = decl.to_uppercase();
    if d.contains("TIMESTAMPTZ") || (d.contains("TIMESTAMP") && d.contains("WITH TIME ZONE")) {
        OID_TIMESTAMPTZ
    } else if d.contains("TIMESTAMP") || d.contains("DATETIME") {
        OID_TIMESTAMP
    } else if d.contains("DATE") {
        OID_DATE
    } else if d.contains("TIME") {
        OID_TIME
    } else {
        OID_TIMESTAMP
    }
}

/// pg_catalog columns that are boolean in PostgreSQL but surface as 0/1 here.
pub(crate) static BOOL_CATALOG_COLUMNS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "attnotnull",
        "atthasdef",
        "atthasmissing",
        "attbyval",
        "attisdropped",
        "attislocal",
        "relhasindex",
        "relisshared",
        "relhasrules",
        "relhastriggers",
        "relhassubclass",
        "relrowsecurity",
        "relforcerowsecurity",
        "relispopulated",
        "relispartition",
        "typnotnull",
        "typbyval",
        "indisprimary",
        "indisunique",
        "indisclustered",
        "indisvalid",
        "indisexclusion",
        "indimmediate",
        "indisready",
        "indislive",
        "indisreplident",
        "conislocal",
        "convalidated",
        "connoinherit",
        "collisdeterministic",
        "condeferrable",
        "condeferred",
        "rolsuper",
        "rolinherit",
        "rolcreaterole",
        "rolcreatedb",
        "rolcanlogin",
        "rolreplication",
        "rolbypassrls",
        "datistemplate",
        "datallowconn",
        "indnullsnotdistinct",
        "indcheckxmin",
        "conperiod",
        "typisdefined",
        "typispreferred",
        "opcdefault",
        "extrelocatable",
        "tgisinternal",
        "tgdeferrable",
        "tginitdeferred",
        "lanispl",
        "lanpltrusted",
        "proisstrict",
        "proretset",
        "prosecdef",
        "proleakproof",
        "pending_restart",
        "is_instead",
        // Prisma's aliases for the booleans it reads.
        "is_partition",
        "has_subclass",
        "has_row_level_security",
        "is_unique",
        "is_primary_key",
        "nulls_first",
        "is_deferrable",
        "is_deferred",
        "extension_relocatable",
        // Boolean expression aliases pg_dump reads as t/f.
        "notnull_islocal",
        "notnull_noinherit",
        "notnull_invalidoid",
        // A sequence read as a relation.
        "is_called",
        // The advisory locks that answer whether they took or let go of one.
        "pg_try_advisory_lock",
        "pg_try_advisory_lock_shared",
        "pg_try_advisory_xact_lock",
        "pg_try_advisory_xact_lock_shared",
        "pg_advisory_unlock",
        "pg_advisory_unlock_shared",
        // The privilege checks that answer from the grants.
        "has_table_privilege",
        "has_column_privilege",
        "has_any_column_privilege",
        // pg_tables / pg_indexes summaries.
        "hasindexes",
        "hasrules",
        "hastriggers",
        "rowsecurity",
    ]
    .into_iter()
    .collect()
});

/// Integer columns of catalog queries that Describe can't type when its probe
/// run returns no rows. Only queries over the catalogs get these.
pub(crate) static INT_CATALOG_COLUMNS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "oid",
        "attnum",
        "attrelid",
        "atttypid",
        "atttypmod",
        "relnamespace",
        "reltype",
        "relam",
        "relnatts",
        "conrelid",
        "confrelid",
        "conindid",
        "contypid",
        "indrelid",
        "indexrelid",
        "indnatts",
        "indnkeyatts",
        "typnamespace",
        "typlen",
        "typelem",
        "typrelid",
        "typbasetype",
        "typtypmod",
        "enumtypid",
        "adrelid",
        "adnum",
        "ordinal_position",
        "numeric_precision",
        "numeric_scale",
        "numeric_precision_radix",
        "datetime_precision",
        "character_maximum_length",
        // Prisma's aliases.
        "con_id",
        "child",
        "parent",
        "colidx",
        "column_index",
        "indkeyid",
        "indkeyidx",
    ]
    .into_iter()
    .collect()
});

lazy_regex!(RE_CATALOG_REF, r"(?i)\b(pg_[a-z_]+|information_schema)\b");

/// Whether a statement reads the catalogs.
pub(crate) fn is_catalog_query(sql: &str) -> bool {
    RE_CATALOG_REF.is_match(&blank_literals(sql))
}

/// Picks an OID for a column: SQLite is dynamically typed, so the runtime type
/// of the first non-NULL value is preferred, then the declared type affinity.
pub(crate) fn oid_for_column(col: &Column, rows: &[Vec<Value>], idx: usize) -> Oid {
    if BOOL_CATALOG_COLUMNS.contains(col.name.as_str()) {
        return OID_BOOL;
    }
    // An integer's width, from the AST path's type environment (the engine's mark).
    match col.decl_type.as_str() {
        "PG_INT2" => return OID_INT2,
        "PG_INT4" => return OID_INT4,
        "PG_INT8" => return OID_INT8,
        "PG_INT2[]" => return crate::types::array::OID_INT2_ARR,
        "PG_INT4[]" => return crate::types::array::OID_INT4_ARR,
        "PG_INT8[]" => return crate::types::array::OID_INT8_ARR,
        _ => {}
    }
    // pg_constraint's key columns, int2[] held as JSON arrays.
    if matches!(col.name.as_str(), "conkey" | "confkey") {
        return OID_INT2_ARR;
    }
    // pg_class.reloptions (and pg_dump's name for it), text[] held as a JSON array.
    if matches!(col.name.as_str(), "reloptions" | "indreloptions") {
        return crate::types::array::OID_TEXT_ARR;
    }
    // A range, under its own type.
    if let Some(oid) = crate::types::range::range_oid_for_decl(&col.decl_type) {
        return oid;
    }
    // An array whose element type its values say: array_agg, ARRAY(subquery).
    if col.decl_type.eq_ignore_ascii_case("ANYARRAY") {
        return sampled_array_oid(rows, idx);
    }
    // Array columns carry their element type in the declared type (`text[]`).
    if is_array_decl(&col.decl_type) {
        return array_oid_for_decl(&col.decl_type);
    }
    // timestamptz is stored as text; trust the declared type.
    if is_timestamptz_decl(&col.decl_type) {
        return OID_TIMESTAMPTZ;
    }
    // numeric is stored as exact decimal text (declared DECIMALTEXT).
    if col.decl_type.to_uppercase().contains("DECIMALTEXT") {
        return OID_NUMERIC;
    }
    if eq_fold(col.decl_type.trim(), "hstore") {
        return OID_HSTORE;
    }
    // A faithfully-mappable declared type wins over value sampling.
    if let Some(oid) = strict_decl_oid(&col.decl_type) {
        return oid;
    }
    for row in rows {
        match row.get(idx) {
            None | Some(Value::Null) => {}
            Some(Value::Bool(_)) => return OID_BOOL,
            Some(Value::Int(_)) => return OID_INT8,
            Some(Value::Float(_)) => return OID_FLOAT8,
            Some(Value::Time(_)) => return time_oid(&col.decl_type),
            Some(Value::Blob(_)) => {
                return if is_text_affinity(&col.decl_type) {
                    OID_TEXT
                } else {
                    OID_BYTEA
                };
            }
            Some(Value::Text(s)) if is_far_zone_time(&col.decl_type, s) => return time_oid(&col.decl_type),
            Some(Value::Text(_)) => return OID_TEXT,
        }
    }
    oid_from_decl_type(&col.decl_type)
}

/// The array type of a column of arrays by the elements of its first one: integers,
/// floats or booleans, else text.
fn sampled_array_oid(rows: &[Vec<Value>], idx: usize) -> Oid {
    let first = rows.iter().find_map(|row| match row.get(idx) {
        Some(Value::Text(s)) => Some(s.as_str()),
        _ => None,
    });
    let Some(text) = first else {
        return OID_TEXT_ARR;
    };
    let items: Vec<String> = match json_decode_first(text) {
        Some(Json::Arr(arr)) => arr.iter().map(json_marshal).collect(),
        _ => text
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .split(',')
            .map(str::to_string)
            .collect(),
    };
    let items: Vec<&str> = items
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
        .collect();
    if items.is_empty() {
        OID_TEXT_ARR
    } else if items.iter().all(|s| s.parse::<i64>().is_ok()) {
        OID_INT8_ARR
    } else if items.iter().all(|s| s.parse::<f64>().is_ok()) {
        OID_FLOAT8_ARR
    } else {
        OID_TEXT_ARR
    }
}

/// Maps a declared type to an OID using SQLite's affinity rules.
fn oid_from_decl_type(decl: &str) -> Oid {
    let d = decl.to_uppercase();
    if d.is_empty() {
        OID_TEXT
    } else if d.contains("INT") {
        OID_INT8
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") || d.contains("STRING") {
        OID_TEXT
    } else if d.contains("BLOB") || d.trim() == "BYTEA" {
        OID_BYTEA
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        OID_FLOAT8
    } else if d.contains("BOOL") {
        OID_BOOL
    } else if d.contains("NUMERIC") || d.contains("DEC") {
        OID_NUMERIC
    } else if d.contains("DATE") || d.contains("TIME") {
        time_oid(&d)
    } else {
        OID_TEXT
    }
}

fn is_text_affinity(decl: &str) -> bool {
    let d = decl.to_uppercase();
    d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT")
}

/// Whether v represents a true boolean, in the forms SQLite may hold.
fn boolish(v: &Value) -> bool {
    match v {
        Value::Bool(x) => *x,
        Value::Text(x) => matches!(
            x.trim().to_lowercase().as_str(),
            "t" | "true" | "1" | "y" | "yes" | "on"
        ),
        other => to_int64(other) != 0,
    }
}

/// `YYYY-MM-DD HH:MM:SS`: fraction truncated to six digits with trailing zeros
/// removed.
fn fmt_fraction(t: &Timestamp) -> String {
    use chrono::Timelike;
    let micros = t.nanosecond() / 1000 % 1_000_000;
    if micros == 0 {
        return String::new();
    }
    let s = format!(".{micros:06}");
    s.trim_end_matches('0').to_string()
}

/// Renders a value in Postgres text format; None is SQL NULL.
fn encode_text(oid: Oid, v: &Value) -> Option<Vec<u8>> {
    if v.is_null() {
        return None;
    }
    // Array columns hold a JSON array in a TEXT cell; render Postgres' `{...}`.
    if is_array_oid(oid) {
        return Some(json_array_to_pg_text(oid, v));
    }
    // A time value is rendered the way Postgres renders it, whatever shape it
    // was stored in.
    if is_time_oid(oid)
        && let Value::Text(s) = v
    {
        return Some(unescape_vec(canonical_time_text(oid, s).into_bytes()));
    }
    // A range as range_out writes it, a tstzrange's bounds in the session's zone.
    if crate::types::range::is_range_oid(oid) {
        return Some(text_to_bytes(&crate::types::range::range_text(oid, &text_or_empty(v))).into_owned());
    }
    // hstore is stored as a JSON object; render it as hstore text.
    if oid == OID_HSTORE {
        return Some(unescape_vec(hstore_text(&text_or_empty(v)).into_bytes()));
    }
    // Boolean columns may arrive as 0/1 integers or a stored 't'/'true' string.
    if oid == OID_BOOL {
        return Some(if boolish(v) { b"t".to_vec() } else { b"f".to_vec() });
    }
    Some(match v {
        Value::Bool(b) => {
            if *b {
                b"t".to_vec()
            } else {
                b"f".to_vec()
            }
        }
        Value::Blob(val) => {
            if oid == OID_BYTEA {
                format!("\\x{}", hex::encode(val)).into_bytes()
            } else {
                val.clone()
            }
        }
        Value::Text(val) => text_to_bytes(val).into_owned(),
        Value::Time(val) => match oid {
            OID_DATE => val.format("%Y-%m-%d").to_string().into_bytes(),
            OID_TIME => format!("{}{}", val.format("%H:%M:%S"), fmt_fraction(val)).into_bytes(),
            OID_TIMESTAMPTZ => {
                crate::types::binary_time::in_session_zone(&val.with_timezone(&chrono::Utc).naive_utc()).into_bytes()
            }
            _ => format!("{}{}", val.format("%Y-%m-%d %H:%M:%S"), fmt_fraction(val)).into_bytes(),
        },
        Value::Int(i) => i.to_string().into_bytes(),
        Value::Float(f) => format_float(*f).into_bytes(),
        other @ Value::Null => other.to_string().into_bytes(),
    })
}

/// A stored value that has no binary form of its type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NoBinaryForm {
    /// The start of the value, in text.
    value: String,
    oid: Oid,
}

impl fmt::Display for NoBinaryForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "stored value {:?} has no binary form of type {}",
            quoted(&self.value),
            self.oid
        )
    }
}

/// Renders a value in the requested wire format; None is SQL NULL.
pub(crate) fn encode_value(format: Format, oid: Oid, v: &Value) -> Result<Option<Vec<u8>>, NoBinaryForm> {
    if v.is_null() {
        return Ok(None);
    }
    if format == Format::Text {
        return Ok(encode_text(oid, v));
    }
    if let Some(bytes) = encode_binary(oid, v) {
        Ok(Some(bytes))
    } else {
        let text = encode_text(OID_TEXT, v).unwrap_or_default();
        let value = String::from_utf8_lossy(&text)
            .chars()
            .take(MAX_VALUE_IN_ERROR)
            .collect();
        Err(NoBinaryForm { value, oid })
    }
}

/// Binary format for an OID; the layout must match what the row description
/// advertised, so the value is coerced to the OID's shape. Every OID oid_for_column
/// can give has its case here or is text on the wire in both formats. None for a stored
/// value that cannot take the shape, which Postgres would have refused on the way in:
/// format 1 promises these bytes, so its text must not go out in their place.
pub(crate) fn encode_binary(oid: Oid, v: &Value) -> Option<Vec<u8>> {
    let text = |v: &Value| encode_text(OID_TEXT, v).unwrap_or_default();
    Some(match oid {
        OID_BOOL => vec![bool_form(v)? as u8],
        OID_INT2 => i16::try_from(int_form(v)?).ok()?.to_be_bytes().to_vec(),
        OID_INT4 => i32::try_from(int_form(v)?).ok()?.to_be_bytes().to_vec(),
        OID_INT8 => int_form(v)?.to_be_bytes().to_vec(),
        OID_FLOAT8 => float_form(v)?.to_bits().to_be_bytes().to_vec(),
        OID_BYTEA => {
            if let Value::Blob(b) = v {
                return Some(b.clone());
            }
            // Older databases and untyped text binds can contain the hex form.
            if let Value::Text(s) = v
                && let Some(b) = bytea_hex(s)
            {
                return Some(b);
            }
            unescape_vec(v.to_string().into_bytes())
        }
        OID_JSONB => {
            // A 1-byte version (1) followed by the JSON text.
            let mut out = vec![1u8];
            out.extend_from_slice(&text(v));
            out
        }
        OID_UUID => uuid_bytes(v)?,
        OID_NUMERIC => numeric_binary(&String::from_utf8_lossy(&text(v)))?,
        OID_HSTORE => hstore_binary(v)?,
        o if crate::types::range::is_range_oid(o) => crate::types::range::range_binary(o, v)?,
        OID_TIMESTAMP | OID_TIMESTAMPTZ => match parse_stored_time(v) {
            Some(tm) => encode_binary_timestamp(&tm),
            None => match infinity(v)? {
                Infinity::Positive => i64::MAX.to_be_bytes().to_vec(),
                Infinity::Negative => i64::MIN.to_be_bytes().to_vec(),
            },
        },
        OID_DATE => match parse_stored_time(v) {
            Some(tm) => encode_binary_date(&tm),
            None => match infinity(v)? {
                Infinity::Positive => i32::MAX.to_be_bytes().to_vec(),
                Infinity::Negative => i32::MIN.to_be_bytes().to_vec(),
            },
        },
        OID_TIME => encode_binary_wall_time(&parse_stored_wall(v)?.0),
        // interval_send: the microseconds, the days, the months.
        OID_INTERVAL => {
            let (months, days, micros) = interval_parts(&String::from_utf8_lossy(&text(v)))?;
            let mut out = micros.to_be_bytes().to_vec();
            out.extend_from_slice(&i32::try_from(days).ok()?.to_be_bytes());
            out.extend_from_slice(&i32::try_from(months).ok()?.to_be_bytes());
            out
        }
        _ if is_array_oid(oid) => array_binary(oid, v)?,
        _ => text(v), // text, json, varchar, bpchar, ...: UTF-8 bytes
    })
}

#[derive(Debug)]
enum Infinity {
    Positive,
    Negative,
}

/// Infinity as a timestamp or a date holds it: the word, stored as a client gave it.
fn infinity(v: &Value) -> Option<Infinity> {
    let Value::Text(s) = v else { return None };
    match s.trim().to_ascii_lowercase().as_str() {
        "infinity" | "+infinity" => Some(Infinity::Positive),
        "-infinity" => Some(Infinity::Negative),
        _ => None,
    }
}

/// A stored UUID as its 16 raw bytes: the text in the forms Postgres reads, or the
/// bytes themselves, as an untyped binary parameter leaves them.
fn uuid_bytes(v: &Value) -> Option<Vec<u8>> {
    let s = match v {
        Value::Blob(b) if b.len() == 16 => return Some(b.clone()),
        Value::Text(s) => s,
        _ => return None,
    };
    let s = s.trim();
    let s = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(s);
    let hex_only = s.replace('-', "");
    if hex_only.len() != 32 {
        return None;
    }
    hex::decode(hex_only).ok()
}

pub(crate) fn to_int64(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::Float(n) => *n as i64,
        Value::Bool(n) => *n as i64,
        Value::Text(n) => n.trim().parse().unwrap_or(0),
        Value::Blob(n) => String::from_utf8_lossy(n).trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// A stored value as an integer, None if it is not one in any spelling.
fn int_form(v: &Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(*n),
        Value::Bool(n) => Some(*n as i64),
        Value::Float(n) if n.fract() == 0.0 && n.abs() < 9.2e18 => Some(*n as i64),
        Value::Text(n) => n.trim().parse().ok(),
        Value::Blob(n) => String::from_utf8_lossy(n).trim().parse().ok(),
        _ => None,
    }
}

/// A stored value as a float, None if it is not one in any spelling.
fn float_form(v: &Value) -> Option<f64> {
    match v {
        Value::Float(n) => Some(*n),
        Value::Int(n) => Some(*n as f64),
        Value::Text(n) => n.trim().parse().ok(),
        Value::Blob(n) => String::from_utf8_lossy(n).trim().parse().ok(),
        _ => None,
    }
}

/// A stored value as a boolean, None if it is not one in any spelling.
fn bool_form(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Int(n) => Some(*n != 0),
        Value::Float(n) => Some(*n != 0.0),
        Value::Text(s) => parse_bool(s),
        Value::Blob(b) => parse_bool(&String::from_utf8_lossy(b)),
        Value::Null | Value::Time(_) => None,
    }
}
