//! timestamptz over SQLite: a `timestamptz` column stores an absolute instant as
//! bare UTC text. On input, a timestamp literal carrying an explicit offset is
//! converted to UTC; on output we advertise the +00 offset and OID 1184 so clients
//! read it as an instant. AT TIME ZONE converts via the zone database.

use crate::ddl::sequences::is_ident_part;
use crate::rewrite::casts::operand_before;
use crate::rewrite::operators::operand_after;
use crate::rewrite::splice;
use crate::rewrite::tokens::{CodeTokKind, code_tokens};
use crate::session::utility::first_word_upper;
use crate::types::binary_time::{TimeLayout, ZONE_ISO, format_date_time, parse_time_layout};
use overlite_core::lazy_regex;
use overlite_core::text::{split_words, trim_end_chars};
use overlite_engine::code_only;

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

/// Converts a default that is a timestamp with an offset to the bare UTC a timestamptz
/// column stores. Only there: a column's default is the one place the column's type is
/// in the statement. A value written to a column goes through overlite_assign, one
/// compared with one through resolve_zone_values, and any other is the text it is.
pub(crate) fn rewrite_timestamptz_literal(sql: &str) -> String {
    let word = first_word_upper(sql);
    if !sql.contains('\'') || (word != "CREATE" && word != "ALTER") {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    let toks = code_tokens(sql);
    for (i, t) in toks.iter().enumerate() {
        let lit = &sql[t.span.clone()];
        if t.kind != CodeTokKind::Value || !lit.starts_with('\'') || !lit.ends_with('\'') || lit.len() < 2 {
            continue;
        }
        let body = &lit[1..lit.len() - 1];
        if !RE_WHOLE_TIMESTAMPTZ.is_match(body) || i == 0 || !toks[i - 1].is_word(sql, "default") {
            continue;
        }
        // The type stands between the column's name and its DEFAULT.
        let column_start = toks[..i]
            .iter()
            .rposition(|p| p.is_punct(sql, ",") || p.is_punct(sql, "("))
            .map_or(0, |p| toks[p].span.end);
        if !is_timestamptz_decl(&sql[column_start..t.span.start]) {
            continue;
        }
        if let Some(utc) = to_utc_literal(body) {
            out.push_str(&sql[at..t.span.start]);
            out.push('\'');
            out.push_str(&utc);
            out.push('\'');
            at = t.span.end;
        }
    }
    out.push_str(&sql[at..]);
    out
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

// What can end the operand of AT TIME ZONE and is not an operand itself: a cast, and
// the type word of a typed literal or a CAST. The type gives the direction.
const TIME_TYPE: &str = r"(timestamptz|timestamp(?:\s*\(\s*\d+\s*\))?(?:\s+with(?:out)?\s+time\s+zone)?)";
lazy_regex!(RE_TAIL_TIME_CAST, &[r"(?i)::\s*", TIME_TYPE, r"\s*$"].concat());
lazy_regex!(RE_TAIL_TIME_WORD, &[r"(?i)\b", TIME_TYPE, r"\s*$"].concat());
lazy_regex!(
    RE_CAST_TO_TIME,
    &[r"(?is)^cast\s*\((.*)\sas\s+", TIME_TYPE, r"\s*\)$"].concat()
);
lazy_regex!(RE_TAIL_CAST, r"::\s*[\w.]+(?:\s*\([\d\s,]*\))?\s*$");

/// Whether a time type is a timestamp, a wall clock, and not a timestamptz.
fn is_wall_clock(ty: &str) -> bool {
    let ty = ty.to_ascii_lowercase();
    !ty.contains("timestamptz") && !split_words(&ty).contains(&"with")
}

/// Where the operand ending at end starts, the casts it carries included.
fn cast_chain_start(sql: &str, end: usize) -> usize {
    let (start, _) = operand_before(sql, end);
    let head = trim_end_chars(&sql[..start], " ");
    match head.strip_suffix("::") {
        Some(rest) => cast_chain_start(sql, rest.len()),
        None => start,
    }
}

/// What is known of the operand of an AT TIME ZONE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneOperand {
    WallClock,
    Instant,
    /// A column, or an expression: the engine finds out.
    Unknown,
}

impl ZoneOperand {
    fn of_type(ty: &str) -> ZoneOperand {
        if is_wall_clock(ty) {
            ZoneOperand::WallClock
        } else {
            ZoneOperand::Instant
        }
    }

    fn function(self) -> &'static str {
        match self {
            ZoneOperand::WallClock => "overlite_from_time_zone(",
            ZoneOperand::Instant => "overlite_instant_at_time_zone(",
            ZoneOperand::Unknown => "overlite_column_at_time_zone(",
        }
    }
}

