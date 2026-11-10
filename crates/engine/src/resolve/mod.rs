//! Per-statement name resolution, run just before a statement reaches SQLite.

pub(crate) mod current_database;
pub(crate) mod current_schema;
pub(crate) mod public_qualifier;
pub(crate) mod regclass;
pub(crate) mod schema_qualify;
pub(crate) mod search_path;

/// What the translation appends to a statement it typed from its environment
/// (crates/postgres/src/ast/types.rs): what follows the word, if anything, is the type
/// of each result column the environment knows (`typed int4,,int8`), which nothing
/// else tells of a computed column.
pub const TYPED_MARK: &str = "/*overlite typed";

/// A statement without the translation's mark, and the result types the mark carries:
/// None for a statement without one (the fast path's, the session's own).
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
            None if *t == "uuid" => c.decl_type = "UUID".to_string(),
            None if *t == "timestamptz" => c.decl_type = "TIMESTAMPTZ".to_string(),
            None => {}
        }
    }
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
