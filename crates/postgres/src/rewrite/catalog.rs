//! Catalog references: pg_catalog, information_schema, object comments and definitions.

use crate::ddl::sequences::{read_paren_args, split_top_level};
use crate::rewrite::strings::map_outside_strings;
use crate::rewrite::{index_from, splice};
use overlite_core::lazy_regex;
use overlite_core::text::{replace_all, replace_all_func};

/// Turns obj_description(oid,'cat') / col_description(oid, attnum) /
/// shobj_description(oid,'cat') into a lookup against the pg_description view
/// (populated from _overlite_comments). These are what psql's \d+ reads.
pub(crate) fn rewrite_obj_description(sql: &str) -> String {
    let sql = rewrite_desc_call(sql, "obj_description", false);
    let sql = rewrite_desc_call(&sql, "shobj_description", false);
    rewrite_desc_call(&sql, "col_description", true)
}

fn rewrite_desc_call(sql: &str, name: &str, is_col: bool) -> String {
    let mut sql = sql.to_string();
    let mut from = 0usize;
    loop {
        let low = sql.to_ascii_lowercase();
        let Some(i) = index_word_call(&low, name, from) else {
            return sql;
        };
        let b = sql.as_bytes();
        let mut open = i + name.len();
        while open < b.len() && b[open] == b' ' {
            open += 1;
        }
        let Some((args, end)) = read_paren_args(&sql, open) else {
            return sql;
        };
        let parts = split_top_level(&args);
        if parts.len() < 2 {
            from = end;
            continue;
        }
        let mut objsub = "0";
        if is_col {
            objsub = parts[1].trim();
        }
        let repl = [
            "(SELECT description FROM pg_description WHERE objoid = ",
            parts[0].trim(),
            " AND objsubid = ",
            objsub,
            " LIMIT 1)",
        ]
        .concat();
        sql = splice(&sql, i, end, &repl);
        from = i + repl.len();
    }
}

// Matches references to the emulated information_schema so they
// resolve to the quoted, dotted view names the engine materializes. Either name may
// be quoted, as ORMs write them (`"information_schema"."sequences"`); a quoted name
// keeps its case, as in Postgres.
lazy_regex!(
    RE_INFO_SCHEMA,
    concat!(
        r#"(?:(?i:\binformation_schema)|"information_schema")\."#,
        r#"(?:(?i:(?:tables|columns|table_constraints|key_column_usage|referential_constraints|"#,
        r#"constraint_column_usage|check_constraints|views|schemata|sequences|routines)\b)"#,
        r#"|"(?:tables|columns|table_constraints|key_column_usage|referential_constraints|"#,
        r#"constraint_column_usage|check_constraints|views|schemata|sequences|routines)")"#
    )
);

pub(crate) fn rewrite_information_schema(sql: &str) -> String {
    map_outside_strings(sql, |code| {
        replace_all_func(&RE_INFO_SCHEMA, code, |m| {
            ["\"", &m.to_lowercase().replace('"', ""), "\""].concat()
        })
    })
}

/// Replaces every call name(...) — with balanced parentheses — by repl. Matching
/// is case-insensitive and respects word boundaries.
pub(crate) fn replace_call(sql: &str, name: &str, repl: &str) -> String {
    let mut sql = sql.to_string();
    let mut low = sql.to_ascii_lowercase();
    let lname = name.to_lowercase();
    let mut from = 0usize;
    loop {
        let Some(i) = index_word_call(&low, &lname, from) else {
            return sql;
        };
        let b = sql.as_bytes();
        let mut open = i + name.len();
        while open < b.len() && b[open] == b' ' {
            open += 1;
        }
        let mut depth = 0;
        let mut j = open;
        while j < b.len() {
            if b[j] == b'(' {
                depth += 1;
            } else if b[j] == b')' {
                depth -= 1;
                if depth == 0 {
                    j += 1;
                    break;
                }
            }
            j += 1;
        }
        if depth != 0 {
            return sql; // unbalanced; bail
        }
        sql = splice(&sql, i, j, repl);
        low = sql.to_ascii_lowercase();
        from = i + repl.len();
    }
}

/// Turns pg_get_indexdef()/pg_get_constraintdef() calls into correlated
/// subqueries that reconstruct the definition from our catalog views
/// (ov_cols/ov_ref), so psql renders real "btree (...)" and "FOREIGN KEY ..."
/// lines instead of blanks. psql looks for " USING " in an index def, so the
/// index form includes it.
pub(crate) fn rewrite_object_defs(sql: &str) -> String {
    let sql = rewrite_func_call_arg1(sql, "pg_get_indexdef", |a| {
        // Full CREATE INDEX so pg_dump emits a restorable statement; psql still
        // finds " USING " to render its \d index line.
        // The index comes through a derived table whose columns have names of their
        // own, so a bare argument (pg_get_indexdef(indexrelid)) still means the
        // caller's column.
        [
            "(SELECT 'CREATE ' || CASE WHEN ix._u THEN 'UNIQUE ' ELSE '' END || 'INDEX '",
            " || (SELECT relname FROM pg_class WHERE oid = ix._i)",
            " || ' ON ' || (SELECT n.nspname||'.'||c.relname",
            "               FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.oid = ix._r)",
            " || ' USING btree (' || ix._c || ')'",
            " FROM (SELECT indexrelid AS _i, indrelid AS _r, indisunique AS _u, ov_cols AS _c FROM pg_index) ix",
            " WHERE ix._i = ",
            a,
            ")",
        ]
        .concat()
    });
    rewrite_func_call_arg1(&sql, "pg_get_constraintdef", |a| {
        [
            "(SELECT CASE _t",
            " WHEN 'f' THEN 'FOREIGN KEY (' || _c || ') REFERENCES ' || _r",
            " WHEN 'p' THEN 'PRIMARY KEY (' || _c || ')'",
            " WHEN 'u' THEN 'UNIQUE (' || _c || ')'",
            " WHEN 'c' THEN 'CHECK (' || _c || ')'",
            " WHEN 'x' THEN _c",
            " ELSE '' END FROM (SELECT oid AS _o, contype AS _t, ov_cols AS _c, ov_ref AS _r FROM pg_constraint)",
            " WHERE _o = ",
            a,
            ")",
        ]
        .concat()
    })
}

