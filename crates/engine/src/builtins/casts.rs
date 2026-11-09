//! What a cast does, type by type. SQLite's own CAST goes by affinity, so it reads
//! `'2024-01-02 03:04:05'` as the number 2024 and `'abc'` as 0 without a word, and
//! gives anything named `interval` or `point` the affinity of an integer. Every cast
//! but one to text becomes `overlite_cast(value, 'type')`, which answers as Postgres'
//! input function for the type does.

use crate::builtins::dates::from_time_zone;
use crate::builtins::decimal::{dec_string_exact, rat_from_string, round_half_away};
use crate::builtins::json::json_is_valid;
use crate::builtins::ranges::{RangeKind, range_input};
use crate::error::{Error, Result};
use chrono::{Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use overlite_core::{SqlState, Value};

/// A cast target, as the rewrite names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CastType {
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric(Option<(i64, i64)>),
    Date,
    Time(Option<u32>),
    TimeTz(Option<u32>),
    Timestamp(Option<u32>),
    TimestampTz(Option<u32>),
    Interval,
    Uuid,
    Json,
    Jsonb,
    Bytea,
    Name,
    Char(usize),
    Varchar(Option<usize>),
    /// A range type, in its canonical form.
    Range(RangeKind),
    /// A type with no behavior here: its value stays as text.
    Text,
}

/// The modifiers of a type name: `(10, 2)` of `numeric(10, 2)`.
fn modifiers(ty: &str) -> (String, Vec<i64>) {
    let Some(open) = ty.find('(') else {
        return (ty.trim().to_ascii_lowercase(), Vec::new());
    };
    let close = ty.rfind(')').unwrap_or(ty.len());
    let mods = ty[open + 1..close.max(open + 1)]
        .split(',')
        .filter_map(|n| n.trim().parse().ok())
        .collect();
    // A modifier may stand before the words that end the name.
    let name = [&ty[..open], ty.get(close + 1..).unwrap_or("")].concat();
    (
        name.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase(),
        mods,
    )
}

impl CastType {
    /// The type a name in a cast means, None for a name Postgres does not ship.
    #[must_use]
    pub fn parse(name: &str) -> Option<CastType> {
        let (base, mods) = modifiers(name);
        let base = base.strip_prefix("pg_catalog.").unwrap_or(&base);
        let first = mods.first().copied();
        let precision = first.map(|p| p.clamp(0, 6) as u32);
        Some(match base {
            "int2" | "smallint" | "smallserial" => CastType::Int2,
            "int" | "int4" | "integer" | "serial" => CastType::Int4,
            "int8" | "bigint" | "bigserial" | "oid" | "xid" | "cid" | "regtype" => CastType::Int8,
            "float4" | "real" => CastType::Float4,
            "float8" | "double precision" | "float" => CastType::Float8,
            "numeric" | "decimal" => CastType::Numeric(first.map(|p| (p, mods.get(1).copied().unwrap_or(0)))),
            "date" => CastType::Date,
            "time" | "time without time zone" => CastType::Time(precision),
            "timetz" | "time with time zone" => CastType::TimeTz(precision),
            "timestamp" | "timestamp without time zone" => CastType::Timestamp(precision),
            "timestamptz" | "timestamp with time zone" => CastType::TimestampTz(precision),
            "interval" => CastType::Interval,
            "uuid" => CastType::Uuid,
            "json" => CastType::Json,
            "jsonb" => CastType::Jsonb,
            "bytea" => CastType::Bytea,
            "name" => CastType::Name,
            "char" | "character" | "bpchar" => match first {
                Some(n) => CastType::Char(n.max(1) as usize),
                None if base == "bpchar" => CastType::Varchar(None),
                None => CastType::Char(1),
            },
            "varchar" | "character varying" => CastType::Varchar(first.map(|n| n.max(1) as usize)),
            "text" | "citext" | "inet" | "cidr" | "macaddr" | "macaddr8" | "bit" | "bit varying" | "varbit"
            | "money" | "xml" | "point" | "line" | "lseg" | "box" | "path" | "polygon" | "circle" | "tsvector"
            | "tsquery" | "pg_lsn" | "int4multirange" | "unknown" | "record" | "cstring" | "tid" | "void"
            | "anyelement" | "anyarray" | "regclass" | "regproc" | "regprocedure" | "regoper" | "regoperator"
            | "regconfig" | "regdictionary" | "regnamespace" | "regrole" | "hstore" | "\"char\"" | "pg_node_tree"
            | "aclitem" => CastType::Text,
            _ => return RangeKind::of_name(base).map(CastType::Range),
        })
    }

    /// The name a result column of this type goes by, unless its value has one.
    pub fn label(&self) -> &'static str {
        match self {
            CastType::Int2 => "int2",
            CastType::Int4 => "int4",
            CastType::Int8 => "int8",
            CastType::Float4 => "float4",
            CastType::Float8 => "float8",
            CastType::Time(_) => "time",
            CastType::TimeTz(_) => "timetz",
            CastType::Timestamp(_) => "timestamp",
            CastType::TimestampTz(_) => "timestamptz",
            CastType::Char(_) => "bpchar",
            CastType::Varchar(_) => "varchar",
            other => other.display(),
        }
    }

    /// The name in Postgres' messages, as format_type gives it.
    fn display(&self) -> &'static str {
        match self {
            CastType::Int2 => "smallint",
            CastType::Int4 => "integer",
            CastType::Int8 => "bigint",
            CastType::Float4 => "real",
            CastType::Float8 => "double precision",
            CastType::Numeric(_) => "numeric",
            CastType::Date => "date",
            CastType::Time(_) => "time without time zone",
            CastType::TimeTz(_) => "time with time zone",
            CastType::Timestamp(_) => "timestamp without time zone",
            CastType::TimestampTz(_) => "timestamp with time zone",
            CastType::Interval => "interval",
            CastType::Uuid => "uuid",
            CastType::Json => "json",
            CastType::Jsonb => "jsonb",
            CastType::Bytea => "bytea",
            CastType::Name => "name",
            CastType::Char(_) => "character",
            CastType::Varchar(_) => "character varying",
            CastType::Range(kind) => kind.name(),
            CastType::Text => "text",
        }
    }

    /// The name in an input function's message, which is not always format_type's.
    fn input_name(&self) -> &'static str {
        match self {
            CastType::Timestamp(_) => "timestamp",
            CastType::TimestampTz(_) => "timestamp with time zone",
            CastType::Time(_) => "time",
            CastType::TimeTz(_) => "time with time zone",
            other => other.display(),
        }
    }
}

