//! LIKE and ILIKE. Postgres' LIKE tells case apart and takes a backslash as its escape
//! unless ESCAPE names another (or '', none); SQLite's LIKE folds ASCII case and has no
//! escape but the one ESCAPE names. So LIKE becomes GLOB, which tells case apart, over
//! the pattern `overlite_like_glob` writes, and ILIKE stays SQLite's LIKE, over the
//! pattern `overlite_ilike_pattern` writes with a backslash for its escape. Both are
//! SQLite's own matching, row after row; the pattern is made once, a constant call.
//! Unlike the passes of rewrite_full, this one runs after the fast path too, where a
//! plain LIKE takes it.

use crate::rewrite::tokens::{CodeTok, CodeTokKind, code_tokens};

/// Words that end the pattern of a LIKE at its depth.
const ENDS: [&str; 26] = [
    "and",
    "or",
    "escape",
    "then",
    "else",
    "when",
    "end",
    "order",
    "group",
    "having",
    "limit",
    "offset",
    "union",
    "except",
    "intersect",
    "returning",
    "from",
    "where",
    "as",
    "is",
    "not",
    "collate",
    "on",
    "join",
    "for",
    "window",
];

/// Writes each `[NOT] LIKE p [ESCAPE e]` and `[NOT] ILIKE p [ESCAPE e]`.
pub(crate) fn rewrite_like(sql: &str) -> String {
    if !sql.as_bytes().windows(4).any(|w| w.eq_ignore_ascii_case(b"like")) {
        return sql.to_string();
    }
    let toks = code_tokens(sql);
    let mut out = String::with_capacity(sql.len() + 48);
    let mut at = 0;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        let ilike = t.is_word(sql, "ilike");
        if !(ilike || t.is_word(sql, "like")) {
            i += 1;
            continue;
        }
        // LIKE ANY (...) and its kin are left as they are.
        let next = toks.get(i + 1);
        if next.is_none_or(|n| ["any", "all", "some"].iter().any(|w| n.is_word(sql, w))) {
            i += 1;
            continue;
        }
        let depth = t.depth;
        let start = i + 1;
        let mut end = start;
        while end < toks.len() && !ends_pattern(sql, &toks[end], depth) {
            end += 1;
        }
        if end == start {
            i += 1;
            continue;
        }
        let pattern = &sql[toks[start].span.start..toks[end - 1].span.end];
        let mut stop = toks[end - 1].span.end;
        let mut escape = "'\\'";
        if end + 1 < toks.len() && toks[end].is_word(sql, "escape") && toks[end].depth == depth {
            escape = &sql[toks[end + 1].span.clone()];
            stop = toks[end + 1].span.end;
            end += 2;
        }
        out.push_str(&sql[at..t.span.start]);
        if ilike {
            out.push_str("LIKE overlite_ilike_pattern(");
            out.push_str(pattern);
            out.push_str(", ");
            out.push_str(escape);
            out.push_str(") ESCAPE '\\'");
        } else {
            out.push_str("GLOB overlite_like_glob(");
            out.push_str(pattern);
            out.push_str(", ");
            out.push_str(escape);
            out.push(')');
        }
        at = stop;
        i = end;
    }
    if at == 0 {
        return sql.to_string();
    }
    out.push_str(&sql[at..]);
    out
}

/// Whether t, past a LIKE at depth, is no longer its pattern.
fn ends_pattern(sql: &str, t: &CodeTok, depth: i32) -> bool {
    if t.depth < depth {
        return true;
    }
    if t.depth > depth {
        return false;
    }
    match t.kind {
        CodeTokKind::Word => ENDS.iter().any(|w| t.is_word(sql, w)),
        CodeTokKind::Punct => matches!(&sql[t.span.clone()], "," | ";" | "=" | "<" | ">" | "!"),
        CodeTokKind::Value => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn like_cases() {
        assert_rewrites(
            rewrite_like,
            &[
                (
                    "SELECT a FROM t WHERE a LIKE 'x%' AND b NOT LIKE $1",
                    "SELECT a FROM t WHERE a GLOB overlite_like_glob('x%', '\\') AND b NOT GLOB overlite_like_glob($1, '\\')",
                ),
                (
                    "SELECT a ILIKE '%' || b || '%' ESCAPE '#', c FROM t",
                    "SELECT a LIKE overlite_ilike_pattern('%' || b || '%', '#') ESCAPE '\\', c FROM t",
                ),
                (
                    "SELECT (a LIKE upper(b)) = true",
                    "SELECT (a GLOB overlite_like_glob(upper(b), '\\')) = true",
                ),
                (
                    "SELECT a FROM t WHERE a LIKE ANY ('{x}')",
                    "SELECT a FROM t WHERE a LIKE ANY ('{x}')",
                ),
                ("SELECT 'a like b'", "SELECT 'a like b'"),
            ],
        );
    }
}
