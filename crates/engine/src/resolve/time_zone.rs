//! `x AT TIME ZONE z` goes both ways: a timestamp, a wall clock, becomes an instant, and
//! a timestamptz, an instant, becomes a wall clock. The rewrite decides by the casts it
//! can see, and takes anything else for an instant. Here the statement is in SQLite's
//! hands, which knows what a column was declared: a probe is prepared with the operand
//! alone over the same FROM, never stepped, and its decltype decides. That sees through
//! an alias, a join and a subquery. An expression has no decltype, and keeps the
//! direction it was given, but for a `datetime()` the rewrite wrote around a column.
//!
//! The same probe tells what a timestamp with an offset is being compared with.

use crate::blank::blank_literals;
use crate::sqlite::Script;
use overlite_core::{Value, lazy_regex};
use rusqlite::Connection;
use std::borrow::Cow;

const AT: &str = "overlite_column_at_time_zone(";
const FROM_ZONE: &str = "overlite_from_time_zone(";

/// Turns each overlite_column_at_time_zone of a timestamp column around.
pub(crate) fn resolve_time_zones<'a>(q: &Connection, query: &'a str) -> Cow<'a, str> {
    if !query.contains(AT) {
        return Cow::Borrowed(query);
    }
    let mut sql = query.to_string();
    let mut from = 0;
    while let Some(at) = sql[from..].find(AT).map(|i| from + i) {
        from = at + AT.len();
        let Some(end) = closing(&sql, from - 1) else {
            continue;
        };
        let Some(operand) = first_arg(&sql[from..end]) else {
            continue;
        };
        if is_timestamp_column(q, &sql, at, operand_column(operand)) {
            sql = [&sql[..at], FROM_ZONE, &sql[from..]].concat();
            from = at + FROM_ZONE.len();
        }
    }
    Cow::Owned(sql)
}

/// The operand itself, or the column an interval is added to or taken from:
/// `(ts + interval '1 hour')` goes the way ts goes.
fn operand_column(operand: &str) -> &str {
    let t = operand.trim();
    let t = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(t);
    if let Some(rest) = t.strip_prefix("datetime(") {
        return first_arg(rest.strip_suffix(')').unwrap_or(rest)).unwrap_or(t);
    }
    let code = blank_literals(t);
    let mut depth = 0;
    for (i, c) in code.bytes().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'+' | b'-' if depth == 0 && i > 0 && t[i + 1..].contains("'interval')") => {
                return t[..i].trim();
            }
            _ => {}
        }
    }
    t
}

/// The index of the parenthesis closing the one at open.
fn closing(sql: &str, open: usize) -> Option<usize> {
    let code = blank_literals(sql);
    let mut depth = 0;
    for (i, c) in code.bytes().enumerate().skip(open) {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The first of an argument list.
fn first_arg(args: &str) -> Option<&str> {
    let code = blank_literals(args);
    let mut depth = 0;
    for (i, c) in code.bytes().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => return Some(&args[..i]),
            _ => {}
        }
    }
    Some(args)
}

/// The start of the FROM of the query around pos, and the end of that query.
fn enclosing_from(sql: &str, pos: usize) -> Option<(usize, usize)> {
    let code = blank_literals(sql).to_ascii_lowercase();
    let b = code.as_bytes();
    let mut opens = Vec::new();
    for (i, c) in b.iter().enumerate().take(pos) {
        match c {
            b'(' => opens.push(i),
            b')' => {
                opens.pop();
            }
            _ => {}
        }
    }
    let mut start = 0;
    let mut end = b.len();
    for &open in opens.iter().rev() {
        if code
            .get(open + 1..)
            .is_some_and(|rest| rest.trim_start().starts_with("select"))
        {
            start = open + 1;
            end = closing(sql, open)?;
            break;
        }
    }
    let mut depth = 0;
    for i in start..end {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0
                && b[i..].starts_with(b"from")
                && (i == 0 || !b[i - 1].is_ascii_alphanumeric() && b[i - 1] != b'_')
                && !b.get(i + 4).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') =>
            {
                return Some((i, end));
            }
            _ => {}
        }
    }
    None
}

/// Bumped by every CREATE, ALTER and DROP this server runs, which may change a declared
/// type: column_decl_cached reads again after one. A file changed by another process
/// is not seen until then.
pub(crate) static DDL_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Declared types by FROM clause and name.
type DeclCache = std::collections::HashMap<(String, String), Option<String>>;

std::thread_local! {
    /// The declared types column_decl_cached has read on this thread's connection, by
    /// the FROM they were read over and the name, while DDL_GENERATION stays.
    static DECLS: std::cell::RefCell<(i64, DeclCache)> = std::cell::RefCell::new((-1, DeclCache::new()));
}