fn bad_input(ty: &CastType, text: &str) -> Error {
    let state = match ty {
        CastType::Date
        | CastType::Time(_)
        | CastType::TimeTz(_)
        | CastType::Timestamp(_)
        | CastType::TimestampTz(_)
        | CastType::Interval => SqlState::INVALID_DATETIME_FORMAT,
        _ => SqlState::INVALID_TEXT_REPRESENTATION,
    };
    Error::with_state(
        state,
        format!("invalid input syntax for type {}: \"{text}\"", ty.input_name()),
    )
}

fn out_of_range(text: &str) -> Error {
    Error::with_state(
        SqlState::DATETIME_FIELD_OVERFLOW,
        format!("date/time field value out of range: \"{text}\""),
    )
}

/// Runs a cast. NULL casts to NULL.
///
/// # Errors
///
/// What Postgres answers for a value that is not one of the type: 22P02, 22007 or
/// 22008 for text, 22003 for a number out of range, and 42846 for a value of a type
/// that has no cast to this one.
pub fn cast(v: &Value, ty: &CastType) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    match ty {
        CastType::Int2 => cast_int(v, ty, i64::from(i16::MIN), i64::from(i16::MAX)),
        CastType::Int4 => cast_int(v, ty, i64::from(i32::MIN), i64::from(i32::MAX)),
        CastType::Int8 => cast_int(v, ty, i64::MIN, i64::MAX),
        CastType::Float4 | CastType::Float8 => cast_float(v, ty),
        CastType::Numeric(m) => cast_numeric(v, *m),
        CastType::Date => {
            let text = stamp_text(v, ty)?;
            match parse_stamp(&text, ty)? {
                Parsed::Special(word) => Ok(Value::Text(word)),
                Parsed::Stamp(s) => Ok(Value::Text(s.instant(ty, &text)?.format("%Y-%m-%d").to_string())),
            }
        }
        CastType::Timestamp(p) | CastType::TimestampTz(p) => {
            let text = stamp_text(v, ty)?;
            match parse_stamp(&text, ty)? {
                Parsed::Special(word) => Ok(Value::Text(word)),
                Parsed::Stamp(s) => Ok(Value::Text(format_stamp(s.instant(ty, &text)?, *p))),
            }
        }
        CastType::Time(p) => {
            let text = stamp_text(v, ty)?;
            let t = parse_time_of_day(&text, ty)?;
            Ok(Value::Text(format_time(t.time, *p)))
        }
        CastType::TimeTz(p) => {
            let text = stamp_text(v, ty)?;
            let t = parse_time_of_day(&text, ty)?;
            let offset = t.offset.unwrap_or(0);
            Ok(Value::Text(format_time(t.time, *p) + &format_offset(offset)))
        }
        CastType::Interval => {
            let text = stamp_text(v, ty)?;
            Ok(Value::Text(
                parse_interval(&text).ok_or_else(|| bad_input(ty, &text))?.to_string(),
            ))
        }
        CastType::Uuid => {
            let text = input_text(v);
            normalize_uuid(&text)
                .map(Value::Text)
                .ok_or_else(|| bad_input(ty, &text))
        }
        CastType::Json | CastType::Jsonb => {
            let text = input_text(v);
            if !json_is_valid(&text) {
                return Err(bad_input(ty, &text));
            }
            Ok(Value::Text(text))
        }
        CastType::Bytea => cast_bytea(v),
        CastType::Name => Ok(Value::Text(truncate_chars(&input_text(v), 63).to_string())),
        CastType::Char(n) => {
            let text = input_text(v);
            let kept = truncate_chars(&text, *n);
            let pad = n.saturating_sub(kept.chars().count());
            Ok(Value::Text(kept.to_string() + &" ".repeat(pad)))
        }
        CastType::Varchar(Some(n)) => Ok(Value::Text(truncate_chars(&input_text(v), *n).to_string())),
        CastType::Varchar(None) | CastType::Text => Ok(Value::Text(input_text(v))),
        CastType::Range(kind) => Ok(Value::Text(range_input(*kind, &input_text(v))?)),
    }
}

/// timestamptz_in as Postgres runs it for text a client gives: without a zone of its
/// own, the text is a wall clock in the session's TimeZone. cast() reads it as UTC,
/// the form a timestamptz is stored in, which is what it is given everywhere else.
///
/// # Errors
///
/// What cast() answers for text no timestamptz reads.
pub fn timestamptz_in_session(v: &Value, precision: Option<u32>) -> Result<Value> {
    let ty = CastType::TimestampTz(precision);
    if v.is_null() {
        return Ok(Value::Null);
    }
    let text = stamp_text(v, &ty)?;
    match parse_stamp(&text, &ty)? {
        Parsed::Special(word) => Ok(Value::Text(word)),
        Parsed::Stamp(s) if s.offset.is_none() && s.zone.is_none() && s.date.is_some() => {
            let wall = s.instant(&CastType::Timestamp(precision), &text)?;
            let utc = crate::builtins::dates::session_wall_to_utc(&wall).ok_or_else(|| bad_input(&ty, &text))?;
            Ok(Value::Text(format_stamp(utc, precision)))
        }
        Parsed::Stamp(s) => Ok(Value::Text(format_stamp(s.instant(&ty, &text)?, precision))),
    }
}

/// A value as the text its type's input function would read.
fn input_text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        other => other.to_string(),
    }
}

fn truncate_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

fn cast_int(v: &Value, ty: &CastType, min: i64, max: i64) -> Result<Value> {
    let name = ty.display();
    let range = || Error::with_state(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, format!("{name} out of range"));
    let n = match v {
        Value::Int(n) => *n,
        Value::Bool(b) => i64::from(*b),
        // Half away from zero, as numeric's cast does: a literal is numeric in Postgres
        // and a real here, and nothing tells them apart.
        Value::Float(f) => {
            if !f.is_finite() || f.abs() >= 9.3e18 {
                return Err(range());
            }
            f.round() as i64
        }
        _ => {
            let text = input_text(v);
            let Some(n) = parse_integer(&text) else {
                return Err(bad_input(ty, &text));
            };
            let Ok(n) = i64::try_from(n) else {
                return Err(value_out_of_range(&text, name));
            };
            if n < min || n > max {
                return Err(value_out_of_range(&text, name));
            }
            n
        }
    };
    if n < min || n > max {
        return Err(range());
    }
    Ok(Value::Int(n))
}

fn value_out_of_range(text: &str, name: &str) -> Error {
    Error::with_state(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("value \"{text}\" is out of range for type {name}"),
    )
}

