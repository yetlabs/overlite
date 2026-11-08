//! The catalogs keep their boolean columns as SQLite integers (0/1), which is
//! what a bare `WHERE indisunique` or `NOT relispartition` wants. Postgres also
//! lets a boolean meet a string literal ('t', 'f', 'true', ...); SQLite compares
//! 0 with 'f' as two different values and finds nothing. Prisma lists tables with
//! `relkind = 'r' AND relispartition = 'f'`, which matched no table at all.
//!
//! Only the catalog's boolean columns are rewritten, by name: a user table's text
//! column compared with 't' stays a text comparison.

use crate::types::BOOL_CATALOG_COLUMNS;
use overlite_core::text::{compile_or_panic, replace_all_func};
use regex::Regex;
use std::sync::LazyLock;

/// Matches `[qualifier.]<catalog bool column> <op> '<literal>'` for the spellings
/// Postgres accepts as a boolean literal.
static RE_CATALOG_BOOL_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    let mut names: Vec<String> = BOOL_CATALOG_COLUMNS.iter().map(|name| regex::escape(name)).collect();
    names.sort();
    compile_or_panic(
        &[
            r"(?i)\b((?:\w+\.)?(?:",
            &names.join("|"),
            r"))(\s*(?:=|<>|!=)\s*)'(t|true|y|yes|on|1|f|false|n|no|off|0)'",
        ]
        .concat(),
    )
});

/// Turns the literal compared with a catalog boolean column into the 1/0 the
/// column holds.
pub(crate) fn rewrite_catalog_bool_literals(sql: &str) -> String {
    if !sql.contains('\'') {
        return sql.to_string();
    }
    replace_all_func(&RE_CATALOG_BOOL_LITERAL, sql, |m| {
        let Some(sub) = RE_CATALOG_BOOL_LITERAL.captures(m) else {
            return m.to_string();
        };
        let v = match sub.get(3).map_or("", |x| x.as_str()).to_lowercase().as_str() {
            "t" | "true" | "y" | "yes" | "on" | "1" => "1",
            _ => "0",
        };
        [
            sub.get(1).map_or("", |x| x.as_str()),
            sub.get(2).map_or("", |x| x.as_str()),
            v,
        ]
        .concat()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn rewrite_catalog_bool_literals_cases() {
        assert_rewrites(
            rewrite_catalog_bool_literals,
            &[
                (
                    "SELECT 1 FROM pg_class tbl WHERE tbl.relkind = 'r' AND tbl.relispartition = 'f'",
                    "SELECT 1 FROM pg_class tbl WHERE tbl.relkind = 'r' AND tbl.relispartition = 0",
                ),
                (
                    "SELECT 1 FROM pg_index WHERE indisunique = 'true' AND indisprimary <> 'f'",
                    "SELECT 1 FROM pg_index WHERE indisunique = 1 AND indisprimary <> 0",
                ),
                (
                    "SELECT 1 FROM pg_attribute WHERE attnotnull='t'",
                    "SELECT 1 FROM pg_attribute WHERE attnotnull=1",
                ),
                (
                    "SELECT 1 FROM users WHERE active = 'f'",
                    "SELECT 1 FROM users WHERE active = 'f'",
                ),
                (
                    "SELECT 1 FROM pg_class WHERE relkind = 'r'",
                    "SELECT 1 FROM pg_class WHERE relkind = 'r'",
                ),
                (
                    "SELECT 1 FROM pg_class WHERE relispartition = 0",
                    "SELECT 1 FROM pg_class WHERE relispartition = 0",
                ),
                (
                    "SELECT 1 FROM t WHERE relispartition_note = 'f'",
                    "SELECT 1 FROM t WHERE relispartition_note = 'f'",
                ),
            ],
        );
    }
}
