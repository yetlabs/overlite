//! Postgres 18's UUID functions: uuidv7() and the readers of a UUID's version and
//! time. uuidv4() is gen_random_uuid() by another name.

use crate::builtins::ranges::arg_str;
use crate::error::{Result, errf};
use chrono::{DateTime, Months, TimeDelta, Utc};
use overlite_core::{SqlState, Value};
use rand::RngCore;
use std::sync::Mutex;

/// The last uuidv7 timestamp given, in units of 1/4096 ms: Postgres keeps a UUID made
/// later in a backend greater than the one before, even within one millisecond, by
/// spending rand_a's 12 bits on the fraction of the millisecond and stepping past the
/// last one when the clock has not moved on.
static LAST_V7: Mutex<u64> = Mutex::new(0);

fn hyphenate(b: &[u8; 16]) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&b[0..4]),
        hex::encode(&b[4..6]),
        hex::encode(&b[6..8]),
        hex::encode(&b[8..10]),
        hex::encode(&b[10..16])
    )
}

/// The instant uuidv7 stamps: now, moved by the interval it was given.
fn shifted_now(shift: Option<&Value>) -> Result<DateTime<Utc>> {
    let now = Utc::now();
    let Some(v) = shift.filter(|v| !v.is_null()) else {
        return Ok(now);
    };
    let text = arg_str(v);
    let iv = crate::builtins::casts::parse_interval(&text).ok_or_else(|| {
        errf!(
            SqlState::INVALID_DATETIME_FORMAT,
            "invalid input syntax for type interval: \"{text}\""
        )
    })?;
    let months = u32::try_from(iv.months.unsigned_abs()).unwrap_or(u32::MAX);
    let moved = if iv.months >= 0 {
        now.checked_add_months(Months::new(months))
    } else {
        now.checked_sub_months(Months::new(months))
    };
    moved
        .and_then(|t| t.checked_add_signed(TimeDelta::days(iv.days)))
        .and_then(|t| t.checked_add_signed(TimeDelta::microseconds(iv.micros)))
        .filter(|t| t.timestamp_millis() >= 0 && t.timestamp_millis() < (1 << 48))
        .ok_or_else(|| errf!(SqlState::DATETIME_FIELD_OVERFLOW, "timestamp out of range"))
}

/// uuidv7([shift interval]): a version 7 UUID, its first 48 bits the Unix time in
/// milliseconds, the next 12 the fraction of the millisecond, the rest random.
pub(crate) fn uuidv7(a: &[Value]) -> Result<Value> {
    let t = shifted_now(a.first())?;
    let nanos_in_ms = u64::from(t.timestamp_subsec_nanos() % 1_000_000);
    let ms = u64::try_from(t.timestamp_millis()).unwrap_or(0);
    let mut stamp = (ms << 12) | (nanos_in_ms * 4096 / 1_000_000);
    if a.first().is_none_or(Value::is_null) {
        let mut last = LAST_V7.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if stamp <= *last {
            stamp = *last + 1;
        }
        *last = stamp;
    }
    let mut b = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut b)
        .map_err(|e| errf!(SqlState::INTERNAL_ERROR, "{e}"))?;
    let ms = stamp >> 12;
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    let frac = (stamp & 0xfff) as u16;
    b[6] = 0x70 | (frac >> 8) as u8;
    b[7] = (frac & 0xff) as u8;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(Value::Text(hyphenate(&b)))
}

/// A UUID's 16 bytes, in any spelling uuid_in takes; None when the text is no UUID.
fn uuid_bytes(v: &Value) -> Option<[u8; 16]> {
    let s: String = arg_str(v).chars().filter(|c| *c != '-' && *c != '{' && *c != '}').collect();
    let raw = hex::decode(s.trim()).ok()?;
    raw.try_into().ok()
}