/// An integer as int4in reads it: spaces around it, a sign, `_` between digits, and
/// the 0x, 0o and 0b prefixes.
fn parse_integer(text: &str) -> Option<i128> {
    let s = text.trim();
    let (neg, s) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (radix, digits) = match s.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("0x") => (16, &s[2..]),
        Some("0o") => (8, &s[2..]),
        Some("0b") => (2, &s[2..]),
        _ => (10, s),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let mut n: i128 = 0;
    for c in digits.chars().filter(|c| *c != '_') {
        n = n
            .saturating_mul(radix as i128)
            .saturating_add(c.to_digit(radix)? as i128);
    }
    Some(if neg { -n } else { n })
}

fn cast_float(v: &Value, ty: &CastType) -> Result<Value> {
    match v {
        Value::Int(n) => Ok(Value::Float(*n as f64)),
        Value::Float(f) => Ok(Value::Float(*f)),
        Value::Bool(b) => Ok(Value::Float(f64::from(u8::from(*b)))),
        _ => {
            let text = input_text(v);
            let t = text.trim();
            let word = t.trim_start_matches(['+', '-']).to_ascii_lowercase();
            let numeric_start = t
                .trim_start_matches(['+', '-'])
                .starts_with(|c: char| c.is_ascii_digit() || c == '.');
            if !numeric_start && !matches!(word.as_str(), "nan" | "inf" | "infinity") {
                return Err(bad_input(ty, &text));
            }
            t.parse().map(Value::Float).map_err(|_| bad_input(ty, &text))
        }
    }
}

/// Whether text reads as a number, as numeric_in does, words included.
fn is_special_number(text: &str) -> bool {
    matches!(
        text.trim().trim_start_matches(['+', '-']).to_ascii_lowercase().as_str(),
        "nan" | "inf" | "infinity"
    )
}

/// Rounds a decimal to numeric(p,s), half away from zero, as apply_typmod does.
///
/// # Errors
///
/// 22003 when its integer part does not fit.
pub(crate) fn round_numeric(text: &str, precision: i64, scale: i64) -> Result<Option<String>> {
    let Some(r) = rat_from_string(text) else {
        return Ok(None);
    };
    // A negative scale rounds to tens, hundreds and so on.
    let tens = rat_from_string(&format!("1{}", "0".repeat(scale.unsigned_abs() as usize)));
    let rounded = match tens {
        Some(tens) if scale < 0 => dec_string_exact(&round_half_away(&r.quo(&tens), 0).mul(&tens)),
        _ => dec_string_exact(&round_half_away(&r, scale)),
    };
    let digits = rounded.trim_start_matches('-');
    let whole = digits.split('.').next().unwrap_or("").trim_start_matches('0').len() as i64;
    if whole > precision - scale {
        let limit = if scale > 0 {
            format!(
                "A field with precision {precision}, scale {scale} must round to an absolute value less than 10^{}.",
                precision - scale
            )
        } else {
            format!(
                "A field with precision {precision}, scale 0 must round to an absolute value less than 10^{precision}."
            )
        };
        return Err(Error::with_state(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("numeric field overflow: {limit}"),
        ));
    }
    Ok(Some(with_scale(&rounded, scale)))
}

/// A column's declared type if a value has to be made to fit it on the way in, in the
/// words `assign` reads, None for one that takes what it is given: each type the
/// storage format has a canonical form for, and each array.
#[must_use]
pub fn assignment_type(decl: &str) -> Option<String> {
    let d = decl
        .trim()
        .trim_matches('"')
        .to_ascii_lowercase()
        .replace("decimaltext", "numeric");
    let base = d.trim_end_matches("[]").trim();
    if base == "boolean" || base == "bool" {
        return Some(if base == d {
            "boolean".to_string()
        } else {
            "boolean[]".to_string()
        });
    }
    if base != d {
        return Some(d);
    }
    if matches!(base, "hstore" | "tsvector" | "tsquery") {
        return Some(d);
    }
    match CastType::parse(base)? {
        CastType::Numeric(Some(_))
        | CastType::Varchar(Some(_))
        | CastType::Char(_)
        | CastType::Date
        | CastType::Time(_)
        | CastType::TimeTz(_)
        | CastType::Timestamp(_)
        | CastType::TimestampTz(_)
        | CastType::Interval
        | CastType::Uuid
        | CastType::Range(_)
        | CastType::Bytea => Some(d),
        _ => None,
    }
}

/// Makes a value fit a column, as Postgres' assignment cast does, and puts it in the
/// form the storage format keeps: a numeric is rounded to its scale, too long a string
/// is an error unless what goes is spaces, a date, time, interval or uuid is the text
/// its type's output gives, a bytea a blob, an array a JSON array, an hstore a JSON
/// object.
///
/// # Errors
///
/// 22003 for a numeric past its precision, 22001 for a string past its length, and
/// what the type's input function answers for text it does not read: 22P02, 22007,
/// 22008, 22023, 42601.
pub fn assign(v: &Value, decl: &str) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    if let Some(element) = decl.strip_suffix("[]") {
        // `int[][]` is an array of ints, of any dimensions, as in Postgres.
        let element = element.trim_end_matches("[]");
        return crate::builtins::canonical::canonical_array(v, element);
    }
    if decl == "boolean" {
        return assign_bool(v);
    }
    if decl == "hstore" {
        return crate::builtins::canonical::canonical_hstore(v);
    }
    if decl == "tsvector" || decl == "tsquery" {
        return crate::builtins::fts::canonical(v, decl);
    }
    let Some(ty) = CastType::parse(decl) else {
        return Ok(v.clone());
    };
    match &ty {
        CastType::Numeric(Some((p, s))) => assign_numeric(v, *p, *s),
        CastType::Varchar(Some(n)) => assign_text(v, *n, &format!("character varying({n})")),
        CastType::Char(n) => assign_text(v, *n, &format!("character({n})")),
        CastType::Date
        | CastType::Time(_)
        | CastType::TimeTz(_)
        | CastType::Timestamp(_)
        | CastType::TimestampTz(_)
        | CastType::Interval
        | CastType::Range(_)
        | CastType::Bytea => cast(v, &ty),
        // A driver's untyped binary uuid: its sixteen bytes.
        CastType::Uuid => match v {
            Value::Blob(b) if b.len() == 16 => {
                let h = hex::encode(b);
                Ok(Value::Text(format!(
                    "{}-{}-{}-{}-{}",
                    &h[..8],
                    &h[8..12],
                    &h[12..16],
                    &h[16..20],
                    &h[20..]
                )))
            }
            _ => cast(v, &ty),
        },
        _ => Ok(v.clone()),
    }
}

