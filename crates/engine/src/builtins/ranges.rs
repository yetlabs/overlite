//! Range types over SQLite: stored as Postgres' range text (`[1,10)`) in a TEXT cell.
//! A value written to a range column, or cast to a range type, is in the canonical form
//! range_out gives: a discrete range (int4range, int8range, daterange) with an inclusive
//! lower and an exclusive upper bound, `empty` for a range with no point, each bound in
//! its type's output form, quoted where range_out quotes, and a tstzrange's bounds in
//! UTC with `+00`. The operators read either form: bounds compare as numbers, as
//! instants, or as text, in that order of what both read as.

use crate::builtins::casts::{CastType, cast};
use crate::builtins::decimal::rat_from_string;
use crate::error::{Error, Result};
use chrono::{Duration, NaiveDate};
use overlite_core::{SqlState, Value};
use std::cmp::Ordering;

/// Renders a constructor argument, mapping NULL to an empty (infinite) bound.
pub(crate) fn arg_str(v: &Value) -> String {
    if v.is_null() {
        return String::new();
    }
    v.to_string()
}

/// A range type of Postgres'.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeKind {
    Int4,
    Int8,
    Num,
    Date,
    Ts,
    Tstz,
}

impl RangeKind {
    /// The kind a type name means: `int4range`, `pg_catalog.daterange`...
    #[must_use]
    pub fn of_name(name: &str) -> Option<RangeKind> {
        let n = name.trim().trim_matches('"').to_ascii_lowercase();
        Some(match n.strip_prefix("pg_catalog.").unwrap_or(&n) {
            "int4range" => RangeKind::Int4,
            "int8range" => RangeKind::Int8,
            "numrange" => RangeKind::Num,
            "daterange" => RangeKind::Date,
            "tsrange" => RangeKind::Ts,
            "tstzrange" => RangeKind::Tstz,
            _ => return None,
        })
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            RangeKind::Int4 => "int4range",
            RangeKind::Int8 => "int8range",
            RangeKind::Num => "numrange",
            RangeKind::Date => "daterange",
            RangeKind::Ts => "tsrange",
            RangeKind::Tstz => "tstzrange",
        }
    }

    /// The type of the bounds.
    #[must_use]
    pub fn element(self) -> &'static str {
        match self {
            RangeKind::Int4 => "integer",
            RangeKind::Int8 => "bigint",
            RangeKind::Num => "numeric",
            RangeKind::Date => "date",
            RangeKind::Ts => "timestamp",
            RangeKind::Tstz => "timestamptz",
        }
    }

    fn element_cast(self) -> CastType {
        match self {
            RangeKind::Int4 => CastType::Int4,
            RangeKind::Int8 => CastType::Int8,
            RangeKind::Num => CastType::Numeric(None),
            RangeKind::Date => CastType::Date,
            RangeKind::Ts => CastType::Timestamp(None),
            RangeKind::Tstz => CastType::TimestampTz(None),
        }
    }

    fn discrete(self) -> bool {
        matches!(self, RangeKind::Int4 | RangeKind::Int8 | RangeKind::Date)
    }
}

/// A range read from its text. A bound that is None is infinite.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub empty: bool,
    /// A bound's text, None for an infinite one.
    pub lo: Option<String>,
    pub hi: Option<String>,
    pub lo_inc: bool,
    pub hi_inc: bool,
}

/// A range's parts as range_send lays them out, None for what is no range.
#[must_use]
pub fn range_parts(s: &str) -> Option<Range> {
    parse_range(s)
}

/// Whether s reads as a range: `empty`, or a bracket, two bounds and a bracket.
pub(crate) fn is_range_text(s: &str) -> bool {
    parse_range(s).is_some()
}

