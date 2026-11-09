//! A client that asks for binary results gets whatever the row description
//! promised, so a date or timestamp column has to be encoded the way Postgres
//! does: microseconds (or days) relative to 2000-01-01, not its text spelling.

use crate::types::timestamptz::with_utc_offset;
use crate::types::{OID_DATE, OID_TIME, OID_TIMESTAMP, OID_TIMESTAMPTZ};
use crate::wire::{append_uint32, append_uint64};
use chrono::{Datelike, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Timelike};
use overlite_core::{Oid, Timestamp, Value};

/// Unix seconds of the moment Postgres counts its binary timestamps from
/// (2000-01-01 00:00:00 UTC).
const PG_EPOCH: i64 = 946_684_800;

const ZONE_NONE: u8 = 0;
/// Layout "-07:00".
const ZONE_NUM: u8 = 1;
/// Layout "Z07:00".
pub(crate) const ZONE_ISO: u8 = 2;

/// A time layout from the family this module parses.
#[derive(Debug)]
pub(crate) struct TimeLayout {
    /// Has "2006-01-02".
    pub(crate) date: bool,
    /// The byte between date and time (b' ' or b'T'), when it has both.
    pub(crate) sep: u8,
    /// Has "15:04:05", with an optional fraction.
    pub(crate) time: bool,
    pub(crate) zone: u8,
}

/// The spellings SQLite stores a time value in.
static TIMESTAMP_LAYOUTS: [TimeLayout; 9] = [
    // "2006-01-02 15:04:05.999999999-07:00"
    TimeLayout {
        date: true,
        sep: b' ',
        time: true,
        zone: ZONE_NUM,
    },
    // "2006-01-02 15:04:05.999999999Z07:00"
    TimeLayout {
        date: true,
        sep: b' ',
        time: true,
        zone: ZONE_ISO,
    },
    // "2006-01-02T15:04:05.999999999Z07:00"
    TimeLayout {
        date: true,
        sep: b'T',
        time: true,
        zone: ZONE_ISO,
    },
    // "2006-01-02 15:04:05.999999999"
    TimeLayout {
        date: true,
        sep: b' ',
        time: true,
        zone: ZONE_NONE,
    },
    // "2006-01-02T15:04:05.999999999"
    TimeLayout {
        date: true,
        sep: b'T',
        time: true,
        zone: ZONE_NONE,
    },
    // "2006-01-02 15:04:05"
    TimeLayout {
        date: true,
        sep: b' ',
        time: true,
        zone: ZONE_NONE,
    },
    // "2006-01-02"
    TimeLayout {
        date: true,
        sep: 0,
        time: false,
        zone: ZONE_NONE,
    },
    // "15:04:05.999999999"
    TimeLayout {
        date: false,
        sep: 0,
        time: true,
        zone: ZONE_NONE,
    },
    // "15:04:05"
    TimeLayout {
        date: false,
        sep: 0,
        time: true,
        zone: ZONE_NONE,
    },
];

fn is_digit(v: &[u8], i: usize) -> bool {
    i < v.len() && v[i].is_ascii_digit()
}

/// time.getnum.
fn getnum(v: &[u8], fixed: bool) -> Option<(u32, &[u8])> {
    if !is_digit(v, 0) {
        return None;
    }
    if !is_digit(v, 1) {
        if fixed {
            return None;
        }
        return Some(((v[0] - b'0') as u32, &v[1..]));
    }
    Some((((v[0] - b'0') as u32) * 10 + (v[1] - b'0') as u32, &v[2..]))
}

fn skip_byte(v: &[u8], c: u8) -> Option<&[u8]> {
    if v.first() == Some(&c) { Some(&v[1..]) } else { None }
}