/// A boolean is stored as 1 or 0, whatever spelling it came in: a quoted literal, a
/// text parameter, a COPY row. Text no boolean reads as is 22P02, as in Postgres.
fn assign_bool(v: &Value) -> Result<Value> {
    match v {
        Value::Int(n) => Ok(Value::Int(i64::from(*n != 0))),
        Value::Float(f) => Ok(Value::Int(i64::from(*f != 0.0))),
        Value::Bool(b) => Ok(Value::Int(i64::from(*b))),
        Value::Text(s) => match overlite_core::parse_bool(s) {
            Some(b) => Ok(Value::Int(i64::from(b))),
            None => Err(Error::with_state(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("invalid input syntax for type boolean: \"{s}\""),
            )),
        },
        other => Ok(other.clone()),
    }
}

fn assign_numeric(v: &Value, precision: i64, scale: i64) -> Result<Value> {
    let text = input_text(v);
    let word = text.trim().trim_start_matches(['+', '-']).to_ascii_lowercase();
    if word == "nan" {
        return Ok(v.clone());
    }
    if word == "inf" || word == "infinity" {
        return Err(Error::with_state(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!(
                "numeric field overflow: A field with precision {precision}, scale {scale} cannot hold an infinite value."
            ),
        ));
    }
    match round_numeric(&text, precision, scale)? {
        Some(rounded) => Ok(Value::Text(rounded)),
        None => Err(bad_input(&CastType::Numeric(None), &text)),
    }
}

fn assign_text(v: &Value, limit: usize, type_name: &str) -> Result<Value> {
    let text = input_text(v);
    let kept = truncate_chars(&text, limit);
    if kept.len() == text.len() {
        return Ok(v.clone());
    }
    if text[kept.len()..].bytes().all(|c| c == b' ') {
        return Ok(Value::Text(kept.to_string()));
    }
    Err(Error::with_state(
        SqlState::STRING_DATA_RIGHT_TRUNCATION,
        format!("value too long for type {type_name}"),
    ))
}

/// A decimal written with exactly scale fractional digits.
pub(crate) fn with_scale(text: &str, scale: i64) -> String {
    if scale <= 0 {
        return text.to_string();
    }
    let (whole, frac) = text.split_once('.').unwrap_or((text, ""));
    format!("{whole}.{frac:0<width$}", width = scale as usize)
}

fn cast_numeric(v: &Value, m: Option<(i64, i64)>) -> Result<Value> {
    let bad = |text: &str| bad_input(&CastType::Numeric(None), text);
    let text = match v {
        Value::Int(_) | Value::Float(_) | Value::Bool(_) if m.is_none() => return Ok(v.clone()),
        _ => input_text(v),
    };
    if is_special_number(&text) {
        return Ok(Value::Text(text.trim().to_string()));
    }
    if rat_from_string(&text).is_none() {
        return Err(bad(&text));
    }
    match m {
        None => Ok(Value::Text(text.trim().trim_start_matches('+').to_string())),
        Some((p, s)) => match round_numeric(&text, p, s)? {
            Some(rounded) => Ok(Value::Text(rounded)),
            None => Err(bad(&text)),
        },
    }
}

fn cast_bytea(v: &Value) -> Result<Value> {
    let text = match v {
        Value::Blob(b) => return Ok(Value::Blob(b.clone())),
        other => input_text(other),
    };
    let ty = CastType::Bytea;
    if let Some(digits) = text.strip_prefix("\\x") {
        return hex::decode(digits).map(Value::Blob).map_err(|e| {
            let message = match e {
                hex::FromHexError::InvalidHexCharacter { c, .. } => format!("invalid hexadecimal digit: \"{c}\""),
                _ => "invalid hexadecimal data: odd number of digits".to_string(),
            };
            Error::with_state(SqlState::INVALID_PARAMETER_VALUE, message)
        });
    }
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
        } else if b.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else if let [d1 @ b'0'..=b'3', d2 @ b'0'..=b'7', d3 @ b'0'..=b'7', ..] = b[i + 1..] {
            out.push((d1 - b'0') << 6 | (d2 - b'0') << 3 | (d3 - b'0'));
            i += 4;
        } else {
            return Err(bad_input(&ty, &text));
        }
    }
    Ok(Value::Blob(out))
}

fn normalize_uuid(text: &str) -> Option<String> {
    let s = text.trim();
    let s = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(s);
    let mut hex = String::with_capacity(32);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // A hyphen may follow any pair of digits past the first group of four.
        if b[i] == b'-' && !hex.is_empty() && hex.len().is_multiple_of(4) && hex.len() < 32 && i + 1 < b.len() {
            i += 1;
        }
        let pair = s.get(i..i + 2)?;
        if !pair.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        hex.push_str(&pair.to_ascii_lowercase());
        i += 2;
    }
    if hex.len() != 32 {
        return None;
    }
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

// --- dates and times --------------------------------------------------------------

/// What a date and time text says.
#[derive(Debug, Default)]
struct Stamp {
    date: Option<NaiveDate>,
    time: Option<NaiveTime>,
    /// Seconds east of UTC.
    offset: Option<i32>,
    /// A zone by name.
    zone: Option<String>,
    /// Added at the end: 24:00:00 and a fraction rounding up.
    carry: Duration,
}

impl Stamp {
    /// The moment this says, in the terms of the target: a timestamp ignores a zone,
    /// and a timestamptz, which is stored as bare UTC, applies it.
    fn instant(&self, ty: &CastType, text: &str) -> Result<NaiveDateTime> {
        let Some(date) = self.date else {
            return Err(bad_input(ty, text));
        };
        let wall = date.and_time(self.time.unwrap_or_default()) + self.carry;
        if !matches!(ty, CastType::TimestampTz(_)) {
            return Ok(wall);
        }
        if let Some(zone) = &self.zone {
            let converted = from_time_zone(&wall.format("%Y-%m-%d %H:%M:%S%.f").to_string(), zone)
                .and_then(|s| naive_stamp(s.trim_end_matches("+00")));
            return converted.ok_or_else(|| {
                Error::with_state(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("time zone \"{zone}\" not recognized"),
                )
            });
        }
        Ok(wall - Duration::seconds(i64::from(self.offset.unwrap_or(0))))
    }
}

