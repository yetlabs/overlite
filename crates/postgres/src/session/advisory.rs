//! Advisory locks: pg_advisory_lock and its family, which migration tools take so
//! two deploys never migrate at once (Prisma, Rails, Flyway, migrate).
//! SQLite has nothing like them, so they live here, in the server: one registry
//! for every connection, a lock per database and key, owned by sessions (by pid).
//!
//! Like nextval, a call is evaluated before the statement reaches SQLite and
//! replaced by its result, so `SELECT pg_try_advisory_lock(42)` runs as
//! `SELECT 1 AS pg_try_advisory_lock`. A session-level lock lasts until it is
//! unlocked as many times as it was taken, or the connection closes; a
//! transaction-level one (the _xact_ forms) until the transaction ends. A lock
//! call that has to wait does, until the holder lets go or the client cancels.

use crate::auth::rls::text_or_empty;
use crate::ddl::sequences::{parse_int, read_paren_args, scan_func_calls};
use crate::error::{Result, errf};
use crate::session::Session;
use crate::session::cancel::Canceler;
use crate::types::param_types::split_top;
use overlite_core::SqlState;
use overlite_core::Value;
use overlite_core::sync::MutexExt;
use overlite_core::text::{eq_fold, quoted, split_words, trim_chars, trim_end_chars};
use std::collections::{HashMap, HashSet};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Names a lock. The one-bigint and two-int4 forms are separate key spaces, as in
/// Postgres.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
struct AdvisoryKey {
    database: String,
    pair: bool,
    a: i64,
    b: i64,
}

/// Who holds a lock: at most one session exclusively, any number shared, each as
/// many times as it took it. Sessions are identified by pid.
#[derive(Clone, Debug, Default)]
struct AdvisoryHold {
    exclusive: Option<i32>,
    excl_count: i32,
    shared: HashMap<i32, i32>,
}

#[derive(Debug)]
struct AdvisoryLocks {
    mu: Mutex<HashMap<AdvisoryKey, AdvisoryHold>>,
    /// Notified whenever a lock is let go, waking every waiter to try again.
    released: Condvar,
}

static ADVISORY_LOCKS: LazyLock<AdvisoryLocks> = LazyLock::new(|| AdvisoryLocks {
    mu: Mutex::new(HashMap::new()),
    released: Condvar::new(),
});

/// How often a waiter wakes to look at its canceler.
const ADVISORY_POLL: Duration = Duration::from_millis(10);

/// The functions handled here, by name.
static ADVISORY_FUNCS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "pg_advisory_lock",
        "pg_advisory_lock_shared",
        "pg_try_advisory_lock",
        "pg_try_advisory_lock_shared",
        "pg_advisory_xact_lock",
        "pg_advisory_xact_lock_shared",
        "pg_try_advisory_xact_lock",
        "pg_try_advisory_xact_lock_shared",
        "pg_advisory_unlock",
        "pg_advisory_unlock_shared",
        "pg_advisory_unlock_all",
    ]
    .into_iter()
    .collect()
});

pub(crate) fn has_advisory_func(sql: &str) -> bool {
    sql.to_lowercase().contains("advisory_")
}

impl Session {
    /// Evaluates every advisory lock call in sql and replaces it with its result:
    /// '' for the void ones, 1/0 for the rest. A statement that is just the call
    /// keeps the column name Postgres gives it.
    pub(crate) fn expand_advisory_locks(&mut self, sql: &str, params: &[Value]) -> Result<String> {
        if !has_advisory_func(sql) {
            return Ok(sql.to_string());
        }
        let mut calls = 0;
        let out = scan_func_calls(sql, is_advisory_func, |f, args| {
            calls += 1;
            self.eval_advisory_func(f, args, params)
        })?;
        Ok(name_sole_call(sql, &out, calls))
    }
}

/// Stands every call in for a result of its type, taking no lock, for Describe.
pub(crate) fn neutralize_advisory_locks(sql: &str) -> String {
    if !has_advisory_func(sql) {
        return sql.to_string();
    }
    let mut calls = 0;
    let out = scan_func_calls(sql, is_advisory_func, |f, _| {
        calls += 1;
        Ok(advisory_result(f, true))
    })
    .unwrap_or_default();
    name_sole_call(sql, &out, calls)
}

fn is_advisory_func(f: &str) -> bool {
    ADVISORY_FUNCS.contains(f)
}