/// Parses by the layouts above. The hour may be one digit, a
/// space in the layout takes any run of spaces, and a fraction of any length (cut
/// to nanoseconds, after a period or a comma) is read after the seconds whether
/// the layout names one or not.
pub(crate) fn parse_time_layout(l: &TimeLayout, s: &[u8]) -> Option<Timestamp> {
    let (wall, off) = parse_time_wall(l, s)?;
    if let Some(fo) = FixedOffset::east_opt(off) {
        return fo.from_local_datetime(&wall).single();
    }
    // chrono stops short of offsets of 24h and over. The instant survives in UTC;
    // whoever needs the wall clock reads parse_stored_wall.
    let utc = wall.checked_sub_signed(chrono::TimeDelta::seconds(off as i64))?;
    FixedOffset::east_opt(0)?.from_local_datetime(&utc).single()
}

/// parse_time_layout's wall clock and offset in seconds.
fn parse_time_wall(l: &TimeLayout, s: &[u8]) -> Option<(NaiveDateTime, i32)> {
    let mut v = s;
    let (mut year, mut month, mut day) = (0i32, 1u32, 1u32);
    if l.date {
        // Four digits, or up to six past 9999, as Postgres writes such a year.
        let digits = v.iter().take_while(|c| c.is_ascii_digit()).count();
        if !(4..=6).contains(&digits) {
            return None;
        }
        year = v[..digits].iter().fold(0, |n, c| n * 10 + (*c - b'0') as i32);
        v = skip_byte(&v[digits..], b'-')?;
        (month, v) = getnum(v, true)?;
        if !(1..=12).contains(&month) {
            return None;
        }
        v = skip_byte(v, b'-')?;
        (day, v) = getnum(v, true)?;
    }
    let (mut hour, mut min, mut sec, mut nanos) = (0u32, 0u32, 0u32, 0u32);
    if l.time {
        if l.date {
            if l.sep == b' ' {
                if !v.is_empty() && v[0] != b' ' {
                    return None;
                }
                while v.first() == Some(&b' ') {
                    v = &v[1..];
                }
            } else {
                v = skip_byte(v, l.sep)?;
            }
        }
        (hour, v) = getnum(v, false)?;
        if hour >= 24 {
            return None;
        }
        v = skip_byte(v, b':')?;
        (min, v) = getnum(v, true)?;
        if min >= 60 {
            return None;
        }
        v = skip_byte(v, b':')?;
        (sec, v) = getnum(v, true)?;
        if sec >= 60 {
            return None;
        }
        if v.len() >= 2 && (v[0] == b'.' || v[0] == b',') && is_digit(v, 1) {
            let mut n = 2;
            while is_digit(v, n) {
                n += 1;
            }
            let digits = &v[1..n.min(10)];
            nanos = digits.iter().fold(0, |x, c| x * 10 + (*c - b'0') as u32);
            for _ in digits.len()..9 {
                nanos *= 10;
            }
            v = &v[n..];
        }
    }
    let mut offset: Option<i32> = None;
    if l.zone != ZONE_NONE {
        if l.zone == ZONE_ISO && v.first() == Some(&b'Z') {
            v = &v[1..];
            offset = Some(0);
        } else {
            if v.len() < 6 || v[3] != b':' {
                return None;
            }
            let (hr, _) = getnum(&v[1..3], true)?;
            let (mm, _) = getnum(&v[4..6], true)?;
            if hr > 24 || mm > 60 {
                return None;
            }
            let secs = (hr * 3600 + mm * 60) as i32;
            offset = Some(match v[0] {
                b'+' => secs,
                b'-' => -secs,
                _ => return None,
            });
            v = &v[6..];
        }
    }
    if !v.is_empty() {
        return None;
    }
    let nd = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_nano_opt(hour, min, sec, nanos)?;
    Some((nd, offset.unwrap_or(0)))
}

