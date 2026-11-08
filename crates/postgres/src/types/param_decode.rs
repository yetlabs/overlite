//! Binary parameter decoding by type.

use crate::session::new_reader;
use crate::types::array::is_array_oid;
use crate::types::param_types::{OID_NAME, OID_NAME_ARR, OID_OID, OID_OID_ARR};
use crate::types::{
    OID_BOOL, OID_BPCHAR, OID_BYTEA, OID_DATE, OID_FLOAT8, OID_INT2, OID_INT4, OID_INT8, OID_JSON, OID_JSONB, OID_TEXT,
    OID_TIMESTAMP, OID_TIMESTAMPTZ, OID_UUID, OID_VARCHAR,
};
use base64::Engine;
use overlite_core::text::text_from_bytes;
use overlite_core::{Oid, Value};

const OID_UNKNOWN: Oid = Oid::new(705);
const OID_FLOAT4: Oid = Oid::new(700);
const OID_TIME: Oid = Oid::new(1083);
const OID_TIMETZ: Oid = Oid::new(1266);
const OID_INTERVAL: Oid = Oid::new(1186);
/// The OID this server gives hstore (types/mod.rs).
const OID_HSTORE: Oid = Oid::new(16400);

/// `HH:MM:SS[.ffffff]` of a microsecond count from midnight, as time_send gives it.
fn format_pg_time(us: i64) -> String {
    let secs = us.div_euclid(1_000_000);
    let frac = us.rem_euclid(1_000_000);
    let mut out = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if frac != 0 {
        out.push('.');
        out.push_str(format!("{frac:06}").trim_end_matches('0'));
    }
    out
}

/// The text of an interval_send: microseconds, days and months, which interval input
/// reads back to the same interval.
fn format_pg_interval(us: i64, days: i32, months: i32) -> String {
    let sign = if us < 0 { "-" } else { "" };
    format!(
        "{months} mons {days} days {sign}{}",
        format_pg_time(us.unsigned_abs() as i64)
    )
}