/// Replaces each call name(arg1, ...) with build(arg1), matching balanced
/// parentheses.
fn rewrite_func_call_arg1(sql: &str, name: &str, build: impl Fn(&str) -> String) -> String {
    let mut sql = sql.to_string();
    let mut low = sql.to_ascii_lowercase();
    let lname = name.to_lowercase();
    let mut from = 0usize;
    loop {
        let Some(i) = index_word_call(&low, &lname, from) else {
            return sql;
        };
        let b = sql.as_bytes();
        let mut open = i + name.len();
        while open < b.len() && b[open] == b' ' {
            open += 1;
        }
        let mut depth = 0;
        let mut j = open;
        let mut arg1_end: Option<usize> = None;
        while j < b.len() {
            match b[j] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b',' if depth == 1 && arg1_end.is_none() => {
                    arg1_end = Some(j);
                }
                _ => {}
            }
            if depth == 0 {
                break;
            }
            j += 1;
        }
        if depth != 0 {
            return sql;
        }
        let arg1_end = arg1_end.unwrap_or(j);
        let repl = build(sql[open + 1..arg1_end].trim());
        sql = splice(&sql, i, j + 1, &repl);
        low = sql.to_ascii_lowercase();
        from = i + repl.len();
    }
}

pub(crate) fn index_word_call(low: &str, name: &str, from: usize) -> Option<usize> {
    let l = low.as_bytes();
    let mut from = from;
    while from < l.len() {
        let i = index_from(low, name, from)?;
        let boundary_before = i == 0 || !is_word_byte(l[i - 1]);
        let mut k = i + name.len();
        while k < l.len() && l[k] == b' ' {
            k += 1;
        }
        if boundary_before && k < l.len() && l[k] == b'(' {
            return Some(i);
        }
        from = i + name.len();
    }
    None
}

pub(crate) fn is_word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_lowercase() || b.is_ascii_uppercase() || b.is_ascii_digit()
}

// Strips the "pg_catalog." schema qualifier: our emulated catalog
// tables and functions live under bare names, and clients reference them both
// qualified and unqualified.
lazy_regex!(RE_PG_CATALOG, r#"(?:(?i:\bpg_catalog)|"pg_catalog")\."#);

pub(crate) fn rewrite_pg_catalog_prefix(sql: &str) -> String {
    map_outside_strings(sql, |code| replace_all(&RE_PG_CATALOG, code, ""))
}

// Matches a "public." (or "public".) schema qualifier on a single name.
// Stripping it across a whole statement is the engine's job, after search_path
// resolution; this is for the places that handle one table name or clause.
lazy_regex!(pub(crate) RE_PUBLIC, r#"(?i)"?\bpublic\b"?\."#);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::{assert_rewrites, rewrite};

    #[test]
    fn rewrite_information_schema_cases() {
        assert_rewrites(
            rewrite_information_schema,
            &[
                (
                    "SELECT * FROM information_schema.tables",
                    r#"SELECT * FROM "information_schema.tables""#,
                ),
                (
                    "SELECT * FROM information_schema.columns WHERE table_name = 'x'",
                    r#"SELECT * FROM "information_schema.columns" WHERE table_name = 'x'"#,
                ),
                (
                    "SELECT * FROM INFORMATION_SCHEMA.TABLES",
                    r#"SELECT * FROM "information_schema.tables""#,
                ),
                (
                    r#"SELECT "sequence_name" FROM "information_schema"."sequences""#,
                    r#"SELECT "sequence_name" FROM "information_schema.sequences""#,
                ),
                (
                    r#"SELECT * FROM information_schema."tables", "information_schema".columns"#,
                    r#"SELECT * FROM "information_schema.tables", "information_schema.columns""#,
                ),
                (
                    r#"SELECT * FROM "INFORMATION_SCHEMA"."tables""#,
                    r#"SELECT * FROM "INFORMATION_SCHEMA"."tables""#,
                ),
            ],
        );
    }

    #[test]
    fn rewrite_pg_catalog_prefix_cases() {
        assert_rewrites(
            rewrite_pg_catalog_prefix,
            &[
                ("SELECT * FROM pg_catalog.pg_class", "SELECT * FROM pg_class"),
                ("SELECT pg_catalog.version()", "SELECT version()"),
                (
                    "JOIN pg_catalog.pg_namespace n ON n.oid",
                    "JOIN pg_namespace n ON n.oid",
                ),
                ("SELECT 1", "SELECT 1"),
                ("SELECT PG_CATALOG.PG_CLASS", "SELECT PG_CLASS"),
                (
                    r#"SELECT * FROM "pg_catalog"."pg_class""#,
                    r#"SELECT * FROM "pg_class""#,
                ),
                (r#"SELECT "pg_catalog".version()"#, "SELECT version()"),
            ],
        );
    }

    #[test]
    fn rewrite_object_defs_cases() {
        assert!(rewrite("pg_catalog.pg_get_indexdef(i.indexrelid, 0, true)").contains("USING btree"));
        assert!(rewrite("pg_catalog.pg_get_constraintdef(con.oid, true)").contains("FOREIGN KEY"));
    }
}
