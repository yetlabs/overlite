//! Row-level locking clauses have no equivalent in SQLite, which serialises
//! writers on the whole database instead. In a transaction, a SELECT ... FOR UPDATE
//! takes that write lock (the session does, by row_lock), which is a lock on every
//! row; the clause itself is then dropped.

use overlite_core::lazy_regex;

// Matches the locking clause at the end of a SELECT, with its optional
// table list and wait behaviour.
lazy_regex!(
    RE_ROW_LOCK,
    concat!(
        r"(?is)\s+FOR\s+(?:UPDATE|NO\s+KEY\s+UPDATE|SHARE|KEY\s+SHARE)",
        r"(?:\s+OF\s+[^\s,]+(?:\s*,\s*[^\s,]+)*)?",
        r"(?:\s+NOWAIT|\s+SKIP\s+LOCKED)?"
    )
);

/// How a SELECT's row-locking clause waits for a lock another transaction holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowLock {
    Wait,
    NoWait,
    SkipLocked,
}

/// The row-locking clause of a SELECT, if it has one, by its code.
pub(crate) fn row_lock(sql: &str) -> Option<RowLock> {
    let head = sql.trim_start().as_bytes();
    let starts = |w: &[u8]| head.len() >= w.len() && head[..w.len()].eq_ignore_ascii_case(w);
    if !(starts(b"select") || starts(b"with") || starts(b"(")) {
        return None;
    }
    let code = overlite_engine::code_only(sql);
    let m = RE_ROW_LOCK.find(&code)?;
    let clause = m.as_str().to_ascii_lowercase();
    Some(if clause.ends_with("nowait") {
        RowLock::NoWait
    } else if clause.ends_with("locked") {
        RowLock::SkipLocked
    } else {
        RowLock::Wait
    })
}

lazy_regex!(
    RE_FROM_TABLE,
    r#"(?is)\bFROM\s+(?:ONLY\s+)?((?:"[^"]+"|[\w$]+)(?:\.(?:"[^"]+"|[\w$]+))?)"#
);

/// The first table a locking SELECT reads, as Postgres names it in 55P03.
pub(crate) fn locked_table(sql: &str) -> String {
    let code = overlite_engine::blank_literals(sql);
    RE_FROM_TABLE
        .captures(&code)
        .and_then(|m| m.get(1))
        .map(|m| {
            let name = m.as_str();
            let last = name.rsplit('.').next().unwrap_or(name);
            last.trim_matches('"').to_string()
        })
        .unwrap_or_default()
}
