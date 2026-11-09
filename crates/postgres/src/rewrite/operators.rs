//! OPERATOR(...), COLLATE, the POSIX regex operators and pg_trgm's.

use crate::rewrite::strings::map_outside_strings;
use overlite_core::lazy_regex;
use overlite_core::text::{replace_all_func, trim_chars};

// Matches Postgres' explicit operator syntax, e.g.
// "OPERATOR(pg_catalog.~)", which psql emits for pattern matching.
lazy_regex!(RE_OPERATOR_CALL, r"(?i)OPERATOR\s*\(\s*([^)]+?)\s*\)");

// Matches a COLLATE clause with an optionally schema-qualified,
// optionally double-quoted collation name.
lazy_regex!(RE_COLLATE, r#"(?i)\s+COLLATE\s+("[^"]*"|\w+)(?:\.("[^"]*"|\w+))?"#);

/// Maps Postgres COLLATE onto SQLite. A language collation (`sv-x-icu`,
/// `sv_SE.utf8`) or one CREATE COLLATION made stays, under its name, and the engine
/// provides it; any other case-insensitive name becomes NOCASE; C, POSIX, default and
/// unknown names fall back to the byte-order default by dropping the clause.
pub(crate) fn rewrite_collate(sql: &str) -> String {
    map_outside_strings(sql, |code| {
        replace_all_func(&RE_COLLATE, code, |m| {
            let Some(sub) = RE_COLLATE.captures(m) else {
                return m.to_string();
            };
            let mut name = sub.get(1).map_or("", |x| x.as_str());
            if let Some(s2) = sub.get(2)
                && !s2.as_str().is_empty()
            {
                name = s2.as_str(); // schema-qualified: the last part is the collation
            }
            let name = trim_chars(name, "\"");
            match name.to_lowercase().as_str() {
                "decimal" | "binary" | "nocase" | "rtrim" => return m.to_string(), // keep overlite/SQLite native collations verbatim
                _ => {}
            }
            // A language collation, or one CREATE COLLATION made: the engine provides
            // it under its own name, which the catalogs then report.
            if overlite_engine::collation_known(name) {
                return [" COLLATE \"", &name.replace('"', "\"\""), "\""].concat();
            }
            if collation_is_ci(name) {
                return " COLLATE NOCASE".to_string();
            }
            String::new()
        })
    })
}

/// Reports whether a Postgres collation name denotes a case-insensitive
/// collation (by convention or ICU comparison level).
pub(crate) fn collation_is_ci(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("nocase")
        || n.contains("insensitive")
        || n.contains("ks-level1")
        || n.contains("ks-level2")
        || n.ends_with("_ci")
        || n.ends_with("-ci")
}
