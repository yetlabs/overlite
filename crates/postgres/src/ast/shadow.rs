//! The two routes side by side on a running server: with OVERLITE_AST_SHADOW=n, one
//! statement in n that the AST path translates is translated by the text passes too,
//! and a difference `compare` cannot explain is logged as a warning with both outputs.
//! This is how a deployment shows the AST path agreeing with the text passes on its
//! own statements (docs/adr/0001-sql-ast.md, stage 1). Off by default: the log line
//! holds the statement, literals and all.
//!
//! Only translations are sampled, not cache hits, so a statement is compared at most
//! once per time it is translated.

use crate::ast::compare::{Verdict, compare};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// One translation in this many is compared; 0 compares none.
static EVERY: LazyLock<u64> = LazyLock::new(|| {
    std::env::var("OVERLITE_AST_SHADOW")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
});

static TRANSLATED: AtomicU64 = AtomicU64::new(0);
static COMPARED: AtomicU64 = AtomicU64::new(0);
static UNEXPLAINED: AtomicU64 = AtomicU64::new(0);

/// Compares ast, the AST path's output for sql after the passes `rewrite` runs on
/// either route, with what text gives, when sql falls in the sample.
pub(crate) fn check(sql: &str, ast: &str, text: impl FnOnce() -> String) {
    let every = *EVERY;
    if every == 0 || !TRANSLATED.fetch_add(1, Ordering::Relaxed).is_multiple_of(every) {
        return;
    }
    let old = text();
    let compared = COMPARED.fetch_add(1, Ordering::Relaxed) + 1;
    if compare(&old, ast) == Verdict::Unexplained {
        let n = UNEXPLAINED.fetch_add(1, Ordering::Relaxed) + 1;
        log::warn!(
            "ast shadow: difference {n} of {compared} compared: {sql:?}\n  text passes: {old}\n  AST path:    {ast}"
        );
    } else if compared.is_power_of_two() {
        log::info!(
            "ast shadow: {compared} statements compared, {} unexplained differences",
            UNEXPLAINED.load(Ordering::Relaxed)
        );
    }
}
