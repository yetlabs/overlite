//! Column and index syntax in DDL: SERIAL, USING `<method>` and a key's deferral.

use crate::rewrite::tokens::{CodeTokKind, code_tokens};
use crate::session::utility::first_word_upper;
use overlite_core::lazy_regex;
use overlite_core::text::{eq_fold, replace_all};

// Matches the access-method clause "USING <method>" that pg_dump
// puts in a CREATE INDEX (SQLite has no such clause and only btree anyway).
// IF EXISTS and ONLY, which SQLite does not take. The session has settled the first
// by now, and ONLY is about inheritance, which does not exist here.
lazy_regex!(
    RE_ALTER_TABLE_MODIFIERS,
    r"(?is)^(\s*ALTER\s+TABLE\s+)(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?"
);

pub(crate) fn rewrite_alter_table_head(sql: &str) -> String {
    replace_all(&RE_ALTER_TABLE_MODIFIERS, sql, "$1")
}

lazy_regex!(RE_INDEX_USING, r"(?i)\bUSING\s+\w+\s+(\()");

/// Drops "USING btree" (etc.) from a CREATE INDEX so it runs on SQLite, e.g.
/// restoring a pg_dump.
pub(crate) fn rewrite_index_using(sql: &str) -> String {
    if !eq_fold(&first_word_upper(sql), "CREATE") {
        return sql.to_string();
    }
    if !sql.to_uppercase().contains("INDEX") {
        return sql.to_string();
    }
    let sql = index_options(sql);
    let sql = move_index_include(&index_nulls_not_distinct(&sql));
    let method = RE_INDEX_USING_METHOD
        .captures(&sql)
        .map(|m| m[1].to_ascii_lowercase())
        .filter(|m| m != "btree");
    let sql = rewrite_index_opclass(&replace_all(&RE_INDEX_USING, &sql, "$1"));
    match method {
        // The access method asked for, for pg_class.relam: every index is a btree here.
        Some(m) => {
            let body = sql.trim_end();
            let (body, semi) = body.strip_suffix(';').map_or((body, ""), |b| (b.trim_end(), ";"));
            [body, " /*overlite using ", &m, "*/", semi].concat()
        }
        None => sql,
    }
}

lazy_regex!(RE_INDEX_USING_METHOD, r"(?i)\bUSING\s+(\w+)\s+\(");

/// Takes out of a CREATE INDEX what SQLite's has no use for: CONCURRENTLY, as SQLite
/// builds an index under the write lock anyway, and the storage parameters of `WITH
/// (fillfactor = 70)`, which tune Postgres' pages.
fn index_options(sql: &str) -> String {
    let toks = code_tokens(sql);
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for (i, t) in toks.iter().enumerate() {
        if t.depth != 0 {
            continue;
        }
        if t.is_word(sql, "concurrently") && i <= 3 {
            out.push_str(sql[at..t.span.start].trim_end());
            at = t.span.end;
        } else if t.is_word(sql, "with") && toks.get(i + 1).is_some_and(|n| n.is_punct(sql, "(")) {
            let Some(close) = (i + 2..toks.len()).find(|&j| toks[j].depth == 0 && toks[j].is_punct(sql, ")")) else {
                continue;
            };
            out.push_str(sql[at..t.span.start].trim_end());
            at = toks[close].span.end;
        }
    }
    if at == 0 {
        return sql.to_string();
    }
    out.push_str(&sql[at..]);
    out
}

