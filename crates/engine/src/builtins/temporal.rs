//! `+` and `-` over dates, times, timestamps and intervals, as Postgres has them:
//! resolve/temporal.rs writes each such sum as `overlite_tadd(a, b, 'KIND', 'KIND')` or
//! `overlite_tsub`, naming what each side is declared when the statement says so, and
//! these read the rest from the values, which the storage format keeps in one form per
//! type. SQLite read `'2010-06-25 12:15:30'` as the number 2010.

use crate::builtins::casts::{CastType, Interval, cast, format_stamp, format_time, parse_interval};
use crate::error::{Error, Result};
use chrono::{Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use overlite_core::{SqlState, Value};

/// What a side of a sum is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Interval,
    Number,
    /// A numeric: exact, where SQLite's arithmetic is a double's.
    Decimal,
}

impl Kind {
    /// The kind of a declared type or a cast's type name, None for another.
    pub(crate) fn of_decl(decl: &str) -> Option<Kind> {
        let d = decl.trim().trim_matches('"').to_ascii_lowercase();
        if d.ends_with(']') {
            return None;
        }
        let base = d.split('(').next().unwrap_or("").trim();
        let zoned = d.contains("tz") || d.contains("with time zone");
        Some(match base {
            "date" => Kind::Date,
            "interval" => Kind::Interval,
            "timestamptz" => Kind::TimestampTz,
            b if b.starts_with("timestamp") && zoned && !d.contains("without") => Kind::TimestampTz,
            b if b.starts_with("timestamp") => Kind::Timestamp,
            "time" | "time without time zone" => Kind::Time,
            b if b.starts_with("time") && !zoned => Kind::Time,
            "numeric" | "decimal" | "decimaltext" => Kind::Decimal,
            "int" | "integer" | "bigint" | "smallint" | "int2" | "int4" | "int8" | "real" | "float" | "float4"
            | "float8" | "double" | "double precision" => Kind::Number,
            _ => return None,
        })
    }

    pub(crate) fn of_name(name: &str) -> Option<Kind> {
        Some(match name {
            "DATE" => Kind::Date,
            "TIME" => Kind::Time,
            "TIMESTAMP" => Kind::Timestamp,
            "TIMESTAMPTZ" => Kind::TimestampTz,
            "INTERVAL" => Kind::Interval,
            "NUMBER" | "DECIMAL" => Kind::Number,
            _ => return None,
        })
    }

    pub(crate) fn temporal(self) -> bool {
        !matches!(self, Kind::Number | Kind::Decimal)
    }
}

/// What `a + b` (or `a - b`) is, by what its sides are, as Postgres' operators say.
pub(crate) fn result_kind(sub: bool, a: Kind, b: Kind) -> Option<Kind> {
    use Kind::{Date, Decimal, Interval, Number, Time, Timestamp, TimestampTz};
    let number = |k: Kind| if k == Decimal { Number } else { k };
    let (a, b) = (number(a), number(b));
    Some(match (sub, a, b) {
        (_, Timestamp, Interval) | (false, Interval, Timestamp) => Timestamp,
        (_, TimestampTz, Interval) | (false, Interval, TimestampTz) => TimestampTz,
        (_, Date, Interval) | (false, Interval | Time, Date) | (false, Date, Time) => Timestamp,
        (_, Date, Number) | (false, Number, Date) => Date,
        (_, Time, Interval) | (false, Interval, Time) => Time,
        (_, Interval, Interval) | (true, Time, Time) => Interval,
        (true, Timestamp | TimestampTz, Timestamp | TimestampTz) => Interval,
        (true, Date, Date) => Number,
        _ => return None,
    })
}

