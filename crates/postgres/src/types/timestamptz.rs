//! timestamptz over SQLite: a `timestamptz` column stores an absolute instant as
//! bare UTC text. On input, a timestamp literal carrying an explicit offset is
//! converted to UTC; on output we advertise the +00 offset and OID 1184 so clients
//! read it as an instant. AT TIME ZONE converts via the zone database.

use crate::types::binary_time::{TimeLayout, ZONE_ISO, format_date_time, parse_time_layout};
use overlite_core::lazy_regex;
use overlite_core::text::trim_end_chars;

/// Reports whether a declared type is timestamp-with-time-zone.
pub(crate) fn is_timestamptz_decl(decl: &str) -> bool {
    let d = decl.to_uppercase();
    d.contains("TIMESTAMPTZ") || (d.contains("TIMESTAMP") && d.contains("WITH TIME ZONE"))
}

/// Tags a bare UTC timestamp string with the +00 offset (leaving a value that
/// already has an offset alone).
pub(crate) fn with_utc_offset(s: &str) -> String {
    let s = trim_end_chars(s, " ");
    let i = match s.find(':') {
        Some(i) if !s.is_empty() => i,
        _ => return s.to_string(), // not a full timestamp
    };
    if s.ends_with('Z') || s[i..].contains(['+', '-']) {
        return s.to_string(); // already carries an offset
    }
    [s, "+00"].concat()
}

/// The bare UTC text a timestamptz column's default stands for, when the default is
/// the text of a timestamp with an offset: what rewrite_timestamptz_literal writes.
pub(crate) fn utc_default(body: &str) -> Option<String> {
    if !RE_WHOLE_TIMESTAMPTZ.is_match(body) {
        return None;
    }
    to_utc_literal(body)
}

/// Parses a timestamp with an explicit offset and returns the bare UTC
/// representation.
fn to_utc_literal(s: &str) -> Option<String> {
    let s = s.trim().replacen('T', " ", 1);
    let ci = s.find(':')?;
    let body: &str;
    let off: String;
    if let Some(b) = s.strip_suffix('Z') {
        body = b;
        off = "+00:00".to_string();
    } else {
        let k = s[ci..].find(['+', '-'])?;
        body = &s[..ci + k];
        off = s[ci + k..].to_string();
    }
    let off = norm_offset(&off);
    if off.is_empty() {
        return None;
    }
    // Layouts "2006-01-02 15:04:05[.999999999]Z07:00".
    let frac = body.contains('.');
    let tm = parse_time_layout(
        &TimeLayout {
            date: true,
            sep: b' ',
            time: true,
            zone: ZONE_ISO,
        },
        [body, &off].concat().as_bytes(),
    )?;
    let utc = tm.naive_utc();
    Some(format_date_time(&utc, frac))
}

/// Canonicalizes an offset (+02, -0500, +02:00, …) to ±HH:MM.
fn norm_offset(o: &str) -> String {
    let b = o.as_bytes();
    if b.len() < 3 || (b[0] != b'+' && b[0] != b'-') {
        return String::new();
    }
    let mut rest = o[1..].replace(':', "");
    while rest.len() < 4 {
        rest.push('0');
    }
    [&o[..1], &rest[..2], ":", &rest[2..4]].concat()
}

// Matches a literal that is entirely a timestamp carrying an explicit zone.
lazy_regex!(
    RE_WHOLE_TIMESTAMPTZ,
    r"^\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}(?::?\d{2})?)$"
);