/// Takes the TABLESPACE clauses out of a CREATE TABLE or CREATE INDEX: `TABLESPACE
/// name` and a constraint's `USING INDEX TABLESPACE name`. SQLite keeps a table in
/// its database's file, which is all a tablespace says.
pub(crate) fn rewrite_tablespace(sql: &str) -> String {
    if !eq_fold(&first_word_upper(sql), "CREATE") {
        return sql.to_string();
    }
    let toks = code_tokens(sql);
    if !toks.iter().any(|t| t.is_word(sql, "tablespace")) {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for (i, t) in toks.iter().enumerate() {
        if !t.is_word(sql, "tablespace") || i == 1 || toks.get(i + 1).is_none() {
            continue;
        }
        let using = i >= 2 && toks[i - 2].is_word(sql, "using") && toks[i - 1].is_word(sql, "index");
        let start = if using { toks[i - 2].span.start } else { t.span.start };
        if start < at {
            continue;
        }
        out.push_str(sql[at..start].trim_end());
        at = toks[i + 1].span.end;
    }
    if at == 0 {
        return sql.to_string();
    }
    out.push_str(&sql[at..]);
    out
}

/// Gives a citext column, in CREATE TABLE or ALTER TABLE, SQLite's NOCASE
/// collation, so that it compares without case as citext does. NOCASE folds ASCII
/// letters only.
pub(crate) fn rewrite_citext(sql: &str) -> String {
    let head = first_word_upper(sql);
    if !eq_fold(&head, "CREATE") && !eq_fold(&head, "ALTER") {
        return sql.to_string();
    }
    let toks = code_tokens(sql);
    if !toks.get(1).is_some_and(|t| t.is_word(sql, "table")) {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + 16);
    let mut at = 0;
    for (i, t) in toks.iter().enumerate() {
        if i < 2 || !t.is_word(sql, "citext") || toks.get(i + 1).is_some_and(|n| n.is_punct(sql, "[")) {
            continue;
        }
        // A type follows a column's name, or TYPE in ALTER COLUMN.
        let prev = &toks[i - 1];
        if !matches!(prev.kind, CodeTokKind::Word | CodeTokKind::Value) {
            continue;
        }
        out.push_str(&sql[at..t.span.end]);
        out.push_str(" COLLATE NOCASE");
        at = t.span.end;
    }
    if at == 0 {
        return sql.to_string();
    }
    out.push_str(&sql[at..]);
    out
}

/// What a key column of a NULLS NOT DISTINCT index is wrapped in: a NULL becomes a
/// blob no column holds, so two NULLs are one key, as Postgres has them.
pub(crate) const NULL_KEY: &str = "x'6f7665726c6974652d6e756c6c'";

/// Makes a unique index NULLS NOT DISTINCT, which SQLite's are not, by indexing
/// `ifnull(col, <blob>)` for each key column, and marks it so at the end of its SQL,
/// `/*overlite nulls not distinct*/`, for the catalog, which reads each column back
/// out of its ifnull. NULLS DISTINCT, the default, goes.
fn index_nulls_not_distinct(sql: &str) -> String {
    let toks = code_tokens(sql);
    let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.depth == 0 && t.is_word(sql, w));
    let Some(n) = (0..toks.len())
        .find(|&i| is(i, "nulls") && (is(i + 1, "distinct") || (is(i + 1, "not") && is(i + 2, "distinct"))))
    else {
        return sql.to_string();
    };
    let not = is(n + 1, "not");
    let end = toks[n + if not { 2 } else { 1 }].span.end;
    let cut = [sql[..toks[n].span.start].trim_end(), &sql[end..]].concat();
    if !not {
        return cut;
    }
    // The key's items, in the parentheses after ON.
    let toks = code_tokens(&cut);
    let Some(on) = toks.iter().position(|t| t.depth == 0 && t.is_word(&cut, "on")) else {
        return sql.to_string();
    };
    let Some(open) = (on..toks.len()).find(|&i| toks[i].depth == 0 && toks[i].is_punct(&cut, "(")) else {
        return sql.to_string();
    };
    let mut out = String::with_capacity(cut.len() + 64);
    let mut at = 0;
    let mut i = open + 1;
    while i < toks.len() && toks[i].depth == 1 {
        let opens = toks[i - 1].is_punct(&cut, "(") || toks[i - 1].is_punct(&cut, ",");
        let name = toks[i].kind == CodeTokKind::Value && cut[toks[i].span.clone()].starts_with('"')
            || toks[i].kind == CodeTokKind::Word;
        let single = toks
            .get(i + 1)
            .is_none_or(|t| !t.is_punct(&cut, "(") && !t.is_punct(&cut, "."));
        if opens && name && single {
            out.push_str(&cut[at..toks[i].span.start]);
            out.push_str("ifnull(");
            out.push_str(&cut[toks[i].span.clone()]);
            out.push_str(", ");
            out.push_str(NULL_KEY);
            out.push(')');
            at = toks[i].span.end;
        }
        i += 1;
    }
    out.push_str(&cut[at..]);
    let body = out.trim_end();
    let (body, semi) = body.strip_suffix(';').map_or((body, ""), |b| (b.trim_end(), ";"));
    [body, " /*overlite nulls not distinct*/", semi].concat()
}

