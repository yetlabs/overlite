//! The translation (docs/adr/0001-sql-ast.md): a statement parsed by Postgres' own
//! grammar (libpg_query, which the pg_query crate builds and links) and printed as
//! SQLite SQL, typed from the catalog (`env`, `types`).
//!
//! What it does not translate is refused: with Postgres' own error where Postgres
//! refuses the statement too (a parse error, an operator no type has), else as a
//! feature overlite does not have (0A000). SQLite's own CREATE TRIGGER is the one
//! statement taken as written (`sqlite_trigger`). The corpus in
//! tests/fixtures/ast-corpus is a snapshot of the printer's output (`snapshot`).

pub(crate) mod classify;
pub(crate) mod ddl;
pub(crate) mod emit;
pub(crate) mod env;
pub(crate) mod labels;
pub(crate) mod params;
pub(crate) mod parse;
pub(crate) mod record;
pub(crate) mod shape;
pub(crate) mod sqlite_trigger;
pub(crate) mod types;

#[cfg(test)]
pub(crate) mod snapshot;

use std::rc::Rc;

/// The statement as SQLite SQL, or why it is not translated: typed from the catalog
/// of the thread's context (`env`), or of an empty one when the thread has none.
///
/// # Errors
///
/// The reason it is not translated: a parse error, or the first node the printer
/// does not write (`emit::POSTGRES_REFUSAL` for one Postgres refuses too).
pub(crate) fn translate(sql: &str) -> Result<String, &'static str> {
    let parsed = parse::kept_or_new(sql)?;
    let ctx = env::current_or_empty();
    let one = |stmt: &pg_query::NodeEnum| Ok(emit::typed(stmt, None, Rc::clone(&ctx))?.sql);
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

/// Postgres' own error for sql, when its grammar refuses it, answered before anything
/// else reads the statement, as Postgres parses first: a syntax error (42601) with its
/// position, or a statement nested too deep (54001). SQLite's trigger
/// (`sqlite_trigger`) is no refusal. A statement that reads or writes rows is not
/// parsed here: its translation, which parses it anyway, answers the same.
pub(crate) fn grammar_refusal(sql: &str) -> Option<crate::error::Error> {
    if crate::rewrite::cache::reads_or_writes_rows(sql) {
        return None;
    }
    grammar_refusal_parsing(sql)
}

/// `grammar_refusal` of any statement, one that reads or writes rows included, which it
/// parses (and keeps parsed, `parse::shared`): what the privilege check, which reads
/// the tree, answers before any grant.
pub(crate) fn grammar_refusal_parsing(sql: &str) -> Option<crate::error::Error> {
    if parse::shared(sql).is_ok() {
        return None;
    }
    if sqlite_trigger::is_sqlite_trigger(sql) {
        return None;
    }
    parse::error_of(sql)
}

/// The type the environment gives each parameter of sql, `$1` first, when the AST
/// path translates it with the thread's context: what Describe answers.
pub(crate) fn param_types(sql: &str) -> Option<Vec<types::Ty>> {
    let ctx = env::current()?;
    let parsed = parse::kept_or_new(sql).ok()?;
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
    let parsed = parse::kept_or_new(sql).ok()?;
    labels::result_labels(one_statement(&parsed).ok()?)
}

/// sql as SQLite SQL with these labels on its result columns (`emit::typed`).
///
/// # Errors
///
/// Why it is not translated, as `translate`.
pub(crate) fn translate_labelled(sql: &str, labels: &[Option<String>]) -> Result<String, &'static str> {
    let parsed = parse::kept_or_new(sql)?;
    Ok(emit::typed(one_statement(&parsed)?, Some(labels), env::current_or_empty())?.sql)
}
