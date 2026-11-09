//! The AST path (docs/adr/0001-sql-ast.md, on by default since stage 1): a statement
//! parsed by Postgres' own grammar (libpg_query, which the pg_query crate builds and
//! links) and printed as SQLite SQL.
//!
//! It covers a slice of the language. [`translate`] answers None for anything outside
//! it, and `rewrite` then runs the text passes as before, so the slice can grow one node
//! at a time. The differential test (`diff.rs`) holds the two paths to the same SQLite
//! SQL over the corpus in tests/fixtures/ast-corpus, and lists where they differ on
//! purpose.

pub(crate) mod compare;
pub(crate) mod emit;
pub(crate) mod labels;
pub(crate) mod params;
pub(crate) mod parse;
pub(crate) mod record;
pub(crate) mod shadow;
pub(crate) mod shape;

#[cfg(test)]
pub(crate) mod diff;

use std::sync::LazyLock;

/// Whether the AST path is on: OVERLITE_AST=0 turns it off, to compare.
pub(crate) static ENABLED: LazyLock<bool> = LazyLock::new(|| std::env::var("OVERLITE_AST").map_or(true, |v| v != "0"));

/// The statement as SQLite SQL, or why the AST path leaves it to the text passes.
///
/// # Errors
///
/// The reason it is not translated: a parse error, more than one statement, or the
/// first node outside the slice.
pub(crate) fn try_translate(sql: &str) -> Result<String, &'static str> {
    let parsed = parse::parse(sql)?;
    emit::statement(one_statement(&parsed)?)
}

/// The one statement sql holds, parsed.
fn one_statement(parsed: &pg_query::protobuf::ParseResult) -> Result<&pg_query::NodeEnum, &'static str> {
    let [raw] = parsed.stmts.as_slice() else {
        return Err("not one statement");
    };
    raw.stmt
        .as_deref()
        .and_then(|s| s.node.as_ref())
        .ok_or("empty statement")
}

/// The labels of sql's result columns (`labels::result_labels`), read from its tree:
/// those of a statement as the client sent it, before the session expands it.
pub(crate) fn result_labels(sql: &str) -> Option<Vec<Option<String>>> {
    let parsed = parse::parse(sql).ok()?;
    labels::result_labels(one_statement(&parsed).ok()?)
}

/// sql as SQLite SQL with these labels on its result columns
/// (`emit::statement_labelled`), when the AST path covers it.
pub(crate) fn translate_labelled(sql: &str, labels: &[Option<String>]) -> Option<String> {
    let shape = shape::of(sql);
    if shape::OUTSIDE.holds(shape) {
        return None;
    }
    let parsed = parse::parse(sql).ok()?;
    let out = one_statement(&parsed)
        .ok()
        .and_then(|stmt| emit::statement_labelled(stmt, Some(labels)).ok());
    if out.is_none() {
        shape::OUTSIDE.insert(shape);
    }
    out
}

/// The statement as SQLite SQL, when the AST path covers all of it.
///
/// A statement whose shape (`shape::of`: the text with its literals' values left out)
/// the AST path has already left to the text passes is not parsed again: a client
/// writing its values into the text sends one shape with ever new texts, which the
/// translation cache cannot hold, and each would pay a parse only to be refused.
pub(crate) fn translate(sql: &str) -> Option<String> {
    let shape = shape::of(sql);
    if shape::OUTSIDE.holds(shape) {
        return None;
    }
    let out = try_translate(sql).ok();
    if out.is_none() {
        shape::OUTSIDE.insert(shape);
    }
    out
}