#[derive(Debug)]
enum Parsed {
    Stamp(Stamp),
    /// infinity and -infinity, which have no calendar form.
    Special(String),
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn month_of(word: &str) -> Option<u32> {
    let w = word.to_ascii_lowercase();
    if w.len() < 3 {
        return None;
    }
    let full = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    let at = MONTHS
        .iter()
        .position(|m| w == *m)
        .or_else(|| full.iter().position(|m| *m == w));
    at.map(|i| i as u32 + 1).or((w == "sept").then_some(9))
}

fn is_day_name(word: &str) -> bool {
    let w = word.to_ascii_lowercase();
    w.len() >= 3 && DAYS.iter().any(|d| w.starts_with(d))
}

fn date_or_range(y: i32, m: u32, d: u32, text: &str) -> Result<NaiveDate> {
    NaiveDate::from_ymd_opt(y, m, d).ok_or_else(|| out_of_range(text))
}

/// Splits on what separates date and time fields, keeping signs with what follows.
fn stamp_fields(text: &str) -> Vec<&str> {
    let s = text.trim();
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut start = None;
    for i in 0..b.len() {
        let c = b[i];
        // `T` between digits is ISO's separator; inside a word it is a letter.
        let t_sep = (c == b'T' || c == b't')
            && i > 0
            && b[i - 1].is_ascii_digit()
            && b.get(i + 1).is_some_and(u8::is_ascii_digit);
        if c == b' ' || c == b'\t' || c == b'\n' || c == b',' || t_sep {
            out.extend(start.take().map(|p| &s[p..i]));
        } else if (c == b'+' || c == b'-' || c == b'z' || c == b'Z')
            && i > 0
            && b[i - 1].is_ascii_digit()
            && s[..i].rsplit([' ', 'T', 't']).next().is_some_and(|f| f.contains(':'))
        {
            // The zone written against the time: 03:04:05+02, 03:04:05Z.
            out.extend(start.take().map(|p| &s[p..i]));
            start = Some(i);
        } else if start.is_none() {
            start = Some(i);
        }
    }
    out.extend(start.map(|p| &s[p..]));
    out
}

/// Reads `H:M[:S[.f]]`.
fn parse_clock(field: &str, text: &str) -> Result<(NaiveTime, Duration)> {
    let mut parts = field.split(':');
    let num = |p: Option<&str>| -> Option<u32> { p?.parse().ok() };
    let h = num(parts.next());
    let m = num(parts.next());
    let sec = parts.next();
    let (s, frac) = match sec {
        None => (Some(0), ""),
        Some(sec) => {
            let (whole, frac) = sec.split_once('.').unwrap_or((sec, ""));
            (whole.parse().ok(), frac)
        }
    };
    let (Some(h), Some(m), Some(s)) = (h, m, s) else {
        return Err(Error::with_state(
            SqlState::INVALID_DATETIME_FORMAT,
            format!("invalid input syntax for type time: \"{text}\""),
        ));
    };
    if !frac.bytes().all(|c| c.is_ascii_digit()) || parts.next().is_some() {
        return Err(out_of_range(text));
    }
    let nanos: u32 = format!("{frac:0<9}").get(..9).and_then(|d| d.parse().ok()).unwrap_or(0);
    // Six digits, rounded, as the timestamp holds them.
    let micros = (nanos + 500) / 1000;
    if h > 24 || m > 59 || s > 60 || (h == 24 && (m > 0 || s > 0 || micros > 0)) {
        return Err(out_of_range(text));
    }
    let carry = Duration::hours(i64::from(h / 24) * 24)
        + Duration::seconds(i64::from(s / 60) * 60)
        + Duration::microseconds(i64::from(micros / 1_000_000));
    let t = NaiveTime::from_hms_micro_opt(h % 24, m, s % 60, micros % 1_000_000).ok_or_else(|| out_of_range(text))?;
    Ok((t, carry))
}

/// Reads the offset of text, which Postgres holds to 15:59:59 either way.
fn checked_offset(field: &str, ty: &CastType, text: &str) -> Result<i32> {
    match parse_offset(field) {
        Some(offset) if offset.abs() < 16 * 3600 => Ok(offset),
        Some(_) => Err(Error::with_state(
            SqlState::INVALID_TIME_ZONE_DISPLACEMENT_VALUE,
            format!("time zone displacement out of range: \"{text}\""),
        )),
        None => Err(bad_input(ty, text)),
    }
}

/// Reads an offset: `+02`, `-0300`, `+05:30`, `+05:30:15`, or None if it is not one.
fn parse_offset(field: &str) -> Option<i32> {
    let sign = match field.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &field[1..];
    let digits: Vec<&str> = if rest.contains(':') {
        rest.split(':').collect()
    } else if rest.len() > 2 {
        let cut = rest.len() - 2;
        if cut > 2 {
            vec![rest.get(..cut - 2)?, rest.get(cut - 2..cut)?, rest.get(cut..)?]
        } else {
            vec![rest.get(..cut)?, rest.get(cut..)?]
        }
    } else {
        vec![rest]
    };
    let mut total = 0;
    for (d, unit) in digits.iter().zip([3600, 60, 1]) {
        if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        total += d.parse::<i32>().ok()? * unit;
    }
    Some(sign * total)
}

/// Reads a date written with `-`, `/` or `.`, or all digits.
fn parse_date_field(field: &str, text: &str) -> Result<Option<NaiveDate>> {
    if field.bytes().all(|c| c.is_ascii_digit()) && field.len() == 8 {
        let n = |r: std::ops::Range<usize>| field[r].parse().unwrap_or(0);
        return date_or_range(n(0..4), n(4..6) as u32, n(6..8) as u32, text).map(Some);
    }
    let parts: Vec<&str> = field.split(['-', '/', '.']).collect();
    let [a, b, c] = parts[..] else {
        return Ok(None);
    };
    if !a.bytes().all(|x| x.is_ascii_digit()) || a.is_empty() {
        // 02-Jan-2024's month stands second; Jan-02-2024's stands first.
        return match (month_of(a), b.parse(), c.parse()) {
            (Some(m), Ok(d), Ok(y)) => date_or_range(y, m, d, text).map(Some),
            _ => Ok(None),
        };
    }
    if let Some(m) = month_of(b) {
        return match (a.parse::<i32>(), c.parse::<i32>()) {
            (Ok(y), Ok(d)) if a.len() >= 3 => date_or_range(y, m, d as u32, text).map(Some),
            (Ok(d), Ok(y)) => date_or_range(y, m, d as u32, text).map(Some),
            _ => Ok(None),
        };
    }
    let (Ok(x), Ok(y), Ok(z)) = (a.parse::<i64>(), b.parse::<i64>(), c.parse::<i64>()) else {
        return Ok(None);
    };
    let (year, month, day) = if a.len() >= 3 {
        (x, y, z)
    } else {
        // Month first, as DateStyle's MDY has it.
        (
            if z < 100 {
                2000 + z - i64::from(z >= 70) * 100
            } else {
                z
            },
            x,
            y,
        )
    };
    date_or_range(year as i32, month as u32, day as u32, text).map(Some)
}

fn date_text_of(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.clone()),
        Value::Blob(b) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

/// The text of a value bound for a date or time type. A number is not one: Postgres
/// has no such cast.
fn stamp_text(v: &Value, ty: &CastType) -> Result<String> {
    date_text_of(v).ok_or_else(|| {
        let from = match v {
            Value::Float(_) => "numeric",
            _ => "integer",
        };
        Error::with_state(
            SqlState::CANNOT_COERCE,
            format!("cannot cast type {from} to {}", ty.display()),
        )
    })
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

/// Reads a date, time and zone text in the forms datetime_in reads.
fn parse_stamp(text: &str, ty: &CastType) -> Result<Parsed> {
    let trimmed = text.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "infinity" | "+infinity" => return Ok(Parsed::Special("infinity".to_string())),
        "-infinity" => return Ok(Parsed::Special("-infinity".to_string())),
        "epoch" => {
            return Ok(Parsed::Stamp(Stamp {
                date: NaiveDate::from_ymd_opt(1970, 1, 1),
                ..Default::default()
            }));
        }
        "now" => {
            let now = Utc::now().naive_utc();
            return Ok(Parsed::Stamp(Stamp {
                date: Some(now.date()),
                time: Some(now.time()),
                ..Default::default()
            }));
        }
        word @ ("today" | "tomorrow" | "yesterday") => {
            let days = i64::from(word == "tomorrow") - i64::from(word == "yesterday");
            return Ok(Parsed::Stamp(Stamp {
                date: Some(today() + Duration::days(days)),
                ..Default::default()
            }));
        }
        _ => {}
    }
    let mut s = Stamp::default();
    let mut words: Vec<&str> = Vec::new();
    let mut pm: Option<bool> = None;
    for field in stamp_fields(trimmed) {
        let low = field.to_ascii_lowercase();
        if field.contains(':') && !field.starts_with(['+', '-']) {
            let (t, carry) = parse_clock(field, text)?;
            s.time = Some(t);
            s.carry = carry;
        } else if s.date.is_none()
            && !field.starts_with('+')
            && let Some(d) = parse_date_field(field, text)?
        {
            s.date = Some(d);
        } else if s.time.is_some() && (field.starts_with('+') || field.starts_with('-')) {
            s.offset = Some(checked_offset(field, ty, text)?);
        } else if matches!(low.as_str(), "z" | "zulu" | "utc" | "gmt" | "ut") {
            s.offset = Some(0);
        } else if low == "am" || low == "pm" {
            pm = Some(low == "pm");
        } else if is_day_name(field) && month_of(field).is_none() {
            // A weekday's name says nothing a date does not.
        } else {
            words.push(field);
        }
    }
    if let Some(pm) = pm {
        let t = s.time.ok_or_else(|| bad_input(ty, text))?;
        let hour = t.hour() % 12 + if pm { 12 } else { 0 };
        s.time = t.with_hour(hour);
    }
    // What is left spells a month with its day and year, or a zone.
    let mut month = None;
    let mut numbers = Vec::new();
    for w in &words {
        if let Some(m) = month_of(w) {
            month = Some(m);
        } else if let Ok(n) = w.trim_end_matches(|c: char| c.is_ascii_alphabetic()).parse::<i32>()
            && !w.is_empty()
        {
            numbers.push((n, w.len()));
        } else if s.zone.is_none() && w.contains('/') {
            s.zone = Some((*w).to_string());
        } else {
            return Err(bad_input(ty, text));
        }
    }
    if let Some(m) = month {
        let (day, year) = match numbers[..] {
            // The longer number is the year: 2024 Jan 2 as much as Jan 2 2024.
            [(a, la), (b, _)] if la > 2 => (b, a),
            [(a, _), (b, _)] => (a, b),
            _ => return Err(bad_input(ty, text)),
        };
        s.date = Some(date_or_range(year, m, day as u32, text)?);
    } else if !numbers.is_empty() {
        return Err(bad_input(ty, text));
    }
    if s.date.is_none() {
        return Err(bad_input(ty, text));
    }
    Ok(Parsed::Stamp(s))
}

/// A time of day and the zone it was written with.
struct TimeOfDay {
    time: NaiveTime,
    offset: Option<i32>,
}

/// Reads a time, which a whole timestamp also is.
fn parse_time_of_day(text: &str, ty: &CastType) -> Result<TimeOfDay> {
    let trimmed = text.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "allballs" => {
            return Ok(TimeOfDay {
                time: NaiveTime::default(),
                offset: Some(0),
            });
        }
        "now" => {
            return Ok(TimeOfDay {
                time: Utc::now().time(),
                offset: Some(0),
            });
        }
        _ => {}
    }
    let (mut time, mut offset, mut pm) = (None, None, None);
    for field in stamp_fields(trimmed) {
        let low = field.to_ascii_lowercase();
        if field.contains(':') && !field.starts_with(['+', '-']) {
            let (t, carry) = parse_clock(field, text)?;
            // 24:00:00 stays itself as a time, and has no other form here.
            time = Some(if carry.is_zero() { t } else { t + carry });
        } else if time.is_some() && (field.starts_with('+') || field.starts_with('-')) {
            offset = Some(checked_offset(field, ty, text)?);
        } else if matches!(low.as_str(), "z" | "utc" | "gmt") {
            offset = Some(0);
        } else if low == "am" || low == "pm" {
            pm = Some(low == "pm");
        } else if time.is_none() && parse_date_field(field, text)?.is_some() {
            // The date of a timestamp read as a time.
        } else {
            return Err(bad_input(ty, text));
        }
    }
    let Some(mut time) = time else {
        return Err(bad_input(ty, text));
    };
    if let Some(pm) = pm {
        let hour = time.hour() % 12 + if pm { 12 } else { 0 };
        time = time.with_hour(hour).unwrap_or(time);
    }
    Ok(TimeOfDay { time, offset })
}