/// Reads a range as range_in does: `empty` in any case, or `[`/`(`, a lower bound, a
/// comma, an upper bound and `]`/`)`, a bound quoted with double quotes or bare, with
/// backslash escapes, and an absent one infinite.
pub(crate) fn parse_range(s: &str) -> Option<Range> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("empty") {
        return Some(Range {
            empty: true,
            ..Range::default()
        });
    }
    let b = t.as_bytes();
    if b.len() < 3 || !matches!(b[0], b'[' | b'(') || !matches!(b[b.len() - 1], b']' | b')') {
        return None;
    }
    let inner = &t[1..t.len() - 1];
    let (lo, rest) = read_bound(inner, b',')?;
    let rest = rest.strip_prefix(',')?;
    let (hi, rest) = read_bound(rest, 0)?;
    if !rest.trim().is_empty() {
        return None;
    }
    Some(Range {
        empty: false,
        lo_inc: b[0] == b'[' && lo.is_some(),
        hi_inc: b[b.len() - 1] == b']' && hi.is_some(),
        lo,
        hi,
    })
}

/// One bound up to stop (or the end): its text, None when it is absent, and what
/// follows it.
fn read_bound(s: &str, stop: u8) -> Option<(Option<String>, &str)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut quoted_any = false;
    while i < b.len() {
        match b[i] {
            c if c == stop && stop != 0 => break,
            b'"' => {
                quoted_any = true;
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' if i + 1 < b.len() => {
                            out.push(b[i + 1]);
                            i += 2;
                        }
                        b'"' if i + 1 < b.len() && b[i + 1] == b'"' => {
                            out.push(b'"');
                            i += 2;
                        }
                        b'"' => break,
                        c => {
                            out.push(c);
                            i += 1;
                        }
                    }
                }
                if i >= b.len() {
                    return None;
                }
                i += 1;
            }
            b'\\' if i + 1 < b.len() => {
                out.push(b[i + 1]);
                i += 2;
            }
            b'(' | b')' | b'[' | b']' | b',' => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    let text = String::from_utf8(out).ok()?;
    let bound = if quoted_any {
        Some(text)
    } else {
        let t = text.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    Some((bound, &s[i..]))
}

fn malformed(text: &str) -> Error {
    Error::with_state(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("malformed range literal: \"{text}\""),
    )
}

/// The canonical text of a range of kind given as text, as range_in reads it.
///
/// # Errors
///
/// 22P02 for text that is no range, what the element type's input answers for a
/// bound, and 22000 for a lower bound past the upper.
pub fn range_input(kind: RangeKind, text: &str) -> Result<String> {
    let mut r = parse_range(text).ok_or_else(|| malformed(text))?;
    if r.empty {
        return Ok("empty".to_string());
    }
    if kind == RangeKind::Tstz {
        // A bound without an offset of its own is a wall clock in the session's zone,
        // as timestamptz_in reads it; a stored range's bounds carry their +00.
        for b in [&mut r.lo, &mut r.hi] {
            if let Some(s) = b.as_ref()
                && !is_special(s)
            {
                let utc = crate::builtins::casts::timestamptz_in_session(&Value::Text(s.clone()), None)?;
                *b = Some(utc.to_string() + "+00");
            }
        }
    }
    build(kind, r)
}

/// The canonical text of the range a constructor builds: `int4range(1, 5, '[]')`.
///
/// # Errors
///
/// 42601 for bounds flags that are not two of `[(` and `)]`, and as range_input.
pub fn range_construct(kind: RangeKind, lo: &Value, hi: &Value, flags: &str) -> Result<String> {
    let f = flags.as_bytes();
    if f.len() != 2 || !matches!(f[0], b'[' | b'(') || !matches!(f[1], b']' | b')') {
        return Err(Error::with_state(
            SqlState::SYNTAX_ERROR,
            "invalid range bound flags".to_string(),
        ));
    }
    let bound = |v: &Value| (!v.is_null()).then(|| v.to_string());
    let (lo, hi) = (bound(lo), bound(hi));
    build(
        kind,
        Range {
            empty: false,
            lo_inc: f[0] == b'[' && lo.is_some(),
            hi_inc: f[1] == b']' && hi.is_some(),
            lo,
            hi,
        },
    )
}