/// uuid_extract_version(uuid): the version of an RFC 9562 UUID, NULL for another variant.
pub(crate) fn uuid_extract_version(a: &[Value]) -> Value {
    match a.first().filter(|v| !v.is_null()).and_then(uuid_bytes) {
        Some(b) if b[8] & 0xc0 == 0x80 => Value::Int(i64::from(b[6] >> 4)),
        _ => Value::Null,
    }
}

/// uuid_extract_timestamp(uuid): when a version 1 or 7 UUID was made, NULL for any other.
pub(crate) fn uuid_extract_timestamp(a: &[Value]) -> Value {
    let Some(b) = a.first().filter(|v| !v.is_null()).and_then(uuid_bytes) else {
        return Value::Null;
    };
    if b[8] & 0xc0 != 0x80 {
        return Value::Null;
    }
    let micros = match b[6] >> 4 {
        7 => {
            let mut ms = [0u8; 8];
            ms[2..].copy_from_slice(&b[..6]);
            i64::from_be_bytes(ms) * 1000
        }
        1 => {
            // 100ns ticks since 1582-10-15, time_low, time_mid and time_hi spread out.
            let low = u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
            let mid = u64::from(u16::from_be_bytes([b[4], b[5]]));
            let hi = u64::from(u16::from_be_bytes([b[6], b[7]]) & 0x0fff);
            let ticks = (hi << 48) | (mid << 32) | low;
            i64::try_from(ticks / 10).unwrap_or(i64::MAX) - 12_219_292_800_000_000
        }
        _ => return Value::Null,
    };
    match DateTime::<Utc>::from_timestamp_micros(micros) {
        Some(t) => Value::Text(t.format("%Y-%m-%d %H:%M:%S%.6f").to_string()),
        None => Value::Null,
    }
}

/// CRC-32 with the reflected polynomial given: crc32 (0xEDB88320) and crc32c (0x82F63B78).
fn crc(data: &[u8], poly: u32) -> i64 {
    let mut c = !0u32;
    for &byte in data {
        c ^= u32::from(byte);
        for _ in 0..8 {
            c = if c & 1 == 1 { (c >> 1) ^ poly } else { c >> 1 };
        }
    }
    i64::from(!c)
}

/// The bytes a bytea argument holds: a blob as it is, text in bytea's hex or escape form.
fn bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Blob(b) => b.clone(),
        other => {
            let s = arg_str(other);
            s.strip_prefix("\\x")
                .and_then(|h| hex::decode(h).ok())
                .unwrap_or_else(|| s.into_bytes())
        }
    }
}

/// crc32(bytea), Postgres 18.
pub(crate) fn crc32(a: &[Value]) -> Value {
    a.first()
        .filter(|v| !v.is_null())
        .map_or(Value::Null, |v| Value::Int(crc(&bytes(v), 0xEDB8_8320)))
}

/// crc32c(bytea), Postgres 18.
pub(crate) fn crc32c(a: &[Value]) -> Value {
    a.first()
        .filter(|v| !v.is_null())
        .map_or(Value::Null, |v| Value::Int(crc(&bytes(v), 0x82F6_3B78)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_values() {
        assert_eq!(crc(b"abc", 0xEDB8_8320), 891_568_578);
        assert_eq!(crc(b"abc", 0x82F6_3B78), 910_901_175);
    }

    #[test]
    fn uuidv7_has_its_version_and_time() {
        let Value::Text(u) = uuidv7(&[]).unwrap() else { panic!() };
        let v = Value::Text(u);
        assert_eq!(uuid_extract_version(std::slice::from_ref(&v)), Value::Int(7));
        let Value::Text(t) = uuid_extract_timestamp(&[v]) else { panic!() };
        assert!(t.starts_with(&Utc::now().format("%Y-%m-%d").to_string()), "{t}");
    }

    #[test]
    fn uuidv7_grows_within_a_millisecond() {
        let a: Vec<String> = (0..1000).map(|_| uuidv7(&[]).unwrap().to_string()).collect();
        assert!(a.windows(2).all(|w| w[0] < w[1]));
    }
}
