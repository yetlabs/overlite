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
pub(crate) mod ddl;
pub(crate) mod emit;
pub(crate) mod env;
pub(crate) mod labels;
pub(crate) mod params;
pub(crate) mod parse;
pub(crate) mod record;
pub(crate) mod shadow;
pub(crate) mod shape;
pub(crate) mod types;

#[cfg(test)]
pub(crate) mod diff;

use std::sync::LazyLock;

/// Whether the AST path is on: OVERLITE_AST=0 turns it off, to compare.
pub(crate) static ENABLED: LazyLock<bool> = LazyLock::new(|| std::env::var("OVERLITE_AST").map_or(true, |v| v != "0"));

/// The statement as SQLite SQL, or why the AST path leaves it to the text passes:
/// typed from the catalog when the thread has a context (`env`), untyped otherwise.
///
/// # Errors
///
/// The reason it is not translated: a parse error, more than one statement, or the
/// first node outside the slice.
pub(crate) fn try_translate(sql: &str) -> Result<String, &'static str> {
    let parsed = parse::parse(sql)?;
    let one = |stmt: &pg_query::NodeEnum| match env::current() {
        Some(ctx) => Ok(emit::typed(stmt, None, ctx)?.sql),
        None => emit::statement(stmt),
    };
    if parsed.stmts.len() > 1 {
        // A Parse holding more than one statement (a Query's are split before): each
        // translated, as the text passes run over them all.
        // The engine reads the mark at the end only: the others' go.
        let mut out = Vec::with_capacity(parsed.stmts.len());
        for (i, raw) in parsed.stmts.iter().enumerate() {
            let stmt = raw
                .stmt
                .as_deref()
                .and_then(|s| s.node.as_ref())
                .ok_or("empty statement")?;
            let sql = one(stmt)?;
            out.push(if i + 1 < parsed.stmts.len() {
                split_mark(&sql).0.to_string()
            } else {
                sql
            });
        }
        return Ok(out.join("; "));
    }
    let stmt = one_statement(&parsed)?;
    if ddl::takes(stmt) {
        return ddl::translate(sql, stmt);
    }
    one(stmt)
}

/// The type the environment gives each parameter of sql, `$1` first, when the AST
/// path translates it with the thread's context: what Describe answers.
pub(crate) fn param_types(sql: &str) -> Option<Vec<types::Ty>> {
    let ctx = env::current()?;
    let parsed = parse::parse(sql).ok()?;
    Some(emit::typed(one_statement(&parsed).ok()?, None, ctx).ok()?.params)
}

/// sql between prefix and suffix, the mark of a typed translation kept at its end, where
/// the engine reads it.
pub(crate) fn wrap_typed(sql: &str, prefix: &str, suffix: &str) -> String {
    let (body, mark) = split_mark(sql);
    [prefix, body, suffix, mark].concat()
}

/// A translation and the engine's mark it ends in, which is empty for an untyped one.
pub(crate) fn split_mark(sql: &str) -> (&str, &str) {
    match sql.rfind(&[" ", overlite_engine::TYPED_MARK].concat()) {
        Some(at) if sql.ends_with("*/") => sql.split_at(at),
        _ => (sql, ""),
    }
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

/// Whether the AST path leaves statements of this shape to the text passes, with the
/// thread's context or without: the typed path covers more than the untyped one, and
/// what the types refuse is not the shape's to remember.
fn outside(shape: u64) -> bool {
    shape::OUTSIDE.holds(shape ^ mode())
}

fn remember_outside(shape: u64, why: &'static str) {
    if !matches!(
        why,
        emit::TYPE_REFUSAL_OPERANDS
            | emit::TYPE_REFUSAL_BOOLEAN
            | emit::TYPE_REFUSAL_JSON
            | emit::TYPE_REFUSAL_RANGE
            | emit::TYPE_REFUSAL_STAR
    ) {
        shape::OUTSIDE.insert(shape ^ mode());
    }
}

/// What tells a shape refused with a context from one refused without.
fn mode() -> u64 {
    if env::stamp() == 0 { 0 } else { 0x9e37_79b9_7f4a_7c15 }
}

/// sql as SQLite SQL with these labels on its result columns
/// (`emit::statement_labelled`), when the AST path covers it.
pub(crate) fn translate_labelled(sql: &str, labels: &[Option<String>]) -> Option<String> {
    let shape = shape::of(sql);
    if outside(shape) {
        return None;
    }
    let parsed = parse::parse(sql).ok()?;
    let out = one_statement(&parsed).and_then(|stmt| match env::current() {
        Some(ctx) => emit::typed(stmt, Some(labels), ctx).map(|p| p.sql),
        None => emit::statement_labelled(stmt, Some(labels)),
    });
    if let Err(why) = out {
        remember_outside(shape, why);
    }
    out.ok()
}

/// The statement as SQLite SQL, when the AST path covers all of it.
///
/// A statement whose shape (`shape::of`: the text with its literals' values left out)
/// the AST path has already left to the text passes is not parsed again: a client
/// writing its values into the text sends one shape with ever new texts, which the
/// translation cache cannot hold, and each would pay a parse only to be refused.
pub(crate) fn translate(sql: &str) -> Option<String> {
    let shape = shape::of(sql);
    if outside(shape) {
        return None;
    }
    let out = try_translate(sql);
    if let Err(why) = out {
        remember_outside(shape, why);
    }
    out.ok()
}
