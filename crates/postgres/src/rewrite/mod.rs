//! A client's statement to SQLite's SQL: [`translate`] takes the fast path, or the
//! tree's translation (crate::ast) from the translation cache (`cache`), or answers
//! Postgres' refusal. The modules beside them hold text readers the session's
//! handlers still use.

pub(crate) mod blank;
pub(crate) mod cache;
pub(crate) mod catalog;
pub(crate) mod ddl_syntax;
pub(crate) mod default_expr;
pub(crate) mod fastpath;
pub(crate) mod functions;
pub(crate) mod operators;
pub(crate) mod strings;
pub(crate) mod tokens;

use crate::error::{Error, Result};
pub use crate::rewrite::fastpath::needs_no_rewrite;
use overlite_core::SqlState;
use overlite_core::text::find_from;

/// sql, a client's statement, as SQLite SQL: the fast path, or the AST path's translation (crate::ast), kept
/// in the process's translation cache (`cache`), which says what a translation
/// depends on.
///
/// Where NULLs sort and what LIKE matches are settled on the fast path, which a plain
/// lookup with an ORDER BY or a LIKE takes: each is one scan of the tokens, made only
/// when its word is there.
///
/// # Errors
///
/// What Postgres answers a statement it refuses: a syntax error (42601) with its
/// position, a statement nested too deep (54001), an operator no type has (42883), a
/// derived column list too long (42P10).
pub fn translate(sql: &str) -> Result<String> {
    if fast_path(sql) {
        return Ok(sql.to_string());
    }
    cache::cached(sql, || crate::ast::translate(sql).or_else(|why| refused(sql, sql, why)))
}

/// The answer to a statement the tree does not translate (why): SQLite's own trigger
/// as it is, or Postgres' error for one Postgres refuses, read from raw, the statement
/// as the client sent it (sql is what the session made of it), or 0A000 for one
/// overlite has no translation of.
fn refused(raw: &str, sql: &str, why: &'static str) -> Result<String> {
    use crate::ast::{emit, parse, sqlite_trigger};
    match why {
        emit::POSTGRES_REFUSAL => {
            let Some(r) = emit::take_refusal() else {
                return Err(Error::with_state(
                    SqlState::INTERNAL_ERROR,
                    "a refusal without its error",
                ));
            };
            let mut err = Error::with_state(r.state, r.message);
            if let Some(h) = r.hint {
                err = err.with_hint(h);
            }
            // The tree's offsets are bytes of the text it parsed; the client counts
            // characters of what it sent, which is that text when nothing was expanded.
            if let Some(at) = r.location.filter(|_| raw == sql).and_then(|at| sql.get(..at))
                && let Ok(p) = u32::try_from(at.chars().count() + 1)
            {
                err = err.at_position(p);
            }
            Err(err)
        }
        "parse error" | parse::TOO_DEEP => {
            if sqlite_trigger::is_sqlite_trigger(raw) {
                return Ok(raw.to_string());
            }
            Err(parse::error_of(raw)
                .or_else(|| parse::error_of(sql))
                .unwrap_or_else(|| Error::with_state(SqlState::SYNTAX_ERROR, "syntax error")))
        }
        why => Err(Error::with_state(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("{why} is not supported"),
        )),
    }
}

/// `translate` for the SQL the server writes itself (a probe, a rebuilt table's DDL):
/// a statement it refuses goes to SQLite as it is, which refuses it in turn.
pub fn rewrite(sql: &str) -> String {
    translate(sql).unwrap_or_else(|_| sql.to_string())
}

/// `translate` of expanded, which the session made of raw by writing values into it
/// (`current_user` as its name, `nextval('s')` as a number, an RLS filter), with raw's
/// labels on its result columns, read from raw's tree: what nextval('s') became says
/// nothing of its name. It does not take the fast path, which writes no labels.
///
/// # Errors
///
/// As `rewrite`, of raw.
pub(crate) fn rewrite_expanded(raw: &str, expanded: &str) -> Result<String> {
    // Keyed by both: the labels are raw's, the translation expanded's.
    let key = [raw, "\0", expanded].concat();
    cache::cached(&key, || match crate::ast::result_labels(raw) {
        Some(labels) => crate::ast::translate_labelled(expanded, &labels).or_else(|why| refused(raw, expanded, why)),
        // No labels to give: one the grammar refuses, whatever its expansion reads as,
        // or a statement that returns no rows.
        None if crate::ast::parse::kept_or_new(raw).is_err() => refused(raw, expanded, "parse error"),
        None => crate::ast::translate(expanded).or_else(|why| refused(raw, expanded, why)),
    })
}

/// Whether sql takes the fast path, which sends it to SQLite as it is: a narrow shape
/// the printer writes as it is read (`needs_no_rewrite`), over no column whose type
/// the typed translation reads (a date or time, an array, a range, a narrow integer, a
/// numeric, a boolean..., `TableTypes::typed_differs`), and one Postgres' grammar
/// reads: a statement of its shape parsed once (`parses`).
pub(crate) fn fast_path(sql: &str) -> bool {
    needs_no_rewrite(sql) && !crate::ast::env::typed_differs(sql) && parses(sql)
}

/// Shapes (`ast::shape::of`: the text with its literals' values left out) of
/// statements the fast path saw Postgres' grammar read as one statement.
static PARSED: crate::ast::shape::Shapes = crate::ast::shape::Shapes::new();

/// Whether Postgres' grammar reads sql as one statement, which a statement of the
/// same shape answers without a parse: the values of literals do not change what the
/// grammar reads. A shape it refuses is parsed each time, and refused by the AST path
/// with Postgres' error: SQLite's dialect, which the fast path's words would take
/// (`INSERT OR IGNORE`), is not run.
fn parses(sql: &str) -> bool {
    let shape = crate::ast::shape::of(sql);
    if PARSED.holds(shape) {
        return true;
    }
    let ok = crate::ast::parse::kept_or_new(sql).is_ok_and(|p| p.stmts.len() == 1);
    if ok {
        PARSED.insert(shape);
    }
    ok
}

/// The first needle at or after from, as an absolute offset.
pub(crate) fn index_from(s: &str, needle: &str, from: usize) -> Option<usize> {
    match s.get(from..) {
        Some(t) => t.find(needle).map(|p| p + from),
        None => find_from(s, needle, from),
    }
}