/// Keeps `precision` fractional digits, rounding; the rest of the fraction is cut to
/// six digits and its trailing zeros go, as Postgres prints it.
fn fraction(micros: u32) -> String {
    if micros == 0 {
        return String::new();
    }
    format!(".{micros:06}").trim_end_matches('0').to_string()
}

fn round_micros(micros: u32, precision: Option<u32>) -> i64 {
    let unit = 10i64.pow(6 - precision.unwrap_or(6).min(6));
    ((i64::from(micros) + unit / 2) / unit) * unit - i64::from(micros)
}

pub(crate) fn format_stamp(t: NaiveDateTime, precision: Option<u32>) -> String {
    let t = t + Duration::microseconds(round_micros(t.nanosecond() / 1000, precision));
    // chrono writes a year past 9999 with a sign, which Postgres does not.
    let text = t.format("%Y-%m-%d %H:%M:%S").to_string();
    text.strip_prefix('+').unwrap_or(&text).to_string() + &fraction(t.nanosecond() / 1000)
}

/// A timestamp's stored text (`YYYY-MM-DD HH:MM:SS[.f]`) as chrono reads it, a year
/// past 9999 included, which chrono reads only with a sign.
pub(crate) fn naive_stamp(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .or_else(|| {
            let year = s.bytes().take_while(u8::is_ascii_digit).count();
            (year > 4)
                .then(|| NaiveDateTime::parse_from_str(&["+", s].concat(), "%Y-%m-%d %H:%M:%S%.f").ok())
                .flatten()
        })
}