/// Takes a covering index's `INCLUDE (cols)` out of its CREATE INDEX, which SQLite
/// does not take, and keeps the columns in a comment at the end of the statement,
/// `/*overlite include "a", "b"*/`, where the catalog reads them back. SQLite's
/// index, unique or not, is over the key columns alone, as Postgres' key is.
fn move_index_include(sql: &str) -> String {
    let toks = code_tokens(sql);
    let Some(inc) = toks
        .iter()
        .position(|t| t.depth == 0 && t.is_word(sql, "include"))
        .filter(|&i| toks.get(i + 1).is_some_and(|t| t.is_punct(sql, "(")))
    else {
        return sql.to_string();
    };
    let Some(close) = (inc + 2..toks.len()).find(|&j| toks[j].depth == 0 && toks[j].is_punct(sql, ")")) else {
        return sql.to_string();
    };
    let cols = sql[toks[inc + 1].span.end..toks[close].span.start]
        .trim()
        .replace("*/", "");
    let rest = sql[toks[close].span.end..].trim_end();
    let (rest, semi) = rest.strip_suffix(';').map_or((rest, ""), |r| (r.trim_end(), ";"));
    [
        sql[..toks[inc].span.start].trim_end(),
        rest,
        " /*overlite include ",
        &cols,
        "*/",
        semi,
    ]
    .concat()
}

/// The operator classes Postgres gives a btree column of a type by default, which
/// pg_get_indexdef leaves out.
const DEFAULT_OPCLASSES: &[&str] = &[
    "bool_ops",
    "bpchar_ops",
    "bytea_ops",
    "char_ops",
    "date_ops",
    "float4_ops",
    "float8_ops",
    "int2_ops",
    "int4_ops",
    "int8_ops",
    "interval_ops",
    "jsonb_ops",
    "name_ops",
    "numeric_ops",
    "oid_ops",
    "text_ops",
    "time_ops",
    "timestamp_ops",
    "timestamptz_ops",
    "uuid_ops",
    "varchar_ops",
];

/// Takes the operator class out of each column of a CREATE INDEX, which SQLite's
/// one btree has no use for (`name varchar_pattern_ops`, Django's `_like` index).
/// One that is not the type's default stays in a comment, `name /*varchar_pattern_ops*/`,
/// which SQLite keeps in the index's SQL for the catalog to read back.
fn rewrite_index_opclass(sql: &str) -> String {
    if !sql.to_ascii_lowercase().contains("_ops") {
        return sql.to_string();
    }
    let toks = code_tokens(sql);
    let Some(on) = toks.iter().position(|t| t.depth == 0 && t.is_word(sql, "on")) else {
        return sql.to_string();
    };
    let Some(open) = (on..toks.len()).find(|&i| toks[i].depth == 0 && toks[i].is_punct(sql, "(")) else {
        return sql.to_string();
    };
    let mut out = String::with_capacity(sql.len() + 8);
    let mut at = 0;
    let mut i = open + 1;
    while i < toks.len() && toks[i].depth > 0 {
        let t = &toks[i];
        let word = &sql[t.span.clone()];
        // A qualified class, schema.class, goes whole.
        let first = if toks[i - 1].is_punct(sql, ".") && i >= open + 3 {
            i - 2
        } else {
            i
        };
        let opens_item = toks[first - 1].is_punct(sql, "(") || toks[first - 1].is_punct(sql, ",");
        if t.depth == 1 && t.kind == CodeTokKind::Word && word.to_ascii_lowercase().ends_with("_ops") && !opens_item {
            let start = toks[first].span.start;
            out.push_str(&sql[at..start]);
            let name = word.to_ascii_lowercase();
            if !DEFAULT_OPCLASSES.contains(&name.as_str()) {
                out.push_str("/*");
                out.push_str(&name);
                out.push_str("*/");
            }
            at = t.span.end;
        }
        i += 1;
    }
    if at == 0 {
        return sql.to_string();
    }
    out.push_str(&sql[at..]);
    out
}