/// A value's kind by its form, for a side the statement does not type: a literal or
/// a parameter. `other` is the kind of the other side, which settles a clock that
/// could be a time or an interval.
fn kind_of_value(v: &Value, other: Option<Kind>, sub: bool) -> Option<Kind> {
    match v {
        Value::Int(_) | Value::Float(_) => return Some(Kind::Number),
        Value::Text(_) => {}
        _ => return None,
    }
    let s = v.to_string();
    let t = s.trim();
    let b = t.as_bytes();
    let digits = |r: std::ops::Range<usize>| b.get(r).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    if digits(0..4) && b.get(4) == Some(&b'-') && digits(5..7) && b.get(7) == Some(&b'-') && digits(8..10) {
        return Some(if b.len() == 10 { Kind::Date } else { Kind::Timestamp });
    }
    if t.parse::<f64>().is_ok() {
        return Some(Kind::Number);
    }
    if digits(0..1) && t.contains(':') && !t.contains(' ') && cast(v, &CastType::Time(None)).is_ok() {
        // A clock: a time beside a time it is taken from, an interval otherwise.
        return Some(match other {
            Some(Kind::Time) if sub => Kind::Time,
            Some(Kind::Date) if !sub => Kind::Time,
            _ => Kind::Interval,
        });
    }
    if parse_interval(t).is_some() {
        return Some(Kind::Interval);
    }
    if cast(v, &CastType::Timestamp(None)).is_ok() {
        return Some(Kind::Timestamp);
    }
    None
}

fn kind_arg(v: &Value) -> Option<Kind> {
    match v {
        Value::Text(s) => Kind::of_name(s),
        _ => None,
    }
}

fn no_operator(sub: bool, a: &Value, b: &Value) -> Error {
    Error::with_state(
        SqlState::UNDEFINED_FUNCTION,
        format!("operator does not exist: {a} {} {b}", if sub { "-" } else { "+" }),
    )
}

/// A value read as its kind's type, as Postgres' input function would.
fn read(v: &Value, k: Kind) -> Result<Value> {
    let ty = match k {
        Kind::Date => CastType::Date,
        Kind::Time => CastType::Time(None),
        Kind::Timestamp => CastType::Timestamp(None),
        Kind::TimestampTz => CastType::TimestampTz(None),
        Kind::Interval => CastType::Interval,
        Kind::Number | Kind::Decimal => return Ok(v.clone()),
    };
    cast(v, &ty)
}

fn stamp(s: &str) -> Option<NaiveDateTime> {
    crate::builtins::casts::naive_stamp(s)
}

fn date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn clock(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, "%H:%M:%S%.f").ok()
}

fn interval(s: &str) -> Option<Interval> {
    parse_interval(s)
}

fn negate(i: Interval) -> Interval {
    Interval {
        months: -i.months,
        days: -i.days,
        micros: -i.micros,
    }
}

/// A timestamp moved by an interval: its months, keeping the day within the month,
/// then its days, then its time.
fn shift(t: NaiveDateTime, i: &Interval) -> Option<NaiveDateTime> {
    let t = if i.months >= 0 {
        t.checked_add_months(Months::new(u32::try_from(i.months).ok()?))?
    } else {
        t.checked_sub_months(Months::new(u32::try_from(-i.months).ok()?))?
    };
    t.checked_add_signed(Duration::days(i.days))?
        .checked_add_signed(Duration::microseconds(i.micros))
}

/// A timestamptz (UTC) moved by an interval as Postgres moves one: its months and days
/// on its wall clock in the session's zone, so that a day across a change of offset is
/// a calendar day, then its time as elapsed time.
fn shift_zoned(t: NaiveDateTime, i: &Interval) -> Option<NaiveDateTime> {
    if (i.months == 0 && i.days == 0) || crate::builtins::dates::session_is_utc() {
        return shift(t, i);
    }
    let wall = t + Duration::seconds(crate::builtins::dates::session_offset_at(&t));
    let calendar = Interval {
        months: i.months,
        days: i.days,
        micros: 0,
    };
    crate::builtins::dates::session_wall_to_utc(&shift(wall, &calendar)?)?
        .checked_add_signed(Duration::microseconds(i.micros))
}