/// Reads a stored timestamp, date or time value.
pub(crate) fn parse_stored_time(v: &Value) -> Option<Timestamp> {
    match v {
        Value::Time(t) => Some(*t),
        Value::Text(t) => {
            let s = t.trim();
            for layout in &TIMESTAMP_LAYOUTS {
                if let Some(tm) = parse_time_layout(layout, s.as_bytes()) {
                    return Some(tm);
                }
            }
            None
        }
        _ => None,
    }
}

/// A stored value's wall clock and offset in seconds, which Timestamp cannot hold
/// past 24h: the local reading where parse_stored_time gives UTC.
pub(crate) fn parse_stored_wall(v: &Value) -> Option<(NaiveDateTime, i32)> {
    match v {
        Value::Time(t) => Some((t.naive_local(), t.offset().local_minus_utc())),
        Value::Text(t) => parse_stored_text(t),
        _ => None,
    }
}

fn parse_stored_text(t: &str) -> Option<(NaiveDateTime, i32)> {
    let s = t.trim();
    TIMESTAMP_LAYOUTS.iter().find_map(|l| parse_time_wall(l, s.as_bytes()))
}

/// Whether a cell that would have read as a time stayed text because its offset
/// is past what chrono holds.
pub(crate) fn is_far_zone_time(decl: &str, s: &str) -> bool {
    matches!(decl, "DATE" | "DATETIME" | "TIMESTAMP") && parse_stored_text(s).is_some_and(|(_, off)| off.abs() >= 86400)
}

/// Nanoseconds since the Postgres epoch, saturating at the i64 range.
fn since_pg_epoch(secs: i64, nanos: u32) -> i64 {
    let ns = (secs as i128 - PG_EPOCH as i128) * 1_000_000_000 + nanos as i128;
    ns.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Renders a timestamp as microseconds since the Postgres epoch.
pub(crate) fn encode_binary_timestamp(tm: &Timestamp) -> Vec<u8> {
    let micros = since_pg_epoch(tm.timestamp(), tm.timestamp_subsec_nanos()) / 1000;
    let mut b = Vec::with_capacity(8);
    append_uint64(&mut b, micros as u64);
    b
}

/// Renders a date as days since the Postgres epoch.
pub(crate) fn encode_binary_date(tm: &Timestamp) -> Vec<u8> {
    const DAY_NS: i64 = 86_400_000_000_000;
    let days = (since_pg_epoch(tm.timestamp().div_euclid(86400) * 86400, 0) / DAY_NS) as i32;
    let mut b = Vec::with_capacity(4);
    append_uint32(&mut b, days as u32);
    b
}

/// encode_binary_time from the wall clock.
pub(crate) fn encode_binary_wall_time(wall: &NaiveDateTime) -> Vec<u8> {
    let micros = wall.num_seconds_from_midnight() as i64 * 1_000_000 + (wall.nanosecond() / 1000) as i64;
    let mut b = Vec::with_capacity(8);
    append_uint64(&mut b, micros as u64);
    b
}

/// Reports whether a column holds a date or time value.
pub(crate) fn is_time_oid(oid: Oid) -> bool {
    oid == OID_DATE || oid == OID_TIME || oid == OID_TIMESTAMP || oid == OID_TIMESTAMPTZ
}

/// `YYYY-MM-DD`.
fn format_date(t: &NaiveDateTime) -> String {
    let y = t.year();
    if y < 0 {
        return format!("-{:04}-{:02}-{:02}", -y, t.month(), t.day());
    }
    format!("{:04}-{:02}-{:02}", y, t.month(), t.day())
}

/// `HH:MM:SS`, plus up to six fractional digits when frac is set.
fn format_clock(t: &NaiveDateTime, frac: bool) -> String {
    let mut out = format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second());
    let micros = t.nanosecond() / 1000;
    if frac && micros != 0 {
        let f = format!(".{micros:06}");
        out.push_str(f.trim_end_matches('0'));
    }
    out
}