/// Aliases the result of `SELECT pg_...(...)` after the function, as Postgres
/// names that column, so a client reading it by name finds it.
fn name_sole_call(orig: &str, expanded: &str, calls: i32) -> String {
    if calls != 1 {
        return expanded.to_string();
    }
    let f = split_words(trim_end_chars(orig.trim(), ";"));
    if f.len() < 2 || !eq_fold(f[0], "SELECT") {
        return expanded.to_string();
    }
    let rest = trim_end_chars(orig.trim(), ";").trim();
    let rest = rest[f[0].len()..].trim();
    let Some(open) = rest.find('(') else {
        return expanded.to_string();
    };
    let func = rest[..open].trim().to_lowercase();
    if !ADVISORY_FUNCS.contains(func.as_str()) {
        return expanded.to_string();
    }
    match read_paren_args(rest, open) {
        Some((_, end)) if rest[end..].trim().is_empty() => {}
        _ => return expanded.to_string(),
    }
    trim_end_chars(expanded.trim(), ";").to_string() + " AS " + &func
}

/// What a call returns, as a literal.
fn advisory_result(f: &str, ok: bool) -> String {
    match f {
        "pg_advisory_lock" | "pg_advisory_lock_shared" | "pg_advisory_xact_lock" | "pg_advisory_xact_lock_shared" => {
            return "''".to_string(); // void
        }
        _ => {}
    }
    if ok { "1".to_string() } else { "0".to_string() }
}

impl Session {
    fn eval_advisory_func(&mut self, f: &str, args: &str, params: &[Value]) -> Result<String> {
        if f == "pg_advisory_unlock_all" {
            release_advisory_locks(self);
            self.tx_advisory.clear();
            return Ok("''".to_string());
        }
        let key = match self.advisory_key(args, params) {
            Ok(k) => k,
            Err(e) => return Err(errf!(e.sql_state(), "{}: {}", f, e)),
        };
        let shared = f.ends_with("_shared");
        if f.starts_with("pg_advisory_unlock") {
            Ok(advisory_result(f, unlock_advisory(self.pid, &key, shared)))
        } else if f.starts_with("pg_try_") {
            let ok = try_advisory(self.pid, &key, shared);
            if ok && f.contains("_xact_") {
                self.hold_for_tx(key, shared);
            }
            Ok(advisory_result(f, ok))
        } else {
            self.canceler.arm();
            let waited = wait_advisory(&self.canceler, self.pid, &key, shared);
            self.canceler.disarm();
            waited?;
            if f.contains("_xact_") {
                self.hold_for_tx(key, shared);
            }
            Ok(advisory_result(f, true))
        }
    }

    /// Reads a call's one bigint or two int4 arguments: literals, casts of them,
    /// or parameters.
    fn advisory_key(&self, args: &str, params: &[Value]) -> Result<AdvisoryKey> {
        let parts = split_top(args);
        if parts.len() != 1 && parts.len() != 2 {
            return Err(errf!(
                SqlState::UNDEFINED_FUNCTION,
                "expects one bigint or two integer keys"
            ));
        }
        let mut nums = vec![0i64; parts.len()];
        for (i, p) in parts.iter().enumerate() {
            let mut p = p.trim();
            if let Some(j) = p.find("::") {
                p = p[..j].trim();
            }
            p = trim_chars(p, "()").trim();
            let param;
            if p.starts_with('$') {
                let n = match parse_int(&p[1..]) {
                    Some(n) if n >= 1 && n as usize <= params.len() => n,
                    _ => return Err(errf!(SqlState::UNDEFINED_PARAMETER, "no value for parameter {}", p)),
                };
                param = text_or_empty(&params[(n - 1) as usize]);
                p = param.trim();
            }
            let Some(v) = parse_int(trim_chars(p, "'")) else {
                return Err(errf!(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    "key {:?} is not an integer",
                    quoted(&parts[i])
                ));
            };
            nums[i] = v;
        }
        let mut key = AdvisoryKey {
            database: self.database.clone(),
            a: nums[0],
            ..Default::default()
        };
        if nums.len() == 2 {
            key.pair = true;
            key.b = nums[1];
        }
        Ok(key)
    }
}

impl AdvisoryHold {
    /// Reports whether pid may take the lock now: exclusively when nobody else
    /// holds it at all, shared when nobody else holds it exclusively.
    fn can_take(&self, pid: i32, shared: bool) -> bool {
        if self.exclusive.is_some() && self.exclusive != Some(pid) {
            return false;
        }
        if shared {
            return true;
        }
        for holder in self.shared.keys() {
            if *holder != pid {
                return false;
            }
        }
        true
    }