/// The interval between two instants, in days and a clock under a day, as Postgres'
/// timestamp - timestamp gives it.
fn between(a: NaiveDateTime, b: NaiveDateTime) -> Interval {
    let micros = (a - b).num_microseconds().unwrap_or(0);
    let day = 86_400_000_000;
    Interval {
        months: 0,
        days: micros / day,
        micros: micros % day,
    }
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        other => other.to_string().trim().parse().ok(),
    }
}

/// `overlite_tadd(a, b, ka, kb)` and `overlite_tsub`: the sum or difference, with ka and
/// kb the kinds the statement gives its sides, '' for one it leaves to the value.
///
/// # Errors
///
/// 42883 for sides no operator takes, and what the type's input answers for a value
/// it does not read.
pub(crate) fn temporal_op(args: &[Value], sub: bool) -> Result<Value> {
    let (a, b) = (&args[0], &args[1]);
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    let (sa, sb) = (kind_arg(&args[2]), kind_arg(&args[3]));
    let ka = sa.or_else(|| kind_of_value(a, sb, sub));
    let kb = sb.or_else(|| kind_of_value(b, ka, sub));
    let (Some(ka), Some(kb)) = (ka, kb) else {
        return Err(no_operator(sub, a, b));
    };
    if !ka.temporal() && !kb.temporal() {
        return Ok(plain(a, b, sub));
    }
    let Some(out) = result_kind(sub, ka, kb) else {
        return Err(no_operator(sub, a, b));
    };
    let (va, vb) = (read(a, ka)?.to_string(), read(b, kb)?.to_string());
    // infinity stays infinity.
    for (v, k) in [(&va, ka), (&vb, kb)] {
        if matches!(k, Kind::Timestamp | Kind::TimestampTz | Kind::Date) && v.ends_with("infinity") {
            return Ok(Value::Text(v.clone()));
        }
    }
    let bad = || no_operator(sub, a, b);
    let text = |t: NaiveDateTime| Value::Text(format_stamp(t, None));
    Ok(match (ka, kb) {
        (Kind::Timestamp | Kind::TimestampTz, Kind::Interval)
        | (Kind::Interval, Kind::Timestamp | Kind::TimestampTz) => {
            let (t, i) = if ka == Kind::Interval { (&vb, &va) } else { (&va, &vb) };
            let i = interval(i).ok_or_else(bad)?;
            let i = if sub { negate(i) } else { i };
            let t = stamp(t).ok_or_else(bad)?;
            let moved = if Kind::TimestampTz == if ka == Kind::Interval { kb } else { ka } {
                shift_zoned(t, &i)
            } else {
                shift(t, &i)
            };
            text(moved.ok_or_else(bad)?)
        }
        (Kind::Date, Kind::Interval) | (Kind::Interval, Kind::Date) => {
            let (d, i) = if ka == Kind::Interval { (&vb, &va) } else { (&va, &vb) };
            let i = interval(i).ok_or_else(bad)?;
            let i = if sub { negate(i) } else { i };
            text(shift(date(d).ok_or_else(bad)?.and_time(NaiveTime::default()), &i).ok_or_else(bad)?)
        }
        (Kind::Date, Kind::Time) | (Kind::Time, Kind::Date) => {
            let (d, c) = if ka == Kind::Time { (&vb, &va) } else { (&va, &vb) };
            text(date(d).ok_or_else(bad)?.and_time(clock(c).ok_or_else(bad)?))
        }
        (Kind::Date, Kind::Number) | (Kind::Number, Kind::Date) => {
            let (d, n) = if ka == Kind::Number { (&vb, a) } else { (&va, b) };
            let n = number(n).ok_or_else(bad)?;
            if n.fract() != 0.0 {
                return Err(bad());
            }
            let n = if sub { -n } else { n } as i64;
            let d = date(d)
                .ok_or_else(bad)?
                .checked_add_signed(Duration::days(n))
                .ok_or_else(bad)?;
            Value::Text(d.format("%Y-%m-%d").to_string())
        }
        (Kind::Date, Kind::Date) => {
            let (x, y) = (date(&va).ok_or_else(bad)?, date(&vb).ok_or_else(bad)?);
            Value::Int((x - y).num_days())
        }
        (Kind::Timestamp | Kind::TimestampTz, Kind::Timestamp | Kind::TimestampTz) => {
            let (x, y) = (stamp(&va).ok_or_else(bad)?, stamp(&vb).ok_or_else(bad)?);
            Value::Text(between(x, y).to_string())
        }
        (Kind::Time, Kind::Time) => {
            let (x, y) = (clock(&va).ok_or_else(bad)?, clock(&vb).ok_or_else(bad)?);
            Value::Text(
                Interval {
                    months: 0,
                    days: 0,
                    micros: (x - y).num_microseconds().unwrap_or(0),
                }
                .to_string(),
            )
        }
        (Kind::Time, Kind::Interval) | (Kind::Interval, Kind::Time) => {
            let (c, i) = if ka == Kind::Interval { (&vb, &va) } else { (&va, &vb) };
            let i = interval(i).ok_or_else(bad)?;
            let micros = if sub { -i.micros } else { i.micros };
            let (t, _) = clock(c)
                .ok_or_else(bad)?
                .overflowing_add_signed(Duration::microseconds(micros));
            Value::Text(format_time(t, None))
        }
        (Kind::Interval, Kind::Interval) => {
            let (x, y) = (interval(&va).ok_or_else(bad)?, interval(&vb).ok_or_else(bad)?);
            let y = if sub { negate(y) } else { y };
            Value::Text(
                Interval {
                    months: x.months + y.months,
                    days: x.days + y.days,
                    micros: x.micros + y.micros,
                }
                .to_string(),
            )
        }
        _ => return Err(bad()),
    })
    .map(|v| {
        // The sum's type in its stored form, as a write of it would keep.
        match (out, &v) {
            (Kind::Time, Value::Text(s)) => clock(s).map_or(v.clone(), |t| Value::Text(format_time(t, None))),
            _ => v,
        }
    })
}