/// `YYYY-MM-DD HH:MM:SS`, plus up to six fractional digits when frac is set.
/// A UTC instant as timestamptz_out writes it: the wall clock of the session's TimeZone
/// and its offset, `+03`, `-03:30`, `+00:19:32`.
pub(crate) fn in_session_zone(utc: &NaiveDateTime) -> String {
    if overlite_engine::session_zone_is_utc() {
        return [format_date_time(utc, true).as_str(), "+00"].concat();
    }
    let off = overlite_engine::session_offset_at(utc);
    let Some(wall) = utc.checked_add_signed(chrono::TimeDelta::seconds(off)) else {
        return [format_date_time(utc, true).as_str(), "+00"].concat();
    };
    let a = off.unsigned_abs();
    let mut zone = format!("{}{:02}", if off < 0 { '-' } else { '+' }, a / 3600);
    if !a.is_multiple_of(3600) {
        zone.push_str(&format!(":{:02}", a / 60 % 60));
        if !a.is_multiple_of(60) {
            zone.push_str(&format!(":{:02}", a % 60));
        }
    }
    [format_date_time(&wall, true), zone].concat()
}

pub(crate) fn format_date_time(t: &NaiveDateTime, frac: bool) -> String {
    [format_date(t), " ".to_string(), format_clock(t, frac)].concat()
}

/// Renders a stored time value the way Postgres writes it. A value it cannot
/// parse is passed through: better the client's own complaint than a value
/// invented here.
pub(crate) fn canonical_time_text(oid: Oid, s: &str) -> String {
    let Some((wall, off)) = parse_stored_text(s) else {
        // Not a full timestamp, but it may still be a bare one that only
        // needs its offset spelled out.
        if oid == OID_TIMESTAMPTZ {
            return with_utc_offset(s);
        }
        return s.to_string();
    };
    if oid == OID_DATE {
        format_date(&wall)
    } else if oid == OID_TIME {
        format_clock(&wall, true)
    } else if oid == OID_TIMESTAMPTZ {
        let Some(utc) = wall.checked_sub_signed(chrono::TimeDelta::seconds(off as i64)) else {
            return s.to_string();
        };
        in_session_zone(&utc)
    } else {
        format_date_time(&wall, true)
    }
}

#[cfg(test)]
mod far_zone_tests {
    use super::*;

    #[test]
    fn an_offset_of_a_day_keeps_its_wall_clock() {
        let s = "2024-01-01 12:00:00+24:30";
        assert_eq!(canonical_time_text(OID_TIMESTAMP, s), "2024-01-01 12:00:00");
        assert_eq!(canonical_time_text(OID_DATE, s), "2024-01-01");
        assert_eq!(canonical_time_text(OID_TIME, s), "12:00:00");
        assert_eq!(canonical_time_text(OID_TIMESTAMPTZ, s), "2023-12-31 11:30:00+00");
        let (wall, off) = parse_stored_wall(&Value::Text(s.to_string())).unwrap();
        assert_eq!(off, 88200);
        assert_eq!(encode_binary_wall_time(&wall), 43_200_000_000u64.to_be_bytes());
        assert!(is_far_zone_time("TIMESTAMP", s));
        assert!(!is_far_zone_time("TEXT", s));
        assert!(!is_far_zone_time("TIMESTAMP", "2024-01-01 12:00:00+23:59"));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(
            canonical_time_text(OID_TIMESTAMPTZ, "2024-01-02 03:04:05"),
            "2024-01-02 03:04:05+00"
        );
        assert_eq!(canonical_time_text(OID_DATE, "2024-01-02"), "2024-01-02");
        assert_eq!(canonical_time_text(OID_TIME, "03:04:05.500"), "03:04:05.5");
        let tm = parse_stored_time(&Value::Text("2000-01-02".into())).unwrap();
        assert_eq!(encode_binary_date(&tm), vec![0, 0, 0, 1]);
        assert_eq!(encode_binary_timestamp(&tm), 86_400_000_000u64.to_be_bytes().to_vec());
    }
}
