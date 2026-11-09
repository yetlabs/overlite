//! Query cancellation, following the Postgres model: the running statement is
//! interrupted directly.

use crate::error::{Error, ErrorKind, Result};
use crate::session::Session;
use overlite_core::SqlState;
use overlite_core::sync::MutexExt;
use overlite_engine::{ErrorKind as EngineKind, InterruptHandle};
use rand::RngCore;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// Whether err is the result of a cancelled query.
/// What a client is told: a statement SQLite was interrupted in was cancelled.
fn exec_message(err: &Error) -> &str {
    match err.kind() {
        ErrorKind::Engine(EngineKind::Sqlite {
            state: SqlState::QUERY_CANCELED,
            ..
        }) => "canceling statement due to user request",
        _ => err.message(),
    }
}

impl Session {
    /// Reports an execution error, in Postgres' words for a cancelled query.
    pub(crate) fn send_exec_error(&mut self, err: &Error) -> Result<()> {
        self.c.send_error_of(err, exec_message(err))
    }

    /// send_exec_error for the extended-query path (arms the skip-until-Sync flag).
    pub(crate) fn proto_exec_error(&mut self, err: &Error) -> Result<()> {
        self.failed = true;
        self.c.send_error_of(err, exec_message(err))
    }
}

/// Interrupts a connection's currently-running query. `armed` is true while a
/// statement runs; a cancel between statements is a no-op.
#[derive(Default)]
pub(crate) struct Canceler {
    handle: Mutex<Option<InterruptHandle>>,
    armed: AtomicBool,
    cancelled: AtomicBool,
}

impl std::fmt::Debug for Canceler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Canceler")
            .field("armed", &self.armed)
            .field("cancelled", &self.cancelled)
            .finish_non_exhaustive()
    }
}

impl Canceler {
    pub(crate) fn set_handle(&self, h: InterruptHandle) {
        *self.handle.locked() = Some(h);
    }

    pub(crate) fn arm(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }

    pub(crate) fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
        self.cancelled.store(false, Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel(&self) {
        if self.armed.load(Ordering::SeqCst) {
            self.cancelled.store(true, Ordering::SeqCst);
            if let Some(h) = self.handle.locked().as_ref() {
                h.interrupt();
            }
        }
    }
}

static BACKENDS: LazyLock<Mutex<HashMap<u64, Arc<Canceler>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn backend_key(pid: i32, secret: i32) -> u64 {
    ((pid as u32 as u64) << 32) | (secret as u32 as u64)
}

pub(crate) fn register_backend(pid: i32, secret: i32) -> Arc<Canceler> {
    let cl = Arc::new(Canceler::default());
    BACKENDS.locked().insert(backend_key(pid, secret), cl.clone());
    cl
}

pub(crate) fn unregister_backend(pid: i32, secret: i32) {
    BACKENDS.locked().remove(&backend_key(pid, secret));
}

/// Interrupts the running query of a backend; unknown pairs are ignored.
pub(crate) fn cancel_backend(pid: i32, secret: i32) {
    let cl = BACKENDS.locked().get(&backend_key(pid, secret)).cloned();
    if let Some(cl) = cl {
        cl.cancel();
    }
}

/// Interrupts every backend's running query, as a shutdown does.
pub(crate) fn cancel_all_backends() {
    let all: Vec<Arc<Canceler>> = BACKENDS.locked().values().cloned().collect();
    for cl in all {
        cl.cancel();
    }
}

/// The target backend of a CancelRequest startup packet.
#[derive(Debug)]
pub(crate) struct CancelRequest {
    pub(crate) pid: i32,
    pub(crate) secret: i32,
}

/// A random non-zero int32 for a backend pid/secret.
pub(crate) fn rand_int32() -> i32 {
    let v = rand::thread_rng().next_u32() as i32;
    if v == 0 { 1 } else { v }
}