/// An interval times a number, as Postgres' interval_mul: the fraction of a month
/// carried into days at 30 a month, the fraction of a day into the time.
pub(crate) fn interval_mul(i: &Interval, factor: f64) -> Option<Interval> {
    if !factor.is_finite() {
        return None;
    }
    let months = i.months as f64 * factor;
    let month_days = (months - months.trunc()) * 30.0;
    let days = i.days as f64 * factor;
    let mut day = days.trunc() as i64;
    let mut secs = (days - days.trunc() + month_days - month_days.trunc()) * 86_400.0;
    secs = (secs * 1e6).round_ties_even() / 1e6;
    if secs.abs() >= 86_400.0 {
        day += (secs / 86_400.0).trunc() as i64;
        secs -= (secs / 86_400.0).trunc() * 86_400.0;
    }
    day += month_days.trunc() as i64;
    let micros = (i.micros as f64 * factor + secs * 1e6).round_ties_even();
    Some(Interval {
        months: months.trunc() as i64,
        days: day,
        micros: micros as i64,
    })
}

/// An interval over a number, as Postgres' interval_div: each field divided, the
/// fraction of a month carried into days at 30 a month, of a day into the time.
pub(crate) fn interval_div(i: &Interval, factor: f64) -> Option<Interval> {
    if !factor.is_finite() || factor == 0.0 {
        return None;
    }
    let months = i.months as f64 / factor;
    let month_days = (months - months.trunc()) * 30.0;
    let days = i.days as f64 / factor;
    let mut day = days.trunc() as i64;
    let mut secs = (days - days.trunc() + month_days - month_days.trunc()) * 86_400.0;
    secs = (secs * 1e6).round_ties_even() / 1e6;
    if secs.abs() >= 86_400.0 {
        day += (secs / 86_400.0).trunc() as i64;
        secs -= (secs / 86_400.0).trunc() * 86_400.0;
    }
    day += month_days.trunc() as i64;
    let micros = (i.micros as f64 / factor + secs * 1e6).round_ties_even();
    Some(Interval {
        months: months.trunc() as i64,
        days: day,
        micros: micros as i64,
    })
}

