//! Per-statement name resolution, run just before a statement reaches SQLite.

pub(crate) mod booleans;
pub(crate) mod current_database;
pub(crate) mod current_schema;
pub(crate) mod names;
pub(crate) mod public_qualifier;
pub(crate) mod regclass;
pub(crate) mod schema_qualify;
pub(crate) mod search_path;
pub(crate) mod temporal;
pub(crate) mod time_zone;
pub(crate) mod tsvector;

use crate::db_state::DbState;
use overlite_core::Value;
use rusqlite::Connection;
use std::borrow::Cow;

/// What the AST path appends to a statement it translated from its type environment
/// (crates/postgres/src/ast/types.rs): its output already says everything the type
/// passes below would, so they do not run on it. What follows the word, if anything,
/// is the type of each result column the environment knows (`typed int4,,int8`).
pub const TYPED_MARK: &str = "/*overlite typed";

/// A statement without the AST path's mark, and the result types the mark carries:
/// None for a statement without one, which the type passes read.
pub(crate) fn split_typed(query: &str) -> (&str, Option<Vec<&str>>) {
    let t = query.trim_end();
    if !t.ends_with("*/") {
        return (query, None);
    }
    let Some(at) = t.rfind(TYPED_MARK) else {
        return (query, None);
    };
    let tail = t[at + TYPED_MARK.len()..t.len() - 2].trim();
    let types = if tail.is_empty() {
        Vec::new()
    } else {
        tail.split(',').map(str::trim).collect()
    };
    (t[..at].trim_end(), Some(types))
}

/// The passes that read a statement's types from its text and the probes of its
/// columns: AT TIME ZONE's direction, a value with an offset, tsvector, date, time and
/// numeric arithmetic and comparison, booleans. The AST path does their work itself.
pub(crate) fn resolve_types<'a>(st: &DbState, q: &Connection, query: &'a str, args: &[Value]) -> Cow<'a, str> {
    let a = time_zone::resolve_time_zones(q, query);
    let b = time_zone::resolve_zone_values(q, &a, args);
    let c = tsvector::resolve_tsvectors(q, &b);
    let d = temporal::resolve_temporal(q, &c);
    let e = booleans::resolve_booleans(st, q, &d);
    if matches!(
        (&a, &b, &c, &d, &e),
        (
            Cow::Borrowed(_),
            Cow::Borrowed(_),
            Cow::Borrowed(_),
            Cow::Borrowed(_),
            Cow::Borrowed(_)
        )
    ) {
        Cow::Borrowed(query)
    } else {
        Cow::Owned(e.into_owned())
    }
}

/// The result types a mark carries, as the declared types the protocol reads
/// (`pg_int4`, `pg_int4[]`, `text[]`): an integer's width and an array's element type,
/// which nothing else tells of a computed column.
pub(crate) fn apply_result_types(columns: &mut [overlite_core::Column], types: Option<&[&str]>) {
    let Some(types) = types else { return };
    if types.len() != columns.len() {
        return;
    }
    for (c, t) in columns.iter_mut().zip(types) {
        match t.strip_suffix("[]") {
            Some(w @ ("int2" | "int4" | "int8")) => c.decl_type = ["PG_", &w.to_ascii_uppercase(), "[]"].concat(),
            Some(_) => c.decl_type = (*t).to_string(),
            None if matches!(*t, "int2" | "int4" | "int8") => {
                c.decl_type = ["PG_", &t.to_ascii_uppercase()].concat();
            }
            None if *t == "bool" => c.decl_type = "BOOL".to_string(),
            None => {}
        }
    }
}

/// The type passes over sql on a database with no tables, as they would finish a
/// statement the text passes translated: what the AST path's differential test
/// (crates/postgres/src/ast/diff.rs) compares its typed translation with, under a
/// catalog as empty. Not for the server's use.
#[doc(hidden)]
#[must_use]
pub fn type_passes_without_tables(sql: &str) -> String {
    thread_local! {
        static EMPTY: (Option<Connection>, std::sync::Arc<DbState>) = (
            Connection::open_in_memory().ok(),
            crate::db_state::state_for(":memory:type-passes"),
        );
    }
    EMPTY.with(|(q, st)| match q {
        Some(q) => resolve_types(st, q, sql, &[]).into_owned(),
        None => sql.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_is_split_off() {
        assert_eq!(split_typed("SELECT 1"), ("SELECT 1", None));
        assert_eq!(split_typed("SELECT 1 /*overlite typed*/"), ("SELECT 1", Some(vec![])));
        assert_eq!(
            split_typed("SELECT a, b /*overlite typed int4,*/ "),
            ("SELECT a, b", Some(vec!["int4", ""]))
        );
        // A literal holding the words ends in a quote, never in the mark.
        assert_eq!(
            split_typed("SELECT '/*overlite typed*/'"),
            ("SELECT '/*overlite typed*/'", None)
        );
    }
}