// SERIAL/BIGSERIAL/SMALLSERIAL are Postgres auto-increment integers. In SQLite,
// an INTEGER PRIMARY KEY is the rowid and auto-increments, so we map the common
// "SERIAL PRIMARY KEY" to it (with AUTOINCREMENT to avoid rowid reuse, matching
// a sequence). A bare SERIAL becomes INTEGER.
//
// Only where a type goes: right after a column's name, which opens a column
// definition (after `(`, `,`, ADD, COLUMN or IF NOT EXISTS). A column may be named
// serial, quoted or not, and keeps its name.
pub(crate) fn rewrite_serial(sql: &str) -> String {
    // Only in DDL, so a column literally named "serial" in a query is safe.
    let h = first_word_upper(sql);
    if h != "CREATE" && h != "ALTER" {
        return sql.to_string();
    }
    if !sql.to_ascii_lowercase().contains("serial") {
        return sql.to_string();
    }
    let toks = code_tokens(sql);
    let mut out = String::with_capacity(sql.len() + 16);
    let mut at = 0;
    for i in 2..toks.len() {
        let t = &toks[i];
        let word = &sql[t.span.clone()];
        if t.kind != CodeTokKind::Word
            || !["serial", "bigserial", "smallserial", "serial4", "serial8", "serial2"]
                .iter()
                .any(|w| word.eq_ignore_ascii_case(w))
        {
            continue;
        }
        let name = &toks[i - 1];
        let before = &toks[i - 2];
        let is_name = name.kind == CodeTokKind::Value && sql.as_bytes()[name.span.start] == b'"'
            || name.kind == CodeTokKind::Word;
        let opens = before.is_punct(sql, "(")
            || before.is_punct(sql, ",")
            || ["add", "column", "exists"].iter().any(|w| before.is_word(sql, w));
        if !is_name || !opens {
            continue;
        }
        // SERIAL PRIMARY KEY is the rowid.
        let pk = toks.get(i + 1).is_some_and(|n| n.is_word(sql, "primary"))
            && toks.get(i + 2).is_some_and(|n| n.is_word(sql, "key"));
        out.push_str(&sql[at..t.span.start]);
        if pk {
            out.push_str("INTEGER PRIMARY KEY AUTOINCREMENT");
            at = toks[i + 2].span.end;
        } else {
            out.push_str("INTEGER");
            at = t.span.end;
        }
    }
    if at == 0 {
        return sql.to_string();
    }
    out.push_str(&sql[at..]);
    out
}