/// An interval's place against another, as Postgres orders them: by their span, a
/// month counted as 30 days and a day as 24 hours.
pub(crate) fn interval_cmp(a: &Interval, b: &Interval) -> std::cmp::Ordering {
    let span = |i: &Interval| (i128::from(i.months) * 30 + i128::from(i.days)) * 86_400_000_000 + i128::from(i.micros);
    span(a).cmp(&span(b))
}

/// `overlite_tmul(a, b, ka, kb, op)`: a product or quotient with a side of a date or
/// time type. An interval by a number is one; any other is 42883, as in Postgres.
///
/// # Errors
///
/// 42883 for sides no operator takes, 22012 for a division by zero.
pub(crate) fn temporal_mul(args: &[Value]) -> Result<Value> {
    let (a, b) = (&args[0], &args[1]);
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    let div = args[4].to_string() == "/";
    let ka = kind_arg(&args[2]).or_else(|| kind_of_value(a, None, false));
    let kb = kind_arg(&args[3]).or_else(|| kind_of_value(b, None, false));
    let type_name = |k: Option<Kind>| match k {
        Some(Kind::Date) => "date",
        Some(Kind::Time) => "time without time zone",
        Some(Kind::Timestamp) => "timestamp without time zone",
        Some(Kind::TimestampTz) => "timestamp with time zone",
        Some(Kind::Interval) => "interval",
        Some(Kind::Decimal) => "numeric",
        Some(Kind::Number) | None => "integer",
    };
    let refuse = || {
        Error::with_state(
            SqlState::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} {} {}",
                type_name(ka),
                if div { "/" } else { "*" },
                type_name(kb)
            ),
        )
    };
    let numberish = |k: Option<Kind>| matches!(k, Some(Kind::Number | Kind::Decimal));
    let (iv, n) = match (ka, kb) {
        (Some(Kind::Interval), k) if numberish(k) => (a, b),
        (k, Some(Kind::Interval)) if numberish(k) && !div => (b, a),
        _ => return Err(refuse()),
    };
    let i = interval(&read(iv, Kind::Interval)?.to_string()).ok_or_else(refuse)?;
    let n = number(n).ok_or_else(refuse)?;
    if div && n == 0.0 {
        return Err(Error::with_state(
            SqlState::DIVISION_BY_ZERO,
            "division by zero".to_string(),
        ));
    }
    let out = if div { interval_div(&i, n) } else { interval_mul(&i, n) };
    let out =
        out.ok_or_else(|| Error::with_state(SqlState::DATETIME_FIELD_OVERFLOW, "interval out of range".to_string()))?;
    Ok(Value::Text(out.to_string()))
}

/// a + b or a - b of two numbers, as SQLite has them.
fn plain(a: &Value, b: &Value, sub: bool) -> Value {
    if let (Value::Int(x), Value::Int(y)) = (a, b)
        && let Some(r) = if sub { x.checked_sub(*y) } else { x.checked_add(*y) }
    {
        return Value::Int(r);
    }
    let (x, y) = (number(a).unwrap_or(0.0), number(b).unwrap_or(0.0));
    Value::Float(if sub { x - y } else { x + y })
}