    fn take(&mut self, pid: i32, shared: bool) {
        if shared {
            *self.shared.entry(pid).or_insert(0) += 1;
            return;
        }
        self.exclusive = Some(pid);
        self.excl_count += 1;
    }
}

/// try_advisory with the registry already locked.
fn try_advisory_locked(
    holds: &mut HashMap<AdvisoryKey, AdvisoryHold>,
    pid: i32,
    key: &AdvisoryKey,
    shared: bool,
) -> bool {
    if let Some(h) = holds.get_mut(key) {
        if !h.can_take(pid, shared) {
            return false;
        }
        h.take(pid, shared);
    } else {
        let mut h = AdvisoryHold::default();
        h.take(pid, shared);
        holds.insert(key.clone(), h);
    }
    true
}

/// Takes the lock if it is free to pid, without waiting.
fn try_advisory(pid: i32, key: &AdvisoryKey, shared: bool) -> bool {
    let mut holds = ADVISORY_LOCKS.mu.locked();
    try_advisory_locked(&mut holds, pid, key, shared)
}

/// Takes the lock, waiting for it until the canceler fires. The caller arms the
/// canceler. A try and the wait after it share the registry lock, so a release
/// cannot slip between them; the timeout is what lets a cancel be noticed.
fn wait_advisory(canceler: &Canceler, pid: i32, key: &AdvisoryKey, shared: bool) -> Result<()> {
    let mut holds: MutexGuard<'_, HashMap<AdvisoryKey, AdvisoryHold>> = ADVISORY_LOCKS.mu.locked();
    loop {
        if try_advisory_locked(&mut holds, pid, key, shared) {
            return Ok(());
        }
        let (guard, _) = ADVISORY_LOCKS
            .released
            .wait_timeout(holds, ADVISORY_POLL)
            .unwrap_or_else(PoisonError::into_inner);
        holds = guard;
        if canceler.is_cancelled() {
            return Err(errf!(
                SqlState::QUERY_CANCELED,
                "canceling statement due to user request"
            ));
        }
    }
}

/// Lets go of one hold pid has on the lock, reporting whether it had one.
fn unlock_advisory(pid: i32, key: &AdvisoryKey, shared: bool) -> bool {
    let mut holds = ADVISORY_LOCKS.mu.locked();
    let Some(h) = holds.get_mut(key) else { return false };
    if shared && let Some(c) = h.shared.get_mut(&pid).filter(|c| **c > 0) {
        *c -= 1;
        if *c == 0 {
            h.shared.remove(&pid);
        }
    } else if !shared && h.exclusive == Some(pid) {
        h.excl_count -= 1;
        if h.excl_count == 0 {
            h.exclusive = None;
        }
    } else {
        return false;
    }
    if h.exclusive.is_none() && h.shared.is_empty() {
        holds.remove(key);
    }
    wake_advisory_waiters();
    true
}

/// Lets go of every lock s holds: pg_advisory_unlock_all, and the connection
/// closing. Takes `&Session`, so clearing s.tx_advisory falls to the
/// pg_advisory_unlock_all caller; a closing connection has no use for it.
pub(crate) fn release_advisory_locks(s: &Session) {
    let pid = s.pid;
    let mut holds = ADVISORY_LOCKS.mu.locked();
    holds.retain(|_, h| {
        if h.exclusive == Some(pid) {
            h.exclusive = None;
            h.excl_count = 0;
        }
        h.shared.remove(&pid);
        !(h.exclusive.is_none() && h.shared.is_empty())
    });
    wake_advisory_waiters();
}

/// Tells every waiting lock call to try again. The caller holds the registry lock.
fn wake_advisory_waiters() {
    ADVISORY_LOCKS.released.notify_all();
}

/// A lock taken for the current transaction.
#[derive(Clone, Debug)]
pub(crate) struct TxAdvisoryHold {
    key: AdvisoryKey,
    shared: bool,
}

impl Session {
    /// Records a transaction-level lock, to let go when the transaction ends;
    /// outside one, the statement is the transaction, so it goes at once.
    fn hold_for_tx(&mut self, key: AdvisoryKey, shared: bool) {
        if !self.in_tx() {
            unlock_advisory(self.pid, &key, shared);
            return;
        }
        self.tx_advisory.push(TxAdvisoryHold { key, shared });
    }

    /// Lets go of the transaction's locks as it ends.
    pub(crate) fn release_tx_advisory_locks(&mut self) {
        for h in &self.tx_advisory {
            unlock_advisory(self.pid, &h.key, h.shared);
        }
        self.tx_advisory.clear();
    }
}
