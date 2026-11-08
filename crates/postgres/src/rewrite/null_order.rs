//! Where NULLs sort. Postgres takes a NULL for larger than any value, so `ORDER BY x`
//! puts NULLs last and `ORDER BY x DESC` first; SQLite takes it for smaller, the other
//! way round both times. Each sort key that does not say NULLS FIRST or LAST is given
//! the one Postgres would use. SQLite still walks an index for it.

use crate::rewrite::tokens::{CodeTok, CodeTokKind, code_tokens};

/// Words that end an ORDER BY at its own depth.
const ENDS: [&str; 13] = [
    "limit",
    "offset",
    "fetch",
    "for",
    "union",
    "except",
    "intersect",
    "rows",
    "range",
    "groups",
    "window",
    "returning",
    "on",
];

/// Writes the NULLS FIRST or LAST of each sort key of every ORDER BY that does not.
pub(crate) fn rewrite_null_order(sql: &str) -> String {
    if !sql.as_bytes().windows(5).any(|w| w.eq_ignore_ascii_case(b"order")) {
        return sql.to_string();
    }
    let toks = code_tokens(sql);
    let mut inserts: Vec<(usize, &'static str)> = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        if !(toks[i].is_word(sql, "order") && toks[i + 1].is_word(sql, "by")) {
            i += 1;
            continue;
        }
        let depth = toks[i].depth;
        let mut start = i + 2;
        let mut j = start;
        loop {
            let at_end = j >= toks.len()
                || toks[j].depth < depth
                || (toks[j].depth == depth
                    && (toks[j].is_punct(sql, ";") || ENDS.iter().any(|w| toks[j].is_word(sql, w))));
            if at_end || (toks[j].depth == depth && toks[j].is_punct(sql, ",")) {
                if let Some(at) = null_order_for(sql, &toks[start..j], depth) {
                    inserts.push(at);
                }
                if at_end {
                    break;
                }
                start = j + 1;
            }
            j += 1;
        }
        i = j;
    }
    if inserts.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + inserts.len() * 12);
    let mut at = 0;
    for (pos, words) in inserts {
        out.push_str(&sql[at..pos]);
        out.push_str(words);
        at = pos;
    }
    out.push_str(&sql[at..]);
    out
}

/// Where the NULLS of one sort key goes, and what it says; None for a key that says
/// it already, or sorts by an operator (USING), or is empty.
fn null_order_for(sql: &str, key: &[CodeTok], depth: i32) -> Option<(usize, &'static str)> {
    let last = key.last()?;
    let own = |w: &str| key.iter().any(|t| t.depth == depth && t.is_word(sql, w));
    if own("nulls") || own("using") {
        return None;
    }
    let desc = last.kind == CodeTokKind::Word && last.depth == depth && last.is_word(sql, "desc");
    Some((last.span.end, if desc { " NULLS FIRST" } else { " NULLS LAST" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn null_order_cases() {
        assert_rewrites(
            rewrite_null_order,
            &[
                ("SELECT a FROM t ORDER BY a", "SELECT a FROM t ORDER BY a NULLS LAST"),
                (
                    "SELECT a FROM t ORDER BY a DESC, b ASC, c LIMIT 1",
                    "SELECT a FROM t ORDER BY a DESC NULLS FIRST, b ASC NULLS LAST, c NULLS LAST LIMIT 1",
                ),
                (
                    "SELECT a FROM t ORDER BY a DESC NULLS LAST, f(b, c) desc",
                    "SELECT a FROM t ORDER BY a DESC NULLS LAST, f(b, c) desc NULLS FIRST",
                ),
                (
                    "SELECT string_agg(a, ',' ORDER BY b DESC), row_number() OVER (PARTITION BY c ORDER BY d ROWS UNBOUNDED PRECEDING) FROM t",
                    "SELECT string_agg(a, ',' ORDER BY b DESC NULLS FIRST), row_number() OVER (PARTITION BY c ORDER BY d NULLS LAST ROWS UNBOUNDED PRECEDING) FROM t",
                ),
                (
                    "SELECT a FROM (SELECT a FROM t ORDER BY a) s ORDER BY 1 FOR UPDATE",
                    "SELECT a FROM (SELECT a FROM t ORDER BY a NULLS LAST) s ORDER BY 1 NULLS LAST FOR UPDATE",
                ),
                ("SELECT 'order by x'", "SELECT 'order by x'"),
                ("CREATE INDEX i ON t (a DESC)", "CREATE INDEX i ON t (a DESC)"),
            ],
        );
    }
}