pub(crate) fn format_time(t: NaiveTime, precision: Option<u32>) -> String {
    let (t, _) = t.overflowing_add_signed(Duration::microseconds(round_micros(t.nanosecond() / 1000, precision)));
    t.format("%H:%M:%S").to_string() + &fraction(t.nanosecond() / 1000)
}

fn format_offset(offset: i32) -> String {
    let (sign, a) = if offset < 0 { ('-', -offset) } else { ('+', offset) };
    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
    match (m, s) {
        (0, 0) => format!("{sign}{h:02}"),
        (_, 0) => format!("{sign}{h:02}:{m:02}"),
        _ => format!("{sign}{h:02}:{m:02}:{s:02}"),
    }
}

/// An interval, in the three parts Postgres keeps apart.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Interval {
    pub(crate) months: i64,
    pub(crate) days: i64,
    pub(crate) micros: i64,
}

impl std::fmt::Display for Interval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        let mut unit = |n: i64, name: &str| {
            if n != 0 {
                let plural = if n == 1 { "" } else { "s" };
                parts.push(format!("{n} {name}{plural}"));
            }
        };
        unit(self.months / 12, "year");
        unit(self.months % 12, "mon");
        unit(self.days, "day");
        if self.micros != 0 || parts.is_empty() {
            let a = self.micros.unsigned_abs();
            let secs = a / 1_000_000;
            let sign = if self.micros < 0 { "-" } else { "" };
            let frac = fraction((a % 1_000_000) as u32);
            parts.push(format!(
                "{sign}{:02}:{:02}:{:02}{frac}",
                secs / 3600,
                secs / 60 % 60,
                secs % 60
            ));
        }
        f.write_str(&parts.join(" "))
    }
}

/// Reads an interval in Postgres' verbose form, `@` and `ago` included, with a clock
/// among its fields. The ISO 8601 form is refused, as a syntax this does not read.
/// An interval's months, days and microseconds, as interval_send gives them, for text
/// interval input reads.
pub fn interval_parts(text: &str) -> Option<(i64, i64, i64)> {
    parse_interval(text).map(|i| (i.months, i.days, i.micros))
}