/// Writes the deferral of a UNIQUE or PRIMARY KEY constraint of a CREATE TABLE as a
/// comment SQLite keeps in the table's SQL, `/*overlite deferrable*/` or
/// `/*overlite deferrable initially deferred*/`, since SQLite takes DEFERRABLE on a
/// foreign key only. pg_constraint reads it back; NOT DEFERRABLE, the default, goes.
/// A foreign key's stays as written.
pub(crate) fn rewrite_deferrable_keys(sql: &str) -> String {
    if !eq_fold(&first_word_upper(sql), "CREATE") {
        return sql.to_string();
    }
    mark_key_deferral(sql)
}

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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn rewrite_index_opclass_cases() {
        assert_rewrites(
            rewrite_index_using,
            &[
                (
                    r#"CREATE INDEX "t_like" ON "t" ("name" varchar_pattern_ops)"#,
                    r#"CREATE INDEX "t_like" ON "t" ("name" /*varchar_pattern_ops*/)"#,
                ),
                (
                    "CREATE UNIQUE INDEX i ON t USING btree (a text_pattern_ops DESC, b int4_ops)",
                    "CREATE UNIQUE INDEX i ON t (a /*text_pattern_ops*/ DESC, b )",
                ),
                (
                    "CREATE INDEX i ON t (x.text_pattern_ops)",
                    "CREATE INDEX i ON t (x.text_pattern_ops)",
                ),
                ("CREATE INDEX i ON t (my_ops)", "CREATE INDEX i ON t (my_ops)"),
            ],
        );
    }

    #[test]
    fn rewrite_citext_cases() {
        assert_rewrites(
            rewrite_citext,
            &[
                (
                    "CREATE TABLE t (a citext NOT NULL, b citext[])",
                    "CREATE TABLE t (a citext COLLATE NOCASE NOT NULL, b citext[])",
                ),
                (
                    "ALTER TABLE t ALTER COLUMN a TYPE citext",
                    "ALTER TABLE t ALTER COLUMN a TYPE citext COLLATE NOCASE",
                ),
                ("CREATE EXTENSION citext", "CREATE EXTENSION citext"),
            ],
        );
    }

    #[test]
    fn rewrite_tablespace_cases() {
        assert_rewrites(
            rewrite_tablespace,
            &[
                (
                    r#"CREATE TABLE t (a int, CONSTRAINT u UNIQUE (a) USING INDEX TABLESPACE "pg_default") TABLESPACE pg_default"#,
                    "CREATE TABLE t (a int, CONSTRAINT u UNIQUE (a))",
                ),
                (
                    "CREATE INDEX i ON t (a) TABLESPACE pg_default WHERE a > 1",
                    "CREATE INDEX i ON t (a) WHERE a > 1",
                ),
                ("CREATE TABLESPACE x LOCATION '/x'", "CREATE TABLESPACE x LOCATION '/x'"),
            ],
        );
    }

    #[test]
    fn index_nulls_not_distinct_cases() {
        assert_rewrites(
            rewrite_index_using,
            &[
                (
                    r#"CREATE UNIQUE INDEX i ON t ("h", w DESC) NULLS NOT DISTINCT WHERE a > 1"#,
                    r#"CREATE UNIQUE INDEX i ON t (ifnull("h", x'6f7665726c6974652d6e756c6c'), ifnull(w, x'6f7665726c6974652d6e756c6c') DESC) WHERE a > 1 /*overlite nulls not distinct*/"#,
                ),
                (
                    "CREATE UNIQUE INDEX i ON t (h) NULLS DISTINCT",
                    "CREATE UNIQUE INDEX i ON t (h)",
                ),
            ],
        );
    }

    #[test]
    fn index_options_and_methods() {
        assert_rewrites(
            rewrite_index_using,
            &[
                (
                    r#"CREATE INDEX CONCURRENTLY "i" ON "t" USING gin ("tags") WITH (fastupdate = off)"#,
                    r#"CREATE INDEX "i" ON "t" ("tags") /*overlite using gin*/"#,
                ),
                (
                    "CREATE INDEX i ON t USING bloom (a, b) WITH (length = 80, col1 = 4) WHERE a > 1",
                    "CREATE INDEX i ON t (a, b) WHERE a > 1 /*overlite using bloom*/",
                ),
                ("CREATE INDEX i ON t USING btree (a)", "CREATE INDEX i ON t (a)"),
            ],
        );
    }

    #[test]
    fn index_include_moves_to_a_comment() {
        assert_rewrites(
            rewrite_index_using,
            &[
                (
                    r#"CREATE UNIQUE INDEX "i" ON "t" ("name") INCLUDE ("color", x)"#,
                    r#"CREATE UNIQUE INDEX "i" ON "t" ("name") /*overlite include "color", x*/"#,
                ),
                (
                    "CREATE INDEX i ON t USING btree (a varchar_pattern_ops) INCLUDE (b) WHERE a > 1;",
                    "CREATE INDEX i ON t (a /*varchar_pattern_ops*/) WHERE a > 1 /*overlite include b*/;",
                ),
            ],
        );
    }

    #[test]
    fn rewrite_deferrable_keys_cases() {
        assert_rewrites(
            rewrite_deferrable_keys,
            &[
                (
                    "CREATE TABLE t (a int, CONSTRAINT u UNIQUE (a) DEFERRABLE INITIALLY DEFERRED)",
                    "CREATE TABLE t (a int, CONSTRAINT u UNIQUE (a) /*overlite deferrable initially deferred*/)",
                ),
                (
                    "CREATE TABLE t (a int UNIQUE DEFERRABLE, b int PRIMARY KEY NOT DEFERRABLE)",
                    "CREATE TABLE t (a int UNIQUE /*overlite deferrable*/, b int PRIMARY KEY )",
                ),
                (
                    "CREATE TABLE t (a int REFERENCES p (id) DEFERRABLE INITIALLY DEFERRED, UNIQUE (a) INITIALLY IMMEDIATE DEFERRABLE)",
                    "CREATE TABLE t (a int REFERENCES p (id) DEFERRABLE INITIALLY DEFERRED, UNIQUE (a) /*overlite deferrable*/)",
                ),
            ],
        );
        assert_eq!(
            mark_key_deferral("CONSTRAINT u UNIQUE (a) DEFERRABLE INITIALLY IMMEDIATE"),
            "CONSTRAINT u UNIQUE (a) /*overlite deferrable*/"
        );
    }

    #[test]
    fn rewrite_serial_cases() {
        assert_rewrites(
            rewrite_serial,
            &[
                (
                    "CREATE TABLE t (id SERIAL PRIMARY KEY, n TEXT)",
                    "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, n TEXT)",
                ),
                (
                    "CREATE TABLE t (id BIGSERIAL PRIMARY KEY)",
                    "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)",
                ),
                ("CREATE TABLE t (a SERIAL, b INT)", "CREATE TABLE t (a INTEGER, b INT)"),
                (
                    "ALTER TABLE t ADD COLUMN c SMALLSERIAL",
                    "ALTER TABLE t ADD COLUMN c INTEGER",
                ),
                ("SELECT serial FROM t", "SELECT serial FROM t"),
                (
                    r#"CREATE TABLE t ("serial" integer, serial int, s serial)"#,
                    r#"CREATE TABLE t ("serial" integer, serial int, s INTEGER)"#,
                ),
                (
                    r#"ALTER TABLE t RENAME COLUMN "serial" TO "bigserial""#,
                    r#"ALTER TABLE t RENAME COLUMN "serial" TO "bigserial""#,
                ),
                (
                    r#"CREATE TABLE t ("id" serial  primary  key)"#,
                    r#"CREATE TABLE t ("id" INTEGER PRIMARY KEY AUTOINCREMENT)"#,
                ),
            ],
        );
    }
}

// Postgres names the table a trigger is on; SQLite takes the trigger's name alone.
lazy_regex!(
    RE_DROP_TRIGGER,
    r#"(?is)^(\s*DROP\s+TRIGGER\s+(?:IF\s+EXISTS\s+)?(?:"[^"]+"|[\w.]+))\s+ON\s+(?:"[^"]+"|[\w.]+)(?:\s+(?:CASCADE|RESTRICT))?\s*;?\s*$"#
);

/// Takes the `ON table` out of a DROP TRIGGER.
pub(crate) fn rewrite_drop_trigger(sql: &str) -> String {
    if !first_word_upper(sql).eq_ignore_ascii_case("DROP") {
        return sql.to_string();
    }
    replace_all(&RE_DROP_TRIGGER, sql, "$1")
}
