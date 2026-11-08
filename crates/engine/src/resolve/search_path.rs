//! After explicit `x.t` qualifiers are handled (see schema_qualify.rs), an
//! unqualified table name in a table position is resolved against the session
//! search_path: for a read it becomes the first path schema that actually has
//! that table; for CREATE TABLE it goes to the first path schema.

use crate::db_state::DbState;
use crate::resolve::schema_qualify::{builder_string, is_ident_byte, lowercase, skip_literal};
use crate::sqlite::do_query;
use overlite_core::Value;
use rusqlite::Connection;
use std::borrow::Cow;
use std::collections::HashMap;

/// The registered, non-public schemas named in the session path, in path order.
pub(crate) fn usable_search_path<'a>(st: &DbState, sp: Option<&'a [String]>) -> Vec<&'a str> {
    let sp = sp.unwrap_or(&[]);
    if sp.is_empty() {
        return Vec::new();
    }
    let mut reg: HashMap<String, bool> = HashMap::new();
    for s in st.schema_list().iter() {
        reg.insert(s.to_lowercase(), true);
    }
    let mut out = Vec::new();
    for s in sp {
        let ls = lowercase(s);
        if ls == "public" || ls == "$user" || !reg.get(ls.as_ref()).copied().unwrap_or(false) {
            continue;
        }
        out.push(s.as_str());
    }
    out
}

/// Rewrites unqualified table references per the session search_path. A no-op
/// with an empty or public-only path.
pub(crate) fn resolve_search_path(sp: Option<&[String]>, st: &DbState, q: &Connection, query: &str) -> String {
    let path = usable_search_path(st, sp);
    if path.is_empty() {
        return query.to_string();
    }
    rewrite_unqualified_tables(q, query, &path)
}

/// Whether a keyword introduces a table name, and whether the position is a
/// CREATE target (resolved to path[0], not by existence).
fn table_intro(prev: &str, prev2: &str) -> (bool, bool) {
    match prev {
        "from" | "join" | "update" | "truncate" => return (true, false),
        "into" => return (true, false), // INSERT INTO
        "table" => match prev2 {
            // CREATE/ALTER/DROP/TRUNCATE TABLE
            "create" => return (true, true),
            "alter" | "drop" => return (true, false),
            _ => {}
        },
        _ => {}
    }
    (false, false)
}

fn rewrite_unqualified_tables(q: &Connection, sql: &str, path: &[&str]) -> String {
    let s = sql.as_bytes();
    let mut b: Vec<u8> = Vec::new();
    let mut prev: Cow<'_, str> = Cow::Borrowed("");
    let mut prev2: Cow<'_, str> = Cow::Borrowed("");
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            let j = skip_literal(sql, i, c);
            b.extend_from_slice(&s[i..j]);
            i = j;
            prev2 = std::mem::replace(&mut prev, Cow::Borrowed("\""));
            continue;
        }
        if is_ident_byte(c) {
            let start = i;
            while i < s.len() && (is_ident_byte(s[i]) || s[i] == b'.') {
                i += 1;
            }
            let word = &sql[start..i];
            let (intro, create) = table_intro(&prev, &prev2);
            if intro
                && !word.contains('.')
                && let Some(repl) = resolve_table_name(q, word, path, create)
            {
                b.extend_from_slice(repl.as_bytes());
                prev2 = std::mem::replace(&mut prev, lowercase(word));
                continue;
            }
            b.extend_from_slice(word.as_bytes());
            prev2 = std::mem::replace(&mut prev, lowercase(word));
            continue;
        }
        b.push(c);
        i += 1;
        if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
            continue; // whitespace keeps the keyword context
        }
        // any other punctuation ends a table position
        prev2 = Cow::Borrowed("");
        prev = Cow::Borrowed("");
    }
    builder_string(b)
}

/// Maps a bare table name to its schema-qualified form. For a CREATE target it
/// uses the first path schema; otherwise the first path schema that actually
/// holds the table (so a public table is left untouched).
fn resolve_table_name(q: &Connection, name: &str, path: &[&str], create: bool) -> Option<String> {
    if create {
        return Some(qualify_for(path[0], name));
    }
    for s in path {
        if schema_has_table(q, s, name) {
            return Some(qualify_for(s, name));
        }
    }
    None
}

/// Renders a schema-qualified name the way it is stored: one identifier holding
/// both parts.
fn qualify_for(schema: &str, name: &str) -> String {
    format!("\"{schema}.{name}\"")
}

/// Whether schema holds a table or view called name.
fn schema_has_table(q: &Connection, schema: &str, name: &str) -> bool {
    let args = [Value::Text(format!("{schema}.{name}"))];
    match do_query(
        q,
        "SELECT 1 FROM main.sqlite_master WHERE type IN ('table','view') AND name = ? LIMIT 1",
        &args,
        "",
    ) {
        Ok(rs) => !rs.rows.is_empty(),
        Err(_) => false,
    }
}
