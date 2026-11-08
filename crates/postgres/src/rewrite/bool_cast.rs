//! A cast to boolean is pg_bool(x), the engine's boolin. SQLite's CAST(x AS
//! boolean) gives the value NUMERIC affinity, which reads every word as 0:
//! 'true'::boolean was false, without an error.
//!
//! Only in statements that run now. A DDL's default or check keeps SQLite's own
//! CAST, so the database file still opens without overlite's functions.

use crate::rewrite::casts::is_operand_char;
use crate::rewrite::utf8_string;
use crate::session::utility::first_word_upper;
use crate::types::param_types::closing;
use overlite_core::text::trim_start_chars;

/// Turns CAST(x AS bool) and CAST(x AS boolean) into pg_bool(x). It runs after
/// rewrite_casts, which writes x::boolean as a CAST.
pub(crate) fn rewrite_bool_casts(sql: &str) -> String {
    let h = first_word_upper(sql);
    if h == "CREATE" || h == "ALTER" {
        return sql.to_string();
    }
    if !sql.to_uppercase().contains("BOOL") {
        return sql.to_string();
    }
    let s = sql.as_bytes();
    let mut b: Vec<u8> = Vec::with_capacity(s.len());
    let mut quoted = false;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' {
            quoted = !quoted;
        }
        if !quoted && is_cast_at(sql, i) {
            let Some(open) = sql[i..].find('(').map(|o| o + i) else {
                break;
            };
            let end = closing(&sql[open..]).map(|e| open + e);
            if let Some(end) = end.filter(|e| *e > open)
                && let Some(operand) = bool_cast_operand(&sql[open + 1..end])
            {
                b.extend_from_slice(b"pg_bool(");
                b.extend_from_slice(rewrite_bool_casts(&operand).as_bytes());
                b.push(b')');
                i = end + 1;
                continue;
            }
        }
        b.push(c);
        i += 1;
    }
    utf8_string(b)
}

/// Reports whether the word CAST, followed by its parenthesis, starts at i.
pub(crate) fn is_cast_at(sql: &str, i: usize) -> bool {
    let b = sql.as_bytes();
    if i + 4 > b.len() || !b[i..i + 4].eq_ignore_ascii_case(b"CAST") {
        return false;
    }
    if i > 0 && is_operand_char(b[i - 1]) {
        return false;
    }
    trim_start_chars(&sql[i + 4..], " \t\r\n").starts_with('(')
}

/// The operand of a CAST's inside "x AS type" when the type is a boolean.
fn bool_cast_operand(inside: &str) -> Option<String> {
    let at = last_top_level_as(inside)?;
    match inside[at + 2..].trim().to_lowercase().as_str() {
        "bool" | "boolean" | "pg_catalog.bool" | "pg_catalog.boolean" => Some(inside[..at].trim().to_string()),
        _ => None,
    }
}

/// The index of the last AS keyword outside parentheses and quotes.
pub(crate) fn last_top_level_as(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut quoted = false;
    let mut last = None;
    for i in 0..b.len() {
        let c = b[i];
        if c == b'\'' {
            quoted = !quoted;
        } else if quoted {
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
        } else if depth == 0
            && i + 2 <= b.len()
            && b[i..i + 2].eq_ignore_ascii_case(b"AS")
            && (i == 0 || !is_operand_char(b[i - 1]))
            && (i + 2 == b.len() || !is_operand_char(b[i + 2]))
        {
            last = Some(i);
        }
    }
    last
}
