//! What a tsvector column changes in a statement, which its text does not say: `length`
//! of one counts its lexemes, and `||` of two concatenates them, positions shifted.
//! SQLite knows a column's declared type: a probe of the operand over the statement's
//! FROM, or the UPDATE's table, is prepared and never stepped, as for AT TIME ZONE.

use crate::blank::blank_literals;
use crate::resolve::time_zone::column_decl;
use crate::sqlite::Script;
use overlite_core::lazy_regex;
use rusqlite::Connection;
use std::borrow::Cow;

const NAME: &str = r#"(?:"[^"]+"|[A-Za-z_][\w$]*)(?:\.(?:"[^"]+"|[A-Za-z_][\w$]*))?"#;

lazy_regex!(RE_LENGTH, &[r"(?i)\blength\(\s*(", NAME, r")\s*\)"].concat());
lazy_regex!(RE_CONCAT, &["(", NAME, r")\s*\|\|\s*(", NAME, ")"].concat());
lazy_regex!(
    RE_WRITE_TARGET,
    &[r"(?is)^\s*(?:UPDATE|DELETE\s+FROM)\s+(", NAME, ")"].concat()
);

/// `length(col)` and `col || col` over tsvector columns, made ts_length and ts_concat.
pub(crate) fn resolve_tsvectors<'a>(q: &Connection, query: &'a str) -> Cow<'a, str> {
    // Every statement passes here: a byte search, no allocation, for the rest.
    if !query.contains("||") && !contains_length(query) {
        return Cow::Borrowed(query);
    }
    let code = blank_literals(query);
    let has_length = contains_length(&code);
    let mut sql = query.to_string();
    if has_length {
        let mut from = 0;
        loop {
            let code = blank_literals(&sql).into_owned();
            let Some(m) = RE_LENGTH.captures_at(&code, from) else {
                break;
            };
            let (whole, col) = (
                m.get(0).map_or(0..0, |x| x.range()),
                m.get(1).map_or(0..0, |x| x.range()),
            );
            from = whole.end;
            if is_tsvector(q, &sql, whole.start, &sql[col.clone()]) {
                let replaced = format!("ts_length({})", &sql[col]);
                sql = [&sql[..whole.start], &replaced, &sql[whole.end..]].concat();
                from = whole.start + replaced.len();
            }
        }
    }
    if code.contains("||") {
        let mut from = 0;
        loop {
            let code = blank_literals(&sql).into_owned();
            let Some(m) = RE_CONCAT.captures_at(&code, from) else {
                break;
            };
            let whole = m.get(0).map_or(0..0, |x| x.range());
            let (a, b) = (
                m.get(1).map_or(0..0, |x| x.range()),
                m.get(2).map_or(0..0, |x| x.range()),
            );
            from = whole.end;
            // A name inside another, or one more `||` on either side, is left alone.
            let before = code[..whole.start].trim_end();
            if before.ends_with("||") || code[whole.end..].trim_start().starts_with("||") {
                continue;
            }
            if is_tsvector(q, &sql, whole.start, &sql[a.clone()]) || is_tsvector(q, &sql, whole.start, &sql[b.clone()])
            {
                let replaced = format!("ts_concat({}, {})", &sql[a], &sql[b]);
                sql = [&sql[..whole.start], &replaced, &sql[whole.end..]].concat();
                from = whole.start + replaced.len();
            }
        }
    }
    if sql == query {
        Cow::Borrowed(query)
    } else {
        Cow::Owned(sql)
    }
}

/// Whether operand, read at pos, is a column declared tsvector.
fn is_tsvector(q: &Connection, sql: &str, pos: usize, operand: &str) -> bool {
    let decl = column_decl(q, sql, pos, operand).or_else(|| {
        let target = RE_WRITE_TARGET.captures(sql)?.get(1)?.as_str().to_string();
        let probe = format!("SELECT {operand} FROM {target}");
        let mut script = Script::new(q, &probe);
        let stmt = script.next().ok()??;
        let (_, decls) = stmt.column_info();
        decls.into_iter().next().filter(|d| !d.is_empty())
    });
    decl.is_some_and(|d| d.trim().eq_ignore_ascii_case("tsvector"))
}

/// Whether `length(` is in s, case ignored.
fn contains_length(s: &str) -> bool {
    s.as_bytes().windows(7).any(|w| w.eq_ignore_ascii_case(b"length("))
}
