//! SQL-standard function syntax: TRIM ... FROM, string_agg and the niladic functions.

use overlite_core::lazy_regex;

pub(crate) fn has_word_at(low: &str, p: usize, word: &str) -> bool {
    let l = low.as_bytes();
    let w = word.as_bytes();
    if p + w.len() > l.len() || &l[p..p + w.len()] != w {
        return false;
    }
    let n = p + w.len();
    n >= l.len() || l[n] == b' ' || l[n] == b'\t' || l[n] == b'\n' || l[n] == b'(' || l[n] == b'\''
}

pub(crate) fn skip_spaces(sql: &str, p: usize) -> usize {
    let b = sql.as_bytes();
    let mut p = p;
    while p < b.len() && (b[p] == b' ' || b[p] == b'\t' || b[p] == b'\n' || b[p] == b'\r') {
        p += 1;
    }
    p
}

// Matches SQL niladic special functions that Postgres allows without
// parentheses but SQLite requires as calls.
lazy_regex!(
    RE_NILADIC,
    r"(?i)\b(current_user|session_user|current_role|current_catalog|current_schema|current_database)\b"
);

// Maps Postgres string_agg() onto SQLite's group_concat().
lazy_regex!(RE_STRING_AGG, r"(?i)\bstring_agg\s*\(");

// string_agg(DISTINCT x, sep): SQLite takes DISTINCT with one argument only.
lazy_regex!(RE_STRING_AGG_DISTINCT, r"(?i)\bstring_agg\s*\(\s*distinct\b");
