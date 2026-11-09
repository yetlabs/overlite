//! SQL-standard function syntax: TRIM ... FROM, string_agg and the niladic functions.

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