/// Casts each bound to the element type, makes a discrete range `[)`, and answers
/// `empty` for a range with no point.
fn build(kind: RangeKind, mut r: Range) -> Result<String> {
    let ty = kind.element_cast();
    let canon = |b: &Option<String>| -> Result<Option<String>> {
        match b {
            None => Ok(None),
            Some(s) => Ok(Some(match cast(&Value::Text(s.clone()), &ty)? {
                Value::Int(n) => n.to_string(),
                v => v.to_string(),
            })),
        }
    };
    r.lo = canon(&r.lo)?;
    r.hi = canon(&r.hi)?;
    if kind.discrete() {
        if let Some(lo) = &r.lo
            && !r.lo_inc
        {
            r.lo = Some(next_value(kind, lo)?);
            r.lo_inc = true;
        }
        if let Some(hi) = &r.hi
            && r.hi_inc
        {
            r.hi = Some(next_value(kind, hi)?);
            r.hi_inc = false;
        }
    }
    if let (Some(lo), Some(hi)) = (&r.lo, &r.hi) {
        match cmp_values(lo, hi) {
            Ordering::Greater => {
                return Err(Error::with_state(
                    SqlState::DATA_EXCEPTION,
                    "range lower bound must be less than or equal to range upper bound".to_string(),
                ));
            }
            Ordering::Equal if !(r.lo_inc && r.hi_inc) => return Ok("empty".to_string()),
            _ => {}
        }
    }
    Ok(render(kind, &r))
}

/// The value after v in a discrete type.
fn next_value(kind: RangeKind, v: &str) -> Result<String> {
    if kind == RangeKind::Date {
        let Ok(d) = NaiveDate::parse_from_str(v, "%Y-%m-%d") else {
            return Ok(v.to_string()); // infinity
        };
        return Ok((d + Duration::days(1)).format("%Y-%m-%d").to_string());
    }
    let n: i64 = v.parse().map_err(|_| malformed(v))?;
    let max = if kind == RangeKind::Int4 {
        i64::from(i32::MAX)
    } else {
        i64::MAX
    };
    if n >= max {
        return Err(Error::with_state(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("{} out of range", kind.element()),
        ));
    }
    Ok((n + 1).to_string())
}

/// range_out's text of a canonical range.
fn render(kind: RangeKind, r: &Range) -> String {
    if r.empty {
        return "empty".to_string();
    }
    let bound = |b: &Option<String>| match b {
        None => String::new(),
        Some(s) if kind == RangeKind::Tstz && !is_special(s) => quote_bound(&(s.clone() + "+00")),
        Some(s) => quote_bound(s),
    };
    [
        if r.lo_inc { "[" } else { "(" },
        &bound(&r.lo),
        ",",
        &bound(&r.hi),
        if r.hi_inc { "]" } else { ")" },
    ]
    .concat()
}

/// A stored tstzrange as range_out writes it in the session's TimeZone: each bound's
/// wall clock there with its offset. None for what is no range.
#[must_use]
pub fn tstzrange_session_text(stored: &str) -> Option<String> {
    let mut r = parse_range(stored)?;
    if r.empty {
        return Some("empty".to_string());
    }
    for b in [&mut r.lo, &mut r.hi] {
        if let Some(s) = b.as_ref()
            && !is_special(s)
        {
            *b = Some(crate::builtins::dates::session_tz_text(s)?);
        }
    }
    // The bounds carry their offsets now: rendered as any range's.
    Some(render(RangeKind::Num, &r))
}

fn is_special(s: &str) -> bool {
    s.eq_ignore_ascii_case("infinity") || s.eq_ignore_ascii_case("-infinity")
}