pub(crate) fn parse_interval(text: &str) -> Option<Interval> {
    let mut out = Interval::default();
    let mut toks: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        // `1day` and `1 day` are one thing.
        let at = word.find(|c: char| c.is_ascii_alphabetic()).filter(|i| *i > 0);
        match at {
            Some(i) if !word.contains(':') => {
                toks.push(word[..i].to_string());
                toks.push(word[i..].to_string());
            }
            _ => toks.push(word.to_string()),
        }
    }
    if toks.first().is_some_and(|t| t == "@") {
        toks.remove(0);
    }
    let ago = toks.last().is_some_and(|t| t.eq_ignore_ascii_case("ago"));
    if ago {
        toks.pop();
    }
    if toks.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if t.contains(':') {
            let neg = t.starts_with('-');
            let body = t.trim_start_matches(['+', '-']);
            let mut f = body.split(':');
            let h: i64 = f.next()?.parse().ok()?;
            let m: i64 = f.next()?.parse().ok()?;
            let (s, frac) = f
                .next()
                .unwrap_or("0")
                .split_once('.')
                .map_or((f64::NAN, 0.0), |(s, r)| {
                    (s.parse().unwrap_or(f64::NAN), format!("0.{r}").parse().unwrap_or(0.0))
                });
            let secs: f64 = if s.is_nan() {
                body.split(':').nth(2).unwrap_or("0").parse().ok()?
            } else {
                s + frac
            };
            let micros = (h * 3600 + m * 60) * 1_000_000 + (secs * 1e6).round() as i64;
            out.micros += if neg { -micros } else { micros };
            i += 1;
            continue;
        }
        let n: f64 = t.parse().ok()?;
        let unit = toks.get(i + 1).map(|u| u.to_ascii_lowercase());
        let singular = unit
            .as_deref()
            .map(|u| if u == "ms" { u } else { u.strip_suffix('s').unwrap_or(u) });
        let (months, days, micros) = match singular {
            Some("microsecond" | "u" | "us" | "usec" | "usecond" | "microsec") => (0.0, 0.0, 1.0),
            Some("ms" | "msec" | "msecond" | "millisec" | "millisecond") => (0.0, 0.0, 1e3),
            Some("second" | "sec" | "s") => (0.0, 0.0, 1e6),
            Some("minute" | "min" | "m") => (0.0, 0.0, 6e7),
            Some("hour" | "hr" | "h") => (0.0, 0.0, 3.6e9),
            Some("day" | "d") => (0.0, 1.0, 0.0),
            Some("week" | "w") => (0.0, 7.0, 0.0),
            Some("month" | "mon") => (1.0, 0.0, 0.0),
            Some("year" | "yr" | "y") => (12.0, 0.0, 0.0),
            Some("decade" | "dec") => (120.0, 0.0, 0.0),
            Some("century" | "cent" | "c") => (1200.0, 0.0, 0.0),
            Some("millennium" | "mil" | "millenium") => (12000.0, 0.0, 0.0),
            // A bare number counts seconds.
            None => (0.0, 0.0, 1e6),
            Some(_) => return None,
        };
        out.months += (n * months).trunc() as i64;
        out.days += (n * days).trunc() as i64;
        out.micros += (n * micros).round() as i64 + (n.fract() * days * 8.64e10).round() as i64;
        i += 2;
    }
    if ago {
        out = Interval {
            months: -out.months,
            days: -out.days,
            micros: -out.micros,
        };
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_cast(input: &str, ty: &str) -> std::result::Result<String, String> {
        let ty = CastType::parse(ty).unwrap();
        match cast(&Value::Text(input.to_string()), &ty) {
            Ok(v) => Ok(v.to_string()),
            Err(e) => Err(e.sql_state().as_str().to_string()),
        }
    }

    #[test]
    fn types_by_name() {
        for (name, want) in [
            ("timestamp(3) with time zone", CastType::TimestampTz(Some(3))),
            ("TIMESTAMP WITHOUT TIME ZONE", CastType::Timestamp(None)),
            ("numeric(10, 2)", CastType::Numeric(Some((10, 2)))),
            ("numeric(4)", CastType::Numeric(Some((4, 0)))),
            ("character varying(3)", CastType::Varchar(Some(3))),
            ("double precision", CastType::Float8),
            ("pg_catalog.int4", CastType::Int4),
            ("char", CastType::Char(1)),
        ] {
            assert_eq!(CastType::parse(name), Some(want), "{name}");
        }
        assert_eq!(CastType::parse("nosuchtype"), None);
    }

    // Each answer was read off Postgres 17.
    #[test]
    fn date_and_time_input() {
        for (input, ty, want) in [
            ("2024-01-02 03:04:05", "timestamp", "2024-01-02 03:04:05"),
            ("2024-01-02T03:04:05.5", "timestamp", "2024-01-02 03:04:05.5"),
            ("2024-01-02", "timestamp", "2024-01-02 00:00:00"),
            ("2024-01-02 03:04:05+02", "timestamp", "2024-01-02 03:04:05"),
            ("2024-01-02 03:04:05+02", "timestamptz", "2024-01-02 01:04:05"),
            ("2024-01-02 03:04:05-0330", "timestamptz", "2024-01-02 06:34:05"),
            ("2024-01-02 03:04:05Z", "timestamptz", "2024-01-02 03:04:05"),
            ("2024-1-2", "date", "2024-01-02"),
            ("Jan 2 2024", "date", "2024-01-02"),
            ("January 2, 2024", "date", "2024-01-02"),
            ("2 Jan 2024", "date", "2024-01-02"),
            ("01/02/2024", "date", "2024-01-02"),
            ("2024-01-02 03:04:05", "date", "2024-01-02"),
            ("20240102", "date", "2024-01-02"),
            ("infinity", "timestamp", "infinity"),
            ("-infinity", "date", "-infinity"),
            ("epoch", "timestamp", "1970-01-01 00:00:00"),
            ("03:04", "time", "03:04:00"),
            ("03:04:05.123", "time", "03:04:05.123"),
            ("3:04 PM", "time", "15:04:00"),
            ("2024-01-02 03:04:05", "time", "03:04:05"),
            ("03:04:05+02", "timetz", "03:04:05+02"),
            ("2024-01-02 03:04:05.678", "timestamp(1)", "2024-01-02 03:04:05.7"),
            ("2024-01-02 03:04:05.5", "timestamp(0)", "2024-01-02 03:04:06"),
            ("2024-01-02 24:00:00", "timestamp", "2024-01-03 00:00:00"),
            ("2024-01-02 23:59:60", "timestamp", "2024-01-03 00:00:00"),
        ] {
            assert_eq!(text_cast(input, ty).as_deref(), Ok(want), "{input} {ty}");
        }
        for (input, ty, state) in [
            ("abc", "timestamp", "22007"),
            ("abc", "date", "22007"),
            ("abc", "time", "22007"),
            ("", "date", "22007"),
            ("2024-13-01", "date", "22008"),
            ("2024-02-30", "date", "22008"),
            ("25:00:00", "time", "22008"),
            ("2024-01-01 12:00:00+16:00", "timestamptz", "22009"),
            ("2024-01-01 12:00:00+24", "timestamp", "22009"),
        ] {
            assert_eq!(
                text_cast(input, ty).as_ref().map_err(String::as_str),
                Err(state),
                "{input} {ty}"
            );
        }
    }

    #[test]
    fn intervals() {
        for (input, want) in [
            ("1 day", "1 day"),
            ("1 day 02:00:00", "1 day 02:00:00"),
            ("1 year 2 mons", "1 year 2 mons"),
            ("14 months", "1 year 2 mons"),
            ("2 weeks", "14 days"),
            ("90 minutes", "01:30:00"),
            ("1.5 hours", "01:30:00"),
            ("@ 1 day ago", "-1 days"),
            ("-1 days", "-1 days"),
            ("00:00:00", "00:00:00"),
            ("1d", "1 day"),
            ("0.5 seconds", "00:00:00.5"),
        ] {
            assert_eq!(text_cast(input, "interval").as_deref(), Ok(want), "{input}");
        }
        assert_eq!(text_cast("abc", "interval").unwrap_err(), "22007");
    }

    #[test]
    fn numbers() {
        for (input, ty, want) in [
            ("12", "int4", "12"),
            (" 12 ", "int4", "12"),
            ("-1", "int2", "-1"),
            ("0x1F", "int4", "31"),
            ("1_000", "int4", "1000"),
            ("1.5", "float8", "1.5"),
            ("NaN", "float8", "NaN"),
            ("1.2345", "numeric(10,2)", "1.23"),
            ("1.235", "numeric(10,2)", "1.24"),
            ("-1.235", "numeric(10,2)", "-1.24"),
            ("1.5", "numeric(10,2)", "1.50"),
            ("999.4", "numeric(3)", "999"),
        ] {
            assert_eq!(text_cast(input, ty).as_deref(), Ok(want), "{input} {ty}");
        }
        for (input, ty, state) in [
            ("abc", "int4", "22P02"),
            ("1.5", "int4", "22P02"),
            ("", "int4", "22P02"),
            ("99999999999", "int4", "22003"),
            ("40000", "int2", "22003"),
            ("99999999999999999999", "int8", "22003"),
            ("abc", "float8", "22P02"),
            ("abc", "numeric", "22P02"),
            ("1000", "numeric(3,1)", "22003"),
            ("999.5", "numeric(3)", "22003"),
        ] {
            assert_eq!(
                text_cast(input, ty).as_ref().map_err(String::as_str),
                Err(state),
                "{input} {ty}"
            );
        }
    }

    #[test]
    fn text_types() {
        assert_eq!(
            text_cast("123E4567-E89B-12D3-A456-426614174000", "uuid").as_deref(),
            Ok("123e4567-e89b-12d3-a456-426614174000")
        );
        assert_eq!(
            text_cast("{a0eebc999c0b4ef8bb6d6bb9bd380a11}", "uuid").as_deref(),
            Ok("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")
        );
        assert_eq!(text_cast("abc", "uuid").unwrap_err(), "22P02");
        assert_eq!(text_cast("{\"a\":1}", "jsonb").as_deref(), Ok("{\"a\":1}"));
        assert_eq!(text_cast("[1", "jsonb").unwrap_err(), "22P02");
        assert_eq!(text_cast("abc", "varchar(2)").as_deref(), Ok("ab"));
        assert_eq!(text_cast("a", "char(3)").as_deref(), Ok("a  "));
        assert_eq!(text_cast("abc", "char").as_deref(), Ok("a"));
    }

    #[test]
    fn numbers_are_no_dates() {
        let ty = CastType::parse("date").unwrap();
        assert_eq!(cast(&Value::Int(1), &ty).unwrap_err().sql_state().as_str(), "42846");
    }
}