/// The operand of the AT TIME ZONE at kw: its span, the expression to convert, and
/// what it is. A cast to a time type is dropped: the value is converted as it is.
fn at_time_zone_operand(sql: &str, kw: usize) -> (usize, usize, String, ZoneOperand) {
    let head = code_only(&sql[..kw]);
    if let Some(c) = RE_TAIL_TIME_CAST.captures(&head) {
        let cast = c.get(0).map_or(kw, |m| m.start());
        let start = cast_chain_start(sql, cast);
        return (
            start,
            kw,
            sql[start..cast].trim().to_string(),
            ZoneOperand::of_type(&c[1]),
        );
    }
    if RE_TAIL_CAST.is_match(&head) {
        let end = trim_end_chars(&sql[..kw], " ").len();
        let start = cast_chain_start(sql, RE_TAIL_CAST.find(&head).map_or(end, |m| m.start()));
        return (start, end, sql[start..end].to_string(), ZoneOperand::Unknown);
    }
    let (ls, le) = operand_before(sql, kw);
    let operand = &sql[ls..le];
    if operand.starts_with('\'')
        && let Some(c) = RE_TAIL_TIME_WORD.captures(&head[..ls])
    {
        let word = c.get(0).map_or(ls, |m| m.start());
        return (word, le, operand.to_string(), ZoneOperand::of_type(&c[1]));
    }
    if let Some(c) = RE_CAST_TO_TIME.captures(&code_only(operand)) {
        let inner = c.get(1).map_or(0..0, |m| m.range());
        return (ls, le, operand[inner].trim().to_string(), ZoneOperand::of_type(&c[2]));
    }
    (ls, le, operand.to_string(), ZoneOperand::Unknown)
}

/// Maps `expr AT TIME ZONE 'zone'` onto the engine function for its direction: an
/// instant becomes a wall clock in the zone, or a wall clock in the zone becomes an
/// instant. A column's type is not known here: the engine resolves
/// overlite_column_at_time_zone by what the column was declared.
pub(crate) fn rewrite_at_time_zone(sql: &str) -> String {
    const KW: &str = "at time zone";
    let mut sql = sql.to_string();
    loop {
        // In the code only: a literal stays where it is, so finding one here never
        // ended, the statement growing until the server ran out of memory.
        let low = code_only(&sql).to_ascii_lowercase();
        let Some(i) = low.find(KW) else { return sql };
        if i > 0 && is_ident_part(low.as_bytes()[i - 1]) {
            return sql;
        }
        let (ls, le, operand, wall) = at_time_zone_operand(&sql, i);
        let (rs, re) = operand_after(&sql, i + KW.len());
        if ls == le || rs == re {
            return sql;
        }
        let repl = [wall.function(), &operand, ", ", &sql[rs..re], ")"].concat();
        sql = splice(&sql, ls, re, &repl);
    }
}

// Matches a literal that is entirely a timestamp carrying an explicit zone.
lazy_regex!(
    RE_WHOLE_TIMESTAMPTZ,
    r"^\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}(?::?\d{2})?)$"
);