/// The sum of intervals given as text, and their average: Postgres adds the months,
/// days and time apart, and divides each, carrying a fraction of a month into days
/// and of a day into the time.
pub(crate) fn interval_sum(values: &[String], avg: bool) -> Option<String> {
    let mut sum = Interval::default();
    for v in values {
        let i = parse_interval(v)?;
        sum.months += i.months;
        sum.days += i.days;
        sum.micros += i.micros;
    }
    if !avg {
        return Some(sum.to_string());
    }
    let n = values.len() as f64;
    if values.is_empty() {
        return None;
    }
    let months = sum.months as f64 / n;
    let whole_months = months.trunc();
    let days = sum.days as f64 / n + (months - whole_months) * 30.0;
    let whole_days = days.trunc();
    let micros = sum.micros as f64 / n + (days - whole_days) * 86_400_000_000.0;
    Some(
        Interval {
            months: whole_months as i64,
            days: whole_days as i64,
            micros: micros.round() as i64,
        }
        .to_string(),
    )
}

/// date_trunc of an interval: the fields under the unit go.
///
/// # Errors
///
/// 0A000 for a unit an interval has no field for, as Postgres says.
pub(crate) fn interval_trunc(unit: &str, text: &str) -> Result<Value> {
    let Some(mut i) = parse_interval(text) else {
        return Ok(Value::Null);
    };
    let second = 1_000_000;
    let keep = |micros: i64, unit: i64| micros - micros % unit;
    match unit.trim().to_ascii_lowercase().as_str() {
        "microseconds" => {}
        "milliseconds" => i.micros = keep(i.micros, 1000),
        "second" => i.micros = keep(i.micros, second),
        "minute" => i.micros = keep(i.micros, 60 * second),
        "hour" => i.micros = keep(i.micros, 3600 * second),
        "day" => i.micros = 0,
        "month" => (i.days, i.micros) = (0, 0),
        "quarter" => (i.months, i.days, i.micros) = (i.months - i.months % 3, 0, 0),
        "year" => (i.months, i.days, i.micros) = (i.months - i.months % 12, 0, 0),
        "decade" => (i.months, i.days, i.micros) = (i.months - i.months % 120, 0, 0),
        "century" => (i.months, i.days, i.micros) = (i.months - i.months % 1200, 0, 0),
        "millennium" => (i.months, i.days, i.micros) = (i.months - i.months % 12000, 0, 0),
        other => {
            return Err(Error::with_state(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("unit \"{other}\" not supported for type interval"),
            ));
        }
    }
    Ok(Value::Text(i.to_string()))
}

/// Whether a text is an interval, for the aggregates: no number, date or timestamp,
/// and what interval input reads.
pub(crate) fn looks_like_interval(s: &str) -> bool {
    let t = s.trim();
    let b = t.as_bytes();
    if t.parse::<f64>().is_ok() || b.len() >= 10 && b[4] == b'-' && b[7] == b'-' {
        return false;
    }
    parse_interval(t).is_some()
}

/// A whole number and the micro-units past it, as the text of a numeric of that scale.
fn scaled(units: i128, scale: u32) -> String {
    if scale == 0 {
        return units.to_string();
    }
    let div = 10i128.pow(scale);
    let sign = if units < 0 { "-" } else { "" };
    let a = units.abs();
    format!("{sign}{}.{:0width$}", a / div, a % div, width = scale as usize)
}

fn unsupported(field: &str, ty: &str) -> Error {
    Error::with_state(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("unit \"{field}\" not supported for type {ty}"),
    )
}