/// hstore_send's pairs as the JSON object an hstore is stored as.
fn decode_hstore(raw: &[u8]) -> Option<Value> {
    let mut r = new_reader(raw);
    let n = r.int32();
    let mut pairs = Vec::new();
    for _ in 0..n.max(0) {
        let kl = r.int32();
        let key = text_from_bytes(r.bytes(kl)).into_owned();
        let vl = r.int32();
        let value = (vl >= 0).then(|| text_from_bytes(r.bytes(vl)).into_owned());
        pairs.push((key, value));
    }
    if r.err {
        return None;
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("{");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        append_json_string(&mut out, k);
        out.push(':');
        match v {
            Some(v) => append_json_string(&mut out, v),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    Some(Value::Text(out))
}

/// Days from 1970-01-01 to the Postgres epoch, 2000-01-01.
const PG_EPOCH_DAYS: i64 = 10957;

/// The proleptic Gregorian (year, month, day) of a day count from 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `YYYY-MM-DD` of a day count from the Postgres epoch. By hand because an int4
/// day count reaches past chrono's range.
fn format_pg_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days + PG_EPOCH_DAYS);
    if y < 0 {
        return format!("-{:04}-{:02}-{:02}", -y, m, d);
    }
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MM:SS.ffffff` of a microsecond count from the Postgres epoch.
/// The product in nanoseconds wraps at the i64 range.
fn format_pg_timestamp(us: i64) -> String {
    let ns = us.wrapping_mul(1000);
    let secs = ns.div_euclid(1_000_000_000);
    let frac_us = ns.rem_euclid(1_000_000_000) / 1000;
    let day_secs = secs.rem_euclid(86400);
    let mut out = format_pg_date(secs.div_euclid(86400));
    out.push_str(&format!(
        " {:02}:{:02}:{:02}",
        day_secs / 3600,
        day_secs / 60 % 60,
        day_secs % 60
    ));
    if frac_us != 0 {
        let frac = format!("{frac_us:06}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out
}

/// Reads a binary parameter by its type, into the value the same parameter sent
/// as text would have stored. None for a type it does not know, which is then
/// guessed by its length.
pub(crate) fn decode_typed_param(oid: Oid, raw: &[u8]) -> Option<Value> {
    match oid {
        OID_BYTEA => return Some(Value::Blob(raw.to_vec())),
        OID_TEXT | OID_VARCHAR | OID_BPCHAR | OID_NAME | OID_UNKNOWN | OID_JSON => {
            return Some(Value::Text(text_from_bytes(raw).into_owned()));
        }
        OID_JSONB => {
            if !raw.is_empty() && raw[0] == 1 {
                // the jsonb format version
                return Some(Value::Text(text_from_bytes(&raw[1..]).into_owned()));
            }
        }
        OID_BOOL => {
            if raw.len() == 1 {
                return Some(Value::Bool(raw[0] != 0));
            }
        }
        OID_INT2 => {
            if raw.len() == 2 {
                return Some(Value::Int(i16::from_be_bytes([raw[0], raw[1]]) as i64));
            }
        }
        OID_INT4 => {
            if raw.len() == 4 {
                return Some(Value::Int(i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64));
            }
        }
        OID_OID => {
            if raw.len() == 4 {
                return Some(Value::Int(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64));
            }
        }
        OID_INT8 => {
            if let Ok(b) = raw.try_into() {
                return Some(Value::Int(i64::from_be_bytes(b)));
            }
        }
        OID_FLOAT4 => {
            if raw.len() == 4 {
                return Some(Value::Float(f32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as f64));
            }
        }
        OID_FLOAT8 => {
            if let Ok(b) = raw.try_into() {
                return Some(Value::Float(f64::from_be_bytes(b)));
            }
        }
        OID_TIMESTAMP | OID_TIMESTAMPTZ => {
            // timestamptz is the instant in UTC, stored bare as a literal with a
            // zone would be; timestamp is the wall clock as given.
            if let Ok(b) = raw.try_into() {
                let us = i64::from_be_bytes(b);
                return Some(Value::Text(match us {
                    i64::MAX => "infinity".to_string(),
                    i64::MIN => "-infinity".to_string(),
                    _ => format_pg_timestamp(us),
                }));
            }
        }
        OID_DATE => {
            if raw.len() == 4 {
                let days = i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64;
                return Some(Value::Text(format_pg_date(days)));
            }
        }
        OID_TIME => {
            if let Ok(b) = raw.try_into() {
                return Some(Value::Text(format_pg_time(i64::from_be_bytes(b))));
            }
        }
        OID_TIMETZ => {
            if raw.len() == 12 {
                let us = i64::from_be_bytes(raw[..8].try_into().ok()?);
                // The zone in seconds west of UTC.
                let east = -i32::from_be_bytes(raw[8..].try_into().ok()?);
                let (sign, z) = if east < 0 { ('-', -east) } else { ('+', east) };
                let mut zone = format!("{sign}{:02}", z / 3600);
                if z % 3600 != 0 {
                    zone.push_str(&format!(":{:02}", z / 60 % 60));
                }
                if z % 60 != 0 {
                    zone.push_str(&format!(":{:02}", z % 60));
                }
                return Some(Value::Text(format_pg_time(us) + &zone));
            }
        }
        OID_INTERVAL => {
            if raw.len() == 16 {
                let us = i64::from_be_bytes(raw[..8].try_into().ok()?);
                let days = i32::from_be_bytes(raw[8..12].try_into().ok()?);
                let months = i32::from_be_bytes(raw[12..].try_into().ok()?);
                return Some(Value::Text(format_pg_interval(us, days, months)));
            }
        }
        OID_HSTORE => return decode_hstore(raw),
        OID_UUID => {
            if raw.len() == 16 {
                return Some(Value::Text(format!(
                    "{}-{}-{}-{}-{}",
                    hex::encode(&raw[0..4]),
                    hex::encode(&raw[4..6]),
                    hex::encode(&raw[6..8]),
                    hex::encode(&raw[8..10]),
                    hex::encode(&raw[10..16])
                )));
            }
        }
        _ => {
            if is_array_oid(oid) || oid == OID_NAME_ARR || oid == OID_OID_ARR {
                return decode_array_param(raw);
            }
        }
    }
    None
}

pub(crate) fn bytea_hex(s: &str) -> Option<Vec<u8>> {
    let digits = s.strip_prefix(r"\x")?;
    hex::decode(digits).ok()
}

/// A JSON string, HTML escapes included.
fn append_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", ch as u32)),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A float as a JSON number; None for NaN and the infinities, which JSON lacks.
fn json_float(f: f64) -> Option<String> {
    if f.is_nan() || f.is_infinite() {
        return None;
    }
    let abs = f.abs();
    if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) {
        let sci = format!("{f:e}");
        let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
        if exp.starts_with('-') {
            return Some(format!("{mant}e{exp}"));
        }
        return Some(format!("{mant}e+{exp}"));
    }
    Some(format!("{f}"))
}

/// Reads a one-dimensional binary array into the JSON array an array is stored as.
fn decode_array_param(raw: &[u8]) -> Option<Value> {
    let mut r = new_reader(raw);
    let ndim = r.int32();
    r.int32(); // has-nulls flag
    let elem = Oid::new(r.int32() as u32);
    if r.err || ndim > 1 {
        return None;
    }
    let mut n = 0;
    if ndim == 1 {
        n = r.int32();
        r.int32(); // lower bound
    }
    let mut out = String::from("[");
    let mut i = 0;
    while i < n {
        if i > 0 {
            out.push(',');
        }
        i += 1;
        let length = r.int32();
        if length == -1 {
            out.push_str("null");
            continue;
        }
        match decode_typed_param(elem, r.bytes(length))? {
            Value::Null => out.push_str("null"),
            Value::Int(v) => out.push_str(&v.to_string()),
            Value::Float(v) => out.push_str(&json_float(v)?),
            Value::Text(v) => append_json_string(&mut out, &v),
            Value::Blob(v) => {
                out.push('"');
                out.push_str(&base64::engine::general_purpose::STANDARD.encode(v));
                out.push('"');
            }
            Value::Bool(v) => out.push_str(if v { "true" } else { "false" }),
            Value::Time(v) => append_json_string(&mut out, &v.to_rfc3339()),
        }
    }
    if r.err {
        return None;
    }
    out.push(']');
    Some(Value::Text(out))
}
