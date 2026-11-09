//! SQL-standard function syntax: TRIM ... FROM, string_agg and the niladic functions.

pub(crate) fn skip_spaces(sql: &str, p: usize) -> usize {
    let b = sql.as_bytes();
    let mut p = p;
    while p < b.len() && (b[p] == b' ' || b[p] == b'\t' || b[p] == b'\n' || b[p] == b'\r') {
        p += 1;
    }
    p
}