/// EXTRACT and date_part: a field of a timestamp, date, time or interval, as the text
/// of the numeric EXTRACT gives (a second with six decimals, an epoch with six for a
/// timestamp), or the double date_part gives.
///
/// # Errors
///
/// 0A000 for a field the type does not have, and what the type's input answers for a
/// value it does not read.
pub(crate) fn extract(field: &Value, v: &Value, numeric: bool) -> Result<Value> {
    if field.is_null() || v.is_null() {
        return Ok(Value::Null);
    }
    let f = field.to_string().trim().to_ascii_lowercase();
    let f = f.trim_matches('\'');
    let text = v.to_string();
    // A time reads as the interval since midnight, which has its fields.
    let (units, scale) = if looks_like_interval(&text) {
        interval_field(f, &text)?
    } else {
        let date_only = text.trim().len() == 10;
        let ts = read(v, Kind::Timestamp)?.to_string();
        if ts.ends_with("infinity") {
            return Ok(if numeric {
                Value::Text(ts.replace("infinity", "Infinity"))
            } else {
                Value::Float(if ts.starts_with('-') {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                })
            });
        }
        let t = stamp(&ts).ok_or_else(|| unsupported(f, "timestamp"))?;
        stamp_field(f, t, date_only)?
    };
    // EXTRACT's numeric as a number SQLite compares with numbers: Django filters on
    // `EXTRACT(YEAR FROM x) = 2015`, which the text of a numeric would never equal.
    let div = 10i128.pow(scale);
    if numeric && units % div == 0 {
        return Ok(i64::try_from(units / div).map_or_else(|_| Value::Text(scaled(units, scale)), Value::Int));
    }
    Ok(Value::Float(units as f64 / div as f64))
}

fn clock_field(f: &str, micros: i128, ty: &str) -> Result<(i128, u32)> {
    let minute = 60_000_000;
    Ok(match f {
        "microseconds" | "microsecond" | "us" => (micros % minute, 0),
        "milliseconds" | "millisecond" | "ms" => (micros % minute, 3),
        "second" | "seconds" | "sec" | "s" => (micros % minute, 6),
        "minute" | "minutes" | "min" | "m" => (micros / minute % 60, 0),
        "hour" | "hours" | "h" => (micros / (60 * minute), 0),
        "epoch" => (micros, 6),
        _ => return Err(unsupported(f, ty)),
    })
}

fn stamp_field(f: &str, t: NaiveDateTime, date_only: bool) -> Result<(i128, u32)> {
    use chrono::Datelike;
    let year = i128::from(t.year());
    let micros = i128::from(t.num_seconds_from_midnight()) * 1_000_000 + i128::from(t.nanosecond() / 1000);
    let whole = |n: i128| Ok((n, 0));
    match f {
        "millennium" | "millennia" => whole(if year > 0 {
            (year + 999) / 1000
        } else {
            -((999 - year) / 1000)
        }),
        "century" | "centuries" => whole(if year > 0 {
            (year + 99) / 100
        } else {
            -((99 - year) / 100)
        }),
        "decade" | "decades" => whole(year.div_euclid(10)),
        "year" | "years" => whole(year),
        "isoyear" => whole(i128::from(t.iso_week().year())),
        "quarter" => whole(i128::from((t.month() - 1) / 3 + 1)),
        "month" | "months" => whole(i128::from(t.month())),
        "week" | "weeks" => whole(i128::from(t.iso_week().week())),
        "day" | "days" => whole(i128::from(t.day())),
        "dow" => whole(i128::from(t.weekday().num_days_from_sunday())),
        "isodow" => whole(i128::from(t.weekday().number_from_monday())),
        "doy" => whole(i128::from(t.ordinal())),
        "julian" => whole(i128::from(t.date().num_days_from_ce()) + 1_721_425),
        "epoch" => {
            let secs = i128::from(t.and_utc().timestamp());
            let micros = secs * 1_000_000 + i128::from(t.nanosecond() / 1000);
            Ok(if date_only { (secs, 0) } else { (micros, 6) })
        }
        "timezone" | "timezone_hour" | "timezone_minute" => whole(0),
        _ if date_only => Err(unsupported(f, "date")),
        other => clock_field(other, micros, "timestamp without time zone"),
    }
}

