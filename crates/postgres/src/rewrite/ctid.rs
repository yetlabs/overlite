//! ctid is Postgres' physical row identifier, a system column every table has and
//! no table may declare. Migrations reach for it to tell duplicate rows apart:
//! Prisma's Evolution API removes duplicate contacts with
//! `DELETE ... WHERE ctid NOT IN (SELECT min(ctid) ... GROUP BY ...)` before adding
//! a unique index. SQLite's rowid plays the same part, so a bare or qualified ctid
//! is read as one. A quoted name is one someone chose, whatever it holds, and is left
//! alone.
//!
//! The rowid must not show in a result, where a client reads the column by the
//! name it asked for: SQLite labels it rowid, or after the column that aliases it.
//! So the outermost select list and RETURNING get the label back. Only those: a
//! subquery's columns are read by the statement around it, which says rowid too.

use crate::rewrite::tokens::{CodeTok, CodeTokKind, code_tokens};
use std::collections::HashSet;
use std::ops::Range;

pub(crate) fn rewrite_ctid(sql: &str) -> String {
    if !sql.as_bytes().windows(4).any(|w| w.eq_ignore_ascii_case(b"ctid")) {
        return sql.to_string();
    }
    let toks = code_tokens(sql);
    let is_ctid = |t: &CodeTok| t.is_word(sql, "ctid");
    let (labelled, kept) = result_labels(sql, &toks);
    let mut out = String::with_capacity(sql.len() + 16);
    let mut at = 0;
    for (i, t) in toks.iter().enumerate() {
        if is_ctid(t) && !kept.contains(&i) {
            out.push_str(&sql[at..t.span.start]);
            out.push_str("rowid");
            if labelled.contains(&i) {
                out.push_str(" AS ctid");
            }
            at = t.span.end;
        }
    }
    out.push_str(&sql[at..]);
    out
}

/// The words that end the select list.
const AFTER_SELECT_LIST: &[&str] = &[
    "from",
    "where",
    "group",
    "having",
    "window",
    "order",
    "limit",
    "offset",
    "fetch",
    "for",
    "union",
    "intersect",
    "except",
];

/// The words after which a value is an operand rather than followed by a label.
const BEFORE_OPERAND: &[&str] = &[
    "and", "or", "not", "is", "in", "like", "ilike", "glob", "regexp", "match", "between", "case", "when", "then",
    "else",
];

/// Which ctid tokens label a column of the result: those that are the whole column
/// and read the rowid, then those that name it, which stay as they are. A name kept
/// in the select list is also what a plain ORDER BY ctid means, as Postgres reads it.
fn result_labels(sql: &str, toks: &[CodeTok]) -> (HashSet<usize>, HashSet<usize>) {
    let (mut labelled, mut kept) = (HashSet::new(), HashSet::new());
    if !matches!(toks.first(), Some(t) if ["select", "with", "insert", "update", "delete"].iter().any(|w| t.is_word(sql, w)))
    {
        return (labelled, kept);
    }
    let top_word = |i: usize, words: &[&str]| toks[i].depth == 0 && words.iter().any(|w| toks[i].is_word(sql, w));
    let select = (0..toks.len()).find(|&i| top_word(i, &["select"]));
    let returning = (0..toks.len()).rfind(|&i| top_word(i, &["returning"]));

    if let Some(select) = select {
        let mut start = select + 1;
        if start < toks.len() && top_word(start, &["distinct", "all"]) {
            start += 1;
        }
        let end = (start..toks.len())
            .find(|&i| top_word(i, AFTER_SELECT_LIST) || toks[i].is_punct(sql, ";"))
            .unwrap_or(toks.len());
        label_list(sql, toks, start..end, &mut labelled, &mut kept);
    }
    if let Some(returning) = returning {
        let end = (returning..toks.len())
            .find(|&i| toks[i].is_punct(sql, ";"))
            .unwrap_or(toks.len());
        label_list(sql, toks, returning + 1..end, &mut labelled, &mut kept);
    }
    if !labelled.is_empty() || !kept.is_empty() {
        for i in (0..toks.len()).filter(|&i| top_word(i, &["order"])) {
            keep_order_terms(sql, toks, i, &mut kept);
        }
    }
    (labelled, kept)
}

fn label_list(
    sql: &str,
    toks: &[CodeTok],
    list: Range<usize>,
    labelled: &mut HashSet<usize>,
    kept: &mut HashSet<usize>,
) {
    let mut start = list.start;
    for i in list.clone() {
        if i + 1 == list.end || (toks[i + 1].depth == 0 && toks[i + 1].is_punct(sql, ",")) {
            label_item(sql, toks, start..i + 1, labelled, kept);
            start = i + 2;
        }
    }
}

fn label_item(
    sql: &str,
    toks: &[CodeTok],
    item: Range<usize>,
    labelled: &mut HashSet<usize>,
    kept: &mut HashSet<usize>,
) {
    let Some(last) = item.end.checked_sub(1).filter(|&l| l >= item.start) else {
        return;
    };
    if toks[last].depth != 0 || !toks[last].is_word(sql, "ctid") {
        return;
    }
    // ctid, t.ctid or "t".ctid: the column itself.
    let chain = toks[item.clone()].iter().enumerate().all(|(n, t)| {
        if n % 2 == 0 {
            t.kind != CodeTokKind::Punct
        } else {
            t.is_punct(sql, ".")
        }
    });
    if chain && (item.end - item.start) % 2 == 1 {
        labelled.insert(last);
        return;
    }
    let prev = &toks[last - 1];
    let ends_value = match prev.kind {
        CodeTokKind::Value => true,
        CodeTokKind::Punct => prev.is_punct(sql, ")") || prev.is_punct(sql, "]"),
        CodeTokKind::Word => !BEFORE_OPERAND.iter().any(|w| prev.is_word(sql, w)),
    };
    if ends_value {
        kept.insert(last);
    }
}

/// Keeps the ORDER BY terms from toks[order] on that are a bare ctid.
fn keep_order_terms(sql: &str, toks: &[CodeTok], order: usize, kept: &mut HashSet<usize>) {
    for i in order + 1..toks.len() {
        let top = toks[i].depth == 0;
        if !top || !toks[i].is_word(sql, "ctid") {
            continue;
        }
        let opens = toks[i - 1].is_word(sql, "by") || toks[i - 1].is_punct(sql, ",");
        let closes = toks.get(i + 1).is_none_or(|t| {
            t.is_punct(sql, ",")
                || t.is_punct(sql, ";")
                || ["asc", "desc", "nulls", "limit", "offset", "fetch", "for"]
                    .iter()
                    .any(|w| t.is_word(sql, w))
        });
        if opens && closes {
            kept.insert(i);
        }
    }
}
