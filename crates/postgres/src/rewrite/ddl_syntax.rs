//! Column and index syntax in DDL: SERIAL, USING `<method>` and a key's deferral.

use crate::rewrite::tokens::code_tokens;
use overlite_core::lazy_regex;

// Matches the access-method clause "USING <method>" that pg_dump
// puts in a CREATE INDEX (SQLite has no such clause and only btree anyway).
// IF EXISTS and ONLY, which SQLite does not take. The session has settled the first
// by now, and ONLY is about inheritance, which does not exist here.
lazy_regex!(
    RE_ALTER_TABLE_MODIFIERS,
    r"(?is)^(\s*ALTER\s+TABLE\s+)(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?"
);

lazy_regex!(RE_INDEX_USING, r"(?i)\bUSING\s+\w+\s+(\()");

lazy_regex!(RE_INDEX_USING_METHOD, r"(?i)\bUSING\s+(\w+)\s+\(");

/// What a key column of a NULLS NOT DISTINCT index is wrapped in: a NULL becomes a
/// blob no column holds, so two NULLs are one key, as Postgres has them.
pub(crate) const NULL_KEY: &str = "x'6f7665726c6974652d6e756c6c'";

// SERIAL/BIGSERIAL/SMALLSERIAL are Postgres auto-increment integers. In SQLite,
// an INTEGER PRIMARY KEY is the rowid and auto-increments, so we map the common
// "SERIAL PRIMARY KEY" to it (with AUTOINCREMENT to avoid rowid reuse, matching
// a sequence). A bare SERIAL becomes INTEGER.
//
/// rewrite_deferrable_keys on any text: a CREATE TABLE, or the constraint an ALTER
/// TABLE adds.
pub(crate) fn mark_key_deferral(sql: &str) -> String {
    let toks = code_tokens(sql);
    let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
    if !toks
        .iter()
        .any(|t| t.is_word(sql, "deferrable") || t.is_word(sql, "initially"))
    {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + 16);
    let mut at = 0;
    let mut i = 0;
    while i < toks.len() {
        let start = i;
        let (mut deferrable, mut deferred, mut not) = (false, false, false);
        let mut j = i;
        loop {
            if is(j, "not") && is(j + 1, "deferrable") {
                not = true;
                j += 2;
            } else if is(j, "deferrable") {
                deferrable = true;
                j += 1;
            } else if is(j, "initially") && (is(j + 1, "deferred") || is(j + 1, "immediate")) {
                deferred = is(j + 1, "deferred");
                j += 2;
            } else {
                break;
            }
        }
        if j == start {
            i += 1;
            continue;
        }
        i = j;
        // The constraint the clause belongs to: the nearest key word before it in its item.
        let depth = toks[start].depth;
        let owner = (0..start)
            .rev()
            .take_while(|&k| toks[k].depth >= depth && !(toks[k].depth == depth && toks[k].is_punct(sql, ",")))
            .filter(|&k| toks[k].depth == depth)
            .find_map(|k| {
                ["references", "unique", "primary", "check", "exclude"]
                    .into_iter()
                    .find(|w| is(k, w))
            });
        if !matches!(owner, Some("unique" | "primary")) {
            continue;
        }
        out.push_str(&sql[at..toks[start].span.start]);
        if !not && (deferrable || deferred) {
            out.push_str(if deferred {
                "/*overlite deferrable initially deferred*/"
            } else {
                "/*overlite deferrable*/"
            });
        }
        at = toks[j - 1].span.end;
    }
    if at == 0 {
        return sql.to_string();
    }
    out.push_str(&sql[at..]);
    out
}

// Postgres names the table a trigger is on; SQLite takes the trigger's name alone.
lazy_regex!(
    RE_DROP_TRIGGER,
    r#"(?is)^(\s*DROP\s+TRIGGER\s+(?:IF\s+EXISTS\s+)?(?:"[^"]+"|[\w.]+))\s+ON\s+(?:"[^"]+"|[\w.]+)(?:\s+(?:CASCADE|RESTRICT))?\s*;?\s*$"#
);