fn interval_field(f: &str, text: &str) -> Result<(i128, u32)> {
    let i = parse_interval(text).ok_or_else(|| unsupported(f, "interval"))?;
    let (months, days, micros) = (i128::from(i.months), i128::from(i.days), i128::from(i.micros));
    let hour = 3_600_000_000;
    let whole = |n: i128| Ok((n, 0));
    match f {
        "millennium" | "millennia" => whole(months / 12 / 1000),
        "century" | "centuries" => whole(months / 12 / 100),
        "decade" | "decades" => whole(months / 12 / 10),
        "year" | "years" => whole(months / 12),
        "quarter" => whole(months % 12 / 3 + 1),
        "month" | "months" => whole(months % 12),
        "day" | "days" => whole(days),
        "hour" | "hours" => whole(micros / hour),
        "epoch" => {
            // A year is 365.25 days, a month 30.
            let secs_micros = (months / 12) * 36_525 * 864_000_000
                + (months % 12) * 30 * 86_400_000_000
                + days * 86_400_000_000
                + micros;
            Ok((secs_micros, 6))
        }
        other => clock_field(other, micros, "interval"),
    }
}

/// now(): the instant of a Julian day number, as SQLite's 'now' gives one for the
/// statement, to its millisecond, in the timestamptz form.
pub(crate) fn now_from_julian(v: &Value) -> Value {
    let Some(jd) = number(v) else {
        return Value::Null;
    };
    let millis = ((jd - 2_440_587.5) * 86_400_000.0).round() as i64;
    match chrono::DateTime::from_timestamp_millis(millis) {
        Some(t) => Value::Text(format_stamp(t.naive_utc(), None)),
        None => Value::Null,
    }
}

std::thread_local! {
    /// When the transaction of the session on this thread began, which now() gives
    /// throughout it.
    static TRANSACTION_START: std::cell::Cell<Option<NaiveDateTime>> = const { std::cell::Cell::new(None) };
}

/// Marks the start of a transaction of the session on this thread, now, or its end.
pub fn mark_transaction_start(open: bool) {
    let start = open.then(|| chrono::Utc::now().naive_utc());
    TRANSACTION_START.with(|t| t.set(start));
}

/// now() in a transaction: when it began.
pub(crate) fn transaction_start() -> Option<Value> {
    TRANSACTION_START
        .with(std::cell::Cell::get)
        .map(|t| Value::Text(format_stamp(t, None)))
}

/// clock_timestamp(): the instant it is read, to the microsecond.
pub(crate) fn clock_timestamp() -> Value {
    Value::Text(format_stamp(chrono::Utc::now().naive_utc(), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(a: &str, b: &str, ka: &str, kb: &str, sub: bool) -> String {
        let args = [
            Value::Text(a.into()),
            Value::Text(b.into()),
            Value::Text(ka.into()),
            Value::Text(kb.into()),
        ];
        temporal_op(&args, sub).unwrap().to_string()
    }

    #[test]
    fn arithmetic() {
        assert_eq!(
            op("2010-06-25 12:15:30", "1 day 02:00:00", "TIMESTAMP", "INTERVAL", false),
            "2010-06-26 14:15:30"
        );
        assert_eq!(
            op(
                "2010-06-26 12:15:31",
                "2010-06-25 12:15:30",
                "TIMESTAMP",
                "TIMESTAMP",
                true
            ),
            "1 day 00:00:01"
        );
        assert_eq!(op("12:30:15", "11:15:00", "TIME", "", true), "01:15:15");
        assert_eq!(op("2010-06-27", "2010-06-20", "DATE", "", true), "7");
        assert_eq!(
            op("2010-01-31", "1 mon", "DATE", "INTERVAL", false),
            "2010-02-28 00:00:00"
        );
        assert_eq!(op("23:30:00", "01:00:00", "TIME", "INTERVAL", false), "00:30:00");
        assert_eq!(
            op("2010-06-25 12:15:30.5", "0.25 seconds", "TIMESTAMP", "", false),
            "2010-06-25 12:15:30.75"
        );
        assert_eq!(
            op("1 day", "2 days 01:00:00", "INTERVAL", "INTERVAL", false),
            "3 days 01:00:00"
        );
        assert_eq!(
            interval_sum(&["1 day 02:00:00".into(), "2 days".into()], true).unwrap(),
            "1 day 13:00:00"
        );
    }
}