/// column_decl, kept for the FROM clause and the name: a statement run again, or one
/// over the same tables, takes no probe. The clause stops at its WHERE, GROUP BY, ORDER
/// BY or LIMIT, whose literals change from one run to the next.
pub(crate) fn column_decl_cached(
    q: &Connection,
    sql: &str,
    pos: usize,
    operand: &str,
    version: &mut Option<i64>,
) -> Option<String> {
    let (from, end) = enclosing_from(sql, pos)?;
    let code = blank_literals(&sql[from..end]).to_ascii_lowercase();
    let cut = [
        " where ", " group ", " order ", " limit ", " having ", " window ", " union ",
    ]
    .iter()
    .filter_map(|w| code.find(w))
    .min()
    .unwrap_or(code.len());
    let key = (sql[from..from + cut].to_string(), operand.to_string());
    let version = *version.get_or_insert_with(|| DDL_GENERATION.load(std::sync::atomic::Ordering::Acquire) as i64);
    let hit = DECLS.with(|d| {
        let mut d = d.borrow_mut();
        if d.0 != version || d.1.len() > 4096 {
            *d = (version, DeclCache::new());
        }
        d.1.get(&key).cloned()
    });
    if let Some(decl) = hit {
        return decl;
    }
    let decl = column_decl(q, sql, pos, operand);
    DECLS.with(|d| d.borrow_mut().1.insert(key, decl.clone()));
    decl
}

/// What a column is declared, if operand, read at pos, is one.
pub(crate) fn column_decl(q: &Connection, sql: &str, pos: usize, operand: &str) -> Option<String> {
    let (from, end) = enclosing_from(sql, pos)?;
    let probe = format!("SELECT {operand} {}", &sql[from..end]);
    let mut script = Script::new(q, &probe);
    let stmt = script.next().ok()??;
    let (_, decls) = stmt.column_info();
    decls.into_iter().next().filter(|d| !d.is_empty())
}

/// Whether a declared type holds an instant.
fn is_instant_decl(decl: &str) -> bool {
    decl.starts_with("TIMESTAMP") && (decl.contains("TZ") || decl.contains("WITH TIME ZONE"))
}

/// Whether operand, read at pos, is a column declared a timestamp without a zone.
fn is_timestamp_column(q: &Connection, sql: &str, pos: usize, operand: &str) -> bool {
    column_decl(q, sql, pos, operand).is_some_and(|d| d.starts_with("TIMESTAMP") && !is_instant_decl(&d))
}

// A timestamp with an offset, whole, in a literal or a bound text.
const OFFSET_STAMP: &str = r"\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}(?::?\d{2})?)";
const NAME: &str = r#"(?:"[^"]+"|[A-Za-z_][\w$]*)(?:\.(?:"[^"]+"|[A-Za-z_][\w$]*))?"#;
const CMP: &str = r"=|<>|!=|<=|>=|<|>";

lazy_regex!(RE_OFFSET_VALUE, &["^", OFFSET_STAMP, "$"].concat());
lazy_regex!(RE_OFFSET_LITERAL, &["'", OFFSET_STAMP, "'"].concat());
lazy_regex!(
    RE_COLUMN_THEN_VALUE,
    &["(", NAME, r")\s*(?:", CMP, r")\s*('", OFFSET_STAMP, r"'|\$\d+)"].concat()
);
lazy_regex!(
    RE_VALUE_THEN_COLUMN,
    &["('", OFFSET_STAMP, r"'|\$\d+)\s*(?:", CMP, r")\s*(", NAME, ")"].concat()
);

/// What a comparison with a column of a time type needs: the value read as that type,
/// and not as the text it is to anything else. A timestamptz is stored as bare UTC, so
/// `tz < '2024-01-02 03:04:05+02'` compares nothing as written. A column of any other
/// type, or an expression, leaves the value as the text it is.
pub(crate) fn resolve_zone_values<'a>(q: &Connection, query: &'a str, args: &[Value]) -> Cow<'a, str> {
    let bound = args
        .iter()
        .any(|a| matches!(a, Value::Text(s) if RE_OFFSET_VALUE.is_match(s)));
    let written = query.contains(':') && RE_OFFSET_LITERAL.is_match(query);
    if !bound && !written {
        return Cow::Borrowed(query);
    }
    let is_offset_value = |value: &str| {
        value.starts_with('\'')
            || value[1..].parse::<usize>().is_ok_and(
                |n| matches!(args.get(n.wrapping_sub(1)), Some(Value::Text(s)) if RE_OFFSET_VALUE.is_match(s)),
            )
    };
    // Each value's span and what to write there, found on one text and applied last
    // to first, so a span stays good.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut note = |at: usize, operand: &str, value: regex::Match<'_>| {
        if !is_offset_value(value.as_str()) || edits.iter().any(|e| e.0 == value.start()) {
            return;
        }
        let Some(decl) = column_decl(q, query, at, operand) else {
            return;
        };
        let ty = match decl.as_str() {
            d if is_instant_decl(d) => "timestamptz",
            d if d.starts_with("TIMESTAMP") => "timestamp",
            _ => return,
        };
        let cast = format!("overlite_cast({}, '{ty}')", value.as_str());
        edits.push((value.start(), value.end(), cast));
    };
    for m in RE_COLUMN_THEN_VALUE.captures_iter(query) {
        if let (Some(name), Some(value)) = (m.get(1), m.get(2)) {
            note(name.start(), name.as_str(), value);
        }
    }
    for m in RE_VALUE_THEN_COLUMN.captures_iter(query) {
        if let (Some(value), Some(name)) = (m.get(1), m.get(2)) {
            note(name.start(), name.as_str(), value);
        }
    }
    if edits.is_empty() {
        return Cow::Borrowed(query);
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut sql = query.to_string();
    for (start, end, text) in edits {
        sql.replace_range(start..end, &text);
    }
    Cow::Owned(sql)
}
