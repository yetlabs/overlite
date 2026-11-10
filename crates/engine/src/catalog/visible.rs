//! What the catalog's definitions name a relation by: bare where the session's
//! search_path finds it, qualified where it does not, as Postgres' ruleutils writes
//! them. pg_dump sets the path to '' so that every name in its output is qualified.
//!
//! A view's query is read by Postgres' grammar, which the engine does not link: the
//! protocol crate gives the reader (`set_view_reader`).

use std::cell::RefCell;
use std::sync::OnceLock;

std::thread_local! {
    /// The search_path of the session this thread serves; None is the default,
    /// `"$user", public`.
    static PATH: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    /// A change of search_path a statement made through set_config, for the session.
    static CHANGED: RefCell<Option<SearchPathChange>> = const { RefCell::new(None) };
}

/// A search_path a statement set: None is the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPathChange(pub Option<Vec<String>>);

/// Makes `path` the search_path the catalog's definitions are written under.
pub fn set_display_search_path(path: Option<&[String]>) {
    PATH.with(|p| {
        let mut p = p.borrow_mut();
        if p.as_deref() != path {
            *p = path.map(<[String]>::to_vec);
        }
    });
}

/// set_config('search_path', value, false), from SQL: kept for the session.
pub(crate) fn set_search_path_from_sql(value: &str) {
    let path = parse_search_path(value);
    set_display_search_path(path.as_deref());
    CHANGED.with(|c| *c.borrow_mut() = Some(SearchPathChange(path)));
}

/// The search_path the statements since the last call set through set_config, if any.
#[must_use]
pub fn take_search_path_change() -> Option<SearchPathChange> {
    CHANGED.with(|c| c.borrow_mut().take())
}

/// A search_path's schemas, unquoted; None for DEFAULT. An empty value is an empty
/// path, where no schema but pg_catalog is visible.
#[must_use]
pub fn parse_search_path(value: &str) -> Option<Vec<String>> {
    let value = value.trim().trim_end_matches(';').trim();
    if value.eq_ignore_ascii_case("default") {
        return None;
    }
    Some(
        value
            .split(',')
            .map(|p| {
                let p = p.trim();
                match p.strip_prefix('"').and_then(|q| q.strip_suffix('"')) {
                    Some(q) => q.replace("\"\"", "\""),
                    None => p.trim_matches('\'').to_lowercase(),
                }
            })
            .filter(|p| !p.is_empty())
            .collect(),
    )
}

/// Whether the session's search_path makes the schema's relations visible.
pub(crate) fn schema_visible(schema: &str) -> bool {
    if schema.eq_ignore_ascii_case("pg_catalog") {
        return true;
    }
    PATH.with(|p| match p.borrow().as_deref() {
        None => schema.eq_ignore_ascii_case("public"),
        Some(path) => path.iter().any(|s| s.eq_ignore_ascii_case(schema)),
    })
}

/// The schema and name of a relation SQLite keeps as `table` in a file whose schema
/// is `schema`: a single-file schema's table is stored as `s.t`.
pub(crate) fn stored_parts<'a>(table: &'a str, schema: &'a str) -> (&'a str, &'a str) {
    table.split_once('.').unwrap_or((schema, table))
}

/// A relation's name as a definition writes it: bare where the path finds its schema.
pub(crate) fn relation_ref(table: &str, schema: &str) -> String {
    use crate::catalog::quote_ident_if_needed;
    let (schema, name) = stored_parts(table, schema);
    if schema_visible(schema) {
        quote_ident_if_needed(name)
    } else {
        [
            quote_ident_if_needed(schema),
            ".".to_string(),
            quote_ident_if_needed(name),
        ]
        .concat()
    }
}

/// A default's `nextval('seq'::regclass)` with the sequence named as regclass writes
/// it: qualified where the path does not find it.
pub(crate) fn qualify_nextval(expr: &str) -> String {
    const HEAD: &str = "nextval('";
    const TAIL: &str = "'::regclass)";
    let Some(at) = expr.find(HEAD) else {
        return expr.to_string();
    };
    let start = at + HEAD.len();
    let Some(len) = expr[start..].find(TAIL) else {
        return expr.to_string();
    };
    let name = &expr[start..start + len];
    let stored = name.trim_matches('"');
    let named = relation_ref(stored, "public").replace('\'', "''");
    [&expr[..start], named.as_str(), &expr[start + len..]].concat()
}

/// Writes a view's query with each relation it reads named by `relation`, given the
/// relation's stored name.
pub type QualifyQuery = fn(query: &str, relation: &dyn Fn(&str) -> String) -> Option<String>;

/// What reads a view's query by Postgres' grammar.
#[derive(Debug, Clone, Copy)]
pub struct ViewReader {
    /// The query with each relation it reads written as `relation` writes it, given
    /// the stored name of the relation and the view's schema; None where the grammar
    /// does not read it.
    pub qualify: QualifyQuery,
    /// The stored names of the relations the query reads (CTEs are not relations).
    pub relations: fn(query: &str) -> Vec<String>,
}

static VIEW_READER: OnceLock<ViewReader> = OnceLock::new();

/// Gives the engine the reader of a view's query. The first one given stays.
pub fn set_view_reader(reader: ViewReader) {
    let _ = VIEW_READER.set(reader);
}

/// The view's query with its relations named as the session's path finds them.
pub(crate) fn qualified_view_query(query: &str, schema: &str) -> String {
    let Some(reader) = VIEW_READER.get() else {
        return query.to_string();
    };
    (reader.qualify)(query, &|table| relation_ref(table, schema)).unwrap_or_else(|| query.to_string())
}

/// The relations a view's query reads, as `schema.name`.
pub(crate) fn view_relations(query: &str, schema: &str) -> Vec<String> {
    let Some(reader) = VIEW_READER.get() else {
        return Vec::new();
    };
    (reader.relations)(query)
        .iter()
        .map(|t| {
            let (s, n) = stored_parts(t, schema);
            [s, ".", n].concat()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_path() {
        assert_eq!(parse_search_path("DEFAULT"), None);
        assert_eq!(parse_search_path(""), Some(vec![]));
        assert_eq!(
            parse_search_path("\"$user\", Public, \"My S\""),
            Some(vec!["$user".to_string(), "public".to_string(), "My S".to_string()])
        );
    }

    #[test]
    fn names_by_the_path() {
        set_display_search_path(None);
        assert_eq!(relation_ref("t", "public"), "t");
        assert_eq!(relation_ref("s.t", "public"), "s.t");
        set_display_search_path(Some(&[]));
        assert_eq!(relation_ref("t", "public"), "public.t");
        set_display_search_path(Some(&["s".to_string()]));
        assert_eq!(relation_ref("s.t", "public"), "t");
        set_display_search_path(Some(&[]));
        assert_eq!(
            qualify_nextval("nextval('t_id_seq'::regclass)"),
            "nextval('public.t_id_seq'::regclass)"
        );
        set_display_search_path(None);
        assert_eq!(
            qualify_nextval("nextval('t_id_seq'::regclass)"),
            "nextval('t_id_seq'::regclass)"
        );
    }
}
