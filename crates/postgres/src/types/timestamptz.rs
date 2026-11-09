//! timestamptz over SQLite: a `timestamptz` column stores an absolute instant as
//! bare UTC text. On input, text without an offset of its own is a wall clock in the
//! session's TimeZone (overlite_tz_in); on output we advertise OID 1184 so clients
//! read it as an instant. AT TIME ZONE converts via the zone database.

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