/// A bound as range_out writes it: in double quotes when empty or holding a quote, a
/// backslash, a bracket, a parenthesis, a comma or a space.
fn quote_bound(s: &str) -> String {
    let needs = s.is_empty()
        || s.bytes()
            .any(|c| matches!(c, b'"' | b'\\' | b'(' | b')' | b'[' | b']' | b',') || c.is_ascii_whitespace());
    if !needs {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Two bounds in order: as numbers when both are, as instants when both are, else as
/// text.
pub(crate) fn cmp_values(a: &str, b: &str) -> Ordering {
    if let (Some(x), Some(y)) = (rat_from_string(a), rat_from_string(b)) {
        return x.cmp(&y).cmp(&0);
    }
    if let (Some(x), Some(y)) = (instant(a), instant(b)) {
        return x.cmp(&y);
    }
    a.cmp(b)
}

/// A bound that is a date or a timestamp, as the UTC text that orders as it does.
fn instant(s: &str) -> Option<String> {
    let first = s.trim().bytes().next()?;
    if !(first.is_ascii_digit() || first == b'-' || first == b'i' || first == b'I') {
        return None;
    }
    match cast(&Value::Text(s.to_string()), &CastType::TimestampTz(None)).ok()? {
        Value::Text(t) if t == "infinity" => Some("\u{10FFFF}".to_string()),
        Value::Text(t) if t == "-infinity" => Some(String::new()),
        Value::Text(t) => Some(t),
        _ => None,
    }
}

/// A lower bound's place against another lower bound: an infinite one first, and at
/// the same value an inclusive one first.
fn cmp_lower(a: &Range, b: &Range) -> Ordering {
    match (&a.lo, &b.lo) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Less,
        (_, None) => Ordering::Greater,
        (Some(x), Some(y)) => cmp_values(x, y).then_with(|| b.lo_inc.cmp(&a.lo_inc)),
    }
}

/// An upper bound's place against another upper bound: an infinite one last, and at
/// the same value an inclusive one last.
fn cmp_upper(a: &Range, b: &Range) -> Ordering {
    match (&a.hi, &b.hi) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some(x), Some(y)) => cmp_values(x, y).then_with(|| a.hi_inc.cmp(&b.hi_inc)),
    }
}

/// Whether x's lower bound is at or before y's upper bound.
fn starts_before_end(x: &Range, y: &Range) -> bool {
    match (&x.lo, &y.hi) {
        (None, _) | (_, None) => true,
        (Some(lo), Some(hi)) => match cmp_values(lo, hi) {
            Ordering::Less => true,
            Ordering::Equal => x.lo_inc && y.hi_inc,
            Ordering::Greater => false,
        },
    }
}

/// Whether range a contains the element x.
pub(crate) fn range_contains(a: &str, x: &str) -> bool {
    let Some(r) = parse_range(a).filter(|r| !r.empty) else {
        return false;
    };
    let after_lo = r.lo.as_ref().is_none_or(|lo| match cmp_values(x, lo) {
        Ordering::Greater => true,
        Ordering::Equal => r.lo_inc,
        Ordering::Less => false,
    });
    let before_hi = r.hi.as_ref().is_none_or(|hi| match cmp_values(x, hi) {
        Ordering::Less => true,
        Ordering::Equal => r.hi_inc,
        Ordering::Greater => false,
    });
    after_lo && before_hi
}

/// Whether range a contains range b. The empty range is contained by every range and
/// contains only itself.
pub(crate) fn range_contains_range(a: &str, b: &str) -> bool {
    range_op("@>", a, b).unwrap_or(false)
}

/// Whether two ranges share a point.
pub(crate) fn range_overlaps(a: &str, b: &str) -> bool {
    range_op("&&", a, b).unwrap_or(false)
}

/// A binary range operator by its Postgres spelling: `&&`, `-|-`, `<<`, `>>`, `&<`,
/// `&>`, `@>`, `<@`, and the comparisons. None when an operand is no range.
#[must_use]
pub fn range_op(op: &str, a: &str, b: &str) -> Option<bool> {
    let ra = parse_range(a)?;
    let rb = parse_range(b)?;
    let both = !ra.empty && !rb.empty;
    Some(match op {
        "&&" => both && starts_before_end(&ra, &rb) && starts_before_end(&rb, &ra),
        "@>" => rb.empty || (!ra.empty && cmp_lower(&ra, &rb).is_le() && cmp_upper(&ra, &rb).is_ge()),
        "<@" => ra.empty || (!rb.empty && cmp_lower(&rb, &ra).is_le() && cmp_upper(&rb, &ra).is_ge()),
        "<<" => both && strictly_left(&ra, &rb),
        ">>" => both && strictly_left(&rb, &ra),
        "&<" => both && cmp_upper(&ra, &rb).is_le(),
        "&>" => both && cmp_lower(&ra, &rb).is_ge(),
        "-|-" => both && (adjacent(&ra, &rb) || adjacent(&rb, &ra)),
        "=" => range_cmp(&ra, &rb).is_eq(),
        "<>" | "!=" => range_cmp(&ra, &rb).is_ne(),
        "<" => range_cmp(&ra, &rb).is_lt(),
        "<=" => range_cmp(&ra, &rb).is_le(),
        ">" => range_cmp(&ra, &rb).is_gt(),
        ">=" => range_cmp(&ra, &rb).is_ge(),
        _ => return None,
    })
}

