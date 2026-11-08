//! Postgres takes any keyword as a column label after AS — GORM's index listing
//! reads "i.indisprimary AS primary" — while SQLite refuses many of its own
//! keywords there ("near primary: syntax error"). Those labels are quoted, which
//! both read the same.
//!
//! Not every keyword: SELECT, VALUES and NOT follow AS in statements of their own
//! (CREATE VIEW v AS SELECT, CREATE TABLE t AS VALUES, a CTE AS NOT MATERIALIZED),
//! and a label is never followed by "(" — that is a CTE's body or a generated
//! column's expression.

use overlite_core::lazy_regex;
use overlite_core::text::{compile_or_panic, replace_all, replace_all_func};

pub(crate) const KEYWORD_LABELS: &str = "primary|index|unique|default|check|references|table|order|group|constraint|foreign|collate|transaction|set|join|using|limit|to|add|isnull|notnull|nothing|returning|escape|autoincrement|deferrable|between|case|else|exists|all|distinct|union|except|intersect|into|alter|drop|create|commit|from|where|when|then";

static RE_KEYWORD_LABEL: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| compile_or_panic(&format!(r"(?i)\bAS\s+({KEYWORD_LABELS})\b(\s*\()?")));

pub(crate) fn rewrite_keyword_labels(sql: &str) -> String {
    crate::rewrite::strings::map_outside_strings(sql, |code| {
        replace_all_func(&RE_KEYWORD_LABEL, code, |m| {
            let Some(sub) = RE_KEYWORD_LABEL.captures(m) else {
                return m.to_string();
            };
            if sub.get(2).is_some_and(|x| !x.as_str().is_empty()) {
                return m.to_string();
            }
            let s1 = sub.get(1).map_or("", |x| x.as_str());
            [&m[..m.len() - s1.len()], "\"", &s1.to_lowercase(), "\""].concat()
        })
    })
}

// DROP … CASCADE / RESTRICT: GORM drops a table with CASCADE, pg_dump scripts
// with either. SQLite has neither — dropping a table leaves views on it to fail
// when used, much as RESTRICT would have refused — so the clause goes.
lazy_regex!(
    RE_DROP_TAIL,
    r"(?is)^(\s*DROP\s+(?:TABLE|VIEW|INDEX|TRIGGER)\b.*?)\s+(?:CASCADE|RESTRICT)\s*;?\s*$"
);

pub(crate) fn rewrite_drop_cascade(sql: &str) -> String {
    let sql = replace_all(&RE_DROP_CONCURRENTLY, sql, "$1");
    split_drop_list(&replace_all(&RE_DROP_TAIL, &sql, "$1"))
}

// DROP INDEX CONCURRENTLY: SQLite drops an index under the write lock anyway.
lazy_regex!(RE_DROP_CONCURRENTLY, r"(?is)^(\s*DROP\s+INDEX\s+)CONCURRENTLY\s+");

lazy_regex!(
    RE_DROP_HEAD,
    r"(?is)^\s*DROP\s+(?:TABLE|VIEW|INDEX)\s+(?:IF\s+EXISTS\s+)?"
);

/// `DROP TABLE a, b` as one DROP for each, which is all SQLite's DROP takes; the
/// engine runs the statements of a text one after another.
fn split_drop_list(sql: &str) -> String {
    let Some(head) = RE_DROP_HEAD.find(sql) else {
        return sql.to_string();
    };
    let names = crate::ddl::sequences::split_top_level(sql[head.end()..].trim_end().trim_end_matches(';'));
    if names.len() < 2 {
        return sql.to_string();
    }
    names
        .iter()
        .map(|n| [head.as_str().trim(), " ", n.trim()].concat())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn drop_lists() {
        assert_rewrites(
            rewrite_drop_cascade,
            &[
                ("DROP TABLE a, \"b c\" CASCADE", "DROP TABLE a; DROP TABLE \"b c\""),
                ("DROP TABLE IF EXISTS a", "DROP TABLE IF EXISTS a"),
                (
                    "drop view if exists v1, v2",
                    "drop view if exists v1; drop view if exists v2",
                ),
            ],
        );
    }

    #[test]
    fn rewrite_keyword_labels_cases() {
        assert_rewrites(
            rewrite_keyword_labels,
            &[
                (
                    "SELECT i.indisprimary AS primary, i.indisunique AS non_unique FROM pg_index i",
                    r#"SELECT i.indisprimary AS "primary", i.indisunique AS non_unique FROM pg_index i"#,
                ),
                ("SELECT x AS Order FROM t", r#"SELECT x AS "order" FROM t"#),
                ("CREATE VIEW v AS SELECT 1", "CREATE VIEW v AS SELECT 1"),
                (
                    "WITH c AS (SELECT 1) SELECT * FROM c",
                    "WITH c AS (SELECT 1) SELECT * FROM c",
                ),
                ("SELECT CAST(x AS int) FROM t", "SELECT CAST(x AS int) FROM t"),
                (
                    "CREATE TABLE t (a int GENERATED ALWAYS AS (1))",
                    "CREATE TABLE t (a int GENERATED ALWAYS AS (1))",
                ),
            ],
        );
    }

    #[test]
    fn rewrite_drop_cascade_cases() {
        assert_rewrites(
            rewrite_drop_cascade,
            &[
                (
                    r#"DROP TABLE IF EXISTS "things" CASCADE"#,
                    r#"DROP TABLE IF EXISTS "things""#,
                ),
                ("DROP VIEW v RESTRICT;", "DROP VIEW v"),
                ("DROP TABLE t", "DROP TABLE t"),
                ("DROP SCHEMA s CASCADE", "DROP SCHEMA s CASCADE"),
            ],
        );
    }
}