/// Whether every point of a is before every point of b.
fn strictly_left(a: &Range, b: &Range) -> bool {
    match (&a.hi, &b.lo) {
        (None, _) | (_, None) => false,
        (Some(hi), Some(lo)) => match cmp_values(hi, lo) {
            Ordering::Less => true,
            Ordering::Equal => !(a.hi_inc && b.lo_inc),
            Ordering::Greater => false,
        },
    }
}

/// Whether a ends where b starts, with no point between and none shared.
fn adjacent(a: &Range, b: &Range) -> bool {
    match (&a.hi, &b.lo) {
        (Some(hi), Some(lo)) => cmp_values(hi, lo).is_eq() && a.hi_inc != b.lo_inc,
        _ => false,
    }
}

/// Postgres' order of ranges: the empty range first, then by lower bound, then by
/// upper bound.
pub(crate) fn range_cmp(a: &Range, b: &Range) -> Ordering {
    match (a.empty, b.empty) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => cmp_lower(a, b).then_with(|| cmp_upper(a, b)),
    }
}

/// A bound as a value: an integer for an integer range's, else its text.
pub(crate) fn bound_value(b: Option<&String>) -> Value {
    match b {
        None => Value::Null,
        Some(s) => s.parse::<i64>().map_or_else(|_| Value::Text(s.clone()), Value::Int),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_forms() {
        let t = |k, s| range_input(k, s).unwrap();
        assert_eq!(t(RangeKind::Int4, "[1,5]"), "[1,6)");
        assert_eq!(t(RangeKind::Int4, "(1,5)"), "[2,5)");
        assert_eq!(t(RangeKind::Int4, "[0,10]"), "[0,11)");
        assert_eq!(t(RangeKind::Int4, "(,)"), "(,)");
        assert_eq!(t(RangeKind::Int4, "[3,3)"), "empty");
        assert_eq!(t(RangeKind::Int4, "EMPTY"), "empty");
        assert_eq!(t(RangeKind::Num, "[1.50, 2.5]"), "[1.50,2.5]");
        assert_eq!(t(RangeKind::Date, "[2015-01-01,2015-01-31]"), "[2015-01-01,2015-02-01)");
        assert_eq!(
            t(RangeKind::Ts, "[2020-01-01 10:00, 2020-01-02)"),
            "[\"2020-01-01 10:00:00\",\"2020-01-02 00:00:00\")"
        );
        assert_eq!(
            t(RangeKind::Tstz, "[\"2020-01-01 10:00+02\",)"),
            "[\"2020-01-01 08:00:00+00\",)"
        );
        assert!(range_input(RangeKind::Int4, "[5,1)").is_err());
        assert!(range_input(RangeKind::Int4, "[a,1)").is_err());
        assert!(range_input(RangeKind::Int4, "1,5").is_err());
    }

    #[test]
    fn operators() {
        let op = |o, a, b| range_op(o, a, b).unwrap();
        assert!(op("-|-", "[1,5)", "[5,10)"));
        assert!(!op("-|-", "[1,5)", "[6,10)"));
        assert!(op("-|-", "[5,10)", "[1,5)"));
        assert!(op("&&", "[2015-01-01,2015-06-01)", "[2015-05-01,2015-07-01)"));
        assert!(!op("&&", "[1,5)", "[5,10)"));
        assert!(op("&<", "[1,5)", "[5,10)"));
        assert!(op("&>", "[1,5)", "[0,10)"));
        assert!(op("<<", "[1,5)", "[5,10)"));
        assert!(!op("<<", "[1,5]", "[5,10)"));
        assert!(op(">>", "[20,30)", "[5,10)"));
        assert!(op("@>", "[6,10)", "empty"));
        assert!(!op("@>", "[6,10)", "[1,5)"));
        assert!(op("<", "[20,50)", "[100,200)"));
        assert!(op("<", "empty", "[1,2)"));
        assert!(range_contains("[2015-01-01,2015-05-06)", "2015-05-05"));
        assert!(!range_contains("[2015-01-01,2015-05-05)", "2015-05-05"));
        assert!(range_contains(
            "[\"2020-01-01 08:00:00+00\",)",
            "2020-01-01 10:00:00+02"
        ));
        assert!(range_contains("[-5,0)", "-1"));
    }
}

/// A bound of a range as Postgres' range_cmp_bounds compares them: its value (None
/// for an infinite one), whether it is inclusive, and whether it is the lower one.
#[derive(Clone, Debug)]
struct Bound {
    val: Option<String>,
    inc: bool,
    lower: bool,
}

fn lower_of(r: &Range) -> Bound {
    Bound {
        val: r.lo.clone(),
        inc: r.lo_inc,
        lower: true,
    }
}

fn upper_of(r: &Range) -> Bound {
    Bound {
        val: r.hi.clone(),
        inc: r.hi_inc,
        lower: false,
    }
}

/// range_cmp_bounds: where two bounds fall, a lower one against an upper one too.
fn cmp_bounds(a: &Bound, b: &Bound) -> Ordering {
    let side = |lower: bool| if lower { Ordering::Less } else { Ordering::Greater };
    match (&a.val, &b.val) {
        (None, None) if a.lower == b.lower => Ordering::Equal,
        (None, _) => side(a.lower),
        (_, None) => side(b.lower).reverse(),
        (Some(x), Some(y)) => match cmp_values(x, y) {
            Ordering::Equal => match (a.inc, b.inc) {
                (false, false) if a.lower == b.lower => Ordering::Equal,
                (false, false | true) => side(a.lower).reverse(),
                (true, false) => side(b.lower),
                (true, true) => Ordering::Equal,
            },
            o => o,
        },
    }
}

/// make_range: the range of two bounds, empty where they hold no point, made canonical
/// as its type is where the type is known.
fn make_range(kind: Option<RangeKind>, lo: &Bound, hi: &Bound) -> Result<String> {
    let r = Range {
        empty: false,
        lo: lo.val.clone(),
        hi: hi.val.clone(),
        lo_inc: lo.inc && lo.val.is_some(),
        hi_inc: hi.inc && hi.val.is_some(),
    };
    if let (Some(a), Some(b)) = (&r.lo, &r.hi) {
        match cmp_values(a, b) {
            Ordering::Greater => return Ok("empty".to_string()),
            Ordering::Equal if !(r.lo_inc && r.hi_inc) => return Ok("empty".to_string()),
            _ => {}
        }
    }
    match kind {
        Some(k) if k.discrete() => build(k, r),
        Some(RangeKind::Tstz) | None => Ok(render(RangeKind::Num, &r)),
        Some(k) => Ok(render(k, &r)),
    }
}

/// A range's union (`+`), difference (`-`) or intersection (`*`), as Postgres'
/// range_union, range_minus and range_intersect compute them over the type's bounds.
/// None when an operand is no range.
///
/// # Errors
///
/// 22000 for a union of ranges that neither overlap nor touch, or a difference that
/// would leave two pieces.
pub fn range_set_op(op: &str, a: &str, b: &str, kind: Option<RangeKind>) -> Result<Option<String>> {
    let (Some(r1), Some(r2)) = (parse_range(a), parse_range(b)) else {
        return Ok(None);
    };
    let text = |r: &Range| render(RangeKind::Num, r);
    let (l1, u1, l2, u2) = (lower_of(&r1), upper_of(&r1), lower_of(&r2), upper_of(&r2));
    let overlap = !r1.empty && !r2.empty && cmp_bounds(&l1, &u2).is_le() && cmp_bounds(&l2, &u1).is_le();
    Ok(Some(match op {
        "*" => {
            if !overlap {
                return Ok(Some("empty".to_string()));
            }
            let lo = if cmp_bounds(&l1, &l2).is_ge() { &l1 } else { &l2 };
            let hi = if cmp_bounds(&u1, &u2).is_le() { &u1 } else { &u2 };
            make_range(kind, lo, hi)?
        }
        "+" => {
            if r1.empty {
                return Ok(Some(text(&r2)));
            }
            if r2.empty {
                return Ok(Some(text(&r1)));
            }
            let touch = adjacent(&r1, &r2) || adjacent(&r2, &r1);
            if !overlap && !touch {
                return Err(Error::with_state(
                    SqlState::DATA_EXCEPTION,
                    "result of range union would not be contiguous".to_string(),
                ));
            }
            let lo = if cmp_bounds(&l1, &l2).is_le() { &l1 } else { &l2 };
            let hi = if cmp_bounds(&u1, &u2).is_ge() { &u1 } else { &u2 };
            make_range(kind, lo, hi)?
        }
        "-" => {
            if r1.empty || r2.empty {
                return Ok(Some(text(&r1)));
            }
            let (l1l2, l1u2) = (cmp_bounds(&l1, &l2), cmp_bounds(&l1, &u2));
            let (u1l2, u1u2) = (cmp_bounds(&u1, &l2), cmp_bounds(&u1, &u2));
            if l1l2.is_lt() && u1u2.is_gt() {
                return Err(Error::with_state(
                    SqlState::DATA_EXCEPTION,
                    "result of range difference would not be contiguous".to_string(),
                ));
            }
            if l1u2.is_gt() || u1l2.is_lt() {
                return Ok(Some(text(&r1)));
            }
            if l1l2.is_ge() && u1u2.is_le() {
                return Ok(Some("empty".to_string()));
            }
            if l1l2.is_le() && u1l2.is_ge() && u1u2.is_le() {
                let hi = Bound {
                    val: l2.val.clone(),
                    inc: !l2.inc,
                    lower: false,
                };
                return make_range(kind, &l1, &hi).map(Some);
            }
            if l1l2.is_ge() && u1u2.is_ge() && l1u2.is_le() {
                let lo = Bound {
                    val: u2.val.clone(),
                    inc: !u2.inc,
                    lower: true,
                };
                return make_range(kind, &lo, &u1).map(Some);
            }
            return Ok(Some(text(&r1)));
        }
        _ => return Ok(None),
    }))
}

#[cfg(test)]
mod set_op_tests {
    use super::*;

    fn op(o: &str, a: &str, b: &str, k: &str) -> String {
        range_set_op(o, a, b, RangeKind::of_name(k)).unwrap().unwrap()
    }

    #[test]
    fn as_postgres_computes_them() {
        assert_eq!(op("*", "[1,5)", "[3,8)", "int4range"), "[3,5)");
        assert_eq!(op("*", "[1,5)", "[7,9)", "int4range"), "empty");
        assert_eq!(op("*", "[1,5)", "[5,9)", "int4range"), "empty");
        assert_eq!(op("+", "[1,5)", "[3,8)", "int4range"), "[1,8)");
        assert_eq!(op("+", "[1,5)", "[5,8)", "int4range"), "[1,8)");
        assert_eq!(op("-", "[1,5)", "[3,8)", "int4range"), "[1,3)");
        assert_eq!(op("-", "[1,10)", "[1,5)", "int4range"), "[5,10)");
        assert_eq!(op("-", "[1,5)", "[1,8)", "int4range"), "empty");
        assert_eq!(op("-", "[1,5)", "[7,8)", "int4range"), "[1,5)");
        assert_eq!(op("+", "[1.5,2.5)", "[2.5,3)", "numrange"), "[1.5,3)");
        assert_eq!(op("-", "[1.5,3)", "[2,3]", "numrange"), "[1.5,2)");
        assert_eq!(op("*", "(,5)", "[2,)", "int4range"), "[2,5)");
        assert_eq!(op("+", "empty", "[2,3)", "int4range"), "[2,3)");
        assert!(range_set_op("+", "[1,2)", "[3,4)", None).is_err());
        assert!(range_set_op("-", "[1,10)", "[3,4)", None).is_err());
    }
}
