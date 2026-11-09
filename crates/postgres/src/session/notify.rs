//! LISTEN/NOTIFY, delivered in-memory to every session of this process. A
//! session's writes and a NOTIFY's never overlap: a NOTIFY writes
//! to the socket only while the session is idle, under `NotifyState::mu`.

use crate::ddl::tree;
use crate::session::Session;
use crate::session::utility::first_word_upper;
use crate::wire::{MSG_NOTIFICATION, NetStream, append_cstring, i32_be};
use overlite_core::Command;
use overlite_core::sync::MutexExt;
use pg_query::NodeEnum;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

#[derive(Debug, Clone)]
struct PgNotify {
    pid: i32,
    channel: String,
    payload: String,
}

#[derive(Debug, Default)]
struct NotifyInner {
    pending: Vec<PgNotify>,
    idle: bool,
}

/// The part of a session another session's NOTIFY reaches.
#[derive(Debug)]
pub(crate) struct NotifyState {
    mu: Mutex<NotifyInner>,
    sink: NetStream,
}

impl NotifyState {
    pub(crate) fn new(sink: NetStream) -> Arc<NotifyState> {
        Arc::new(NotifyState {
            mu: Mutex::new(NotifyInner::default()),
            sink,
        })
    }

    fn key(self: &Arc<Self>) -> usize {
        Arc::as_ptr(self) as usize
    }

    fn deliver_locked(&self, inner: &mut NotifyInner) {
        if inner.pending.is_empty() {
            return;
        }
        let mut out = Vec::new();
        for n in inner.pending.drain(..) {
            let mut b = i32_be(n.pid).to_vec();
            append_cstring(&mut b, &n.channel);
            append_cstring(&mut b, &n.payload);
            out.push(MSG_NOTIFICATION);
            out.extend_from_slice(&((b.len() + 4) as i32).to_be_bytes());
            out.extend_from_slice(&b);
        }
        let _ = self.sink.write_all(&out);
    }

    fn enqueue(&self, n: PgNotify) {
        let mut g = self.mu.locked();
        g.pending.push(n);
        if g.idle {
            self.deliver_locked(&mut g);
        }
    }
}

type Channels = HashMap<String, HashMap<usize, Arc<NotifyState>>>;

static NOTIFY_REGISTRY: LazyLock<Mutex<Channels>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn register_listen(ch: &str, s: &Arc<NotifyState>) {
    NOTIFY_REGISTRY
        .locked()
        .entry(ch.to_string())
        .or_default()
        .insert(s.key(), s.clone());
}

fn unregister_listen(ch: &str, s: &Arc<NotifyState>) {
    let mut reg = NOTIFY_REGISTRY.locked();
    if let Some(m) = reg.get_mut(ch) {
        m.remove(&s.key());
        if m.is_empty() {
            reg.remove(ch);
        }
    }
}

pub(crate) fn unregister_all(s: &Arc<NotifyState>) {
    let mut reg = NOTIFY_REGISTRY.locked();
    let k = s.key();
    reg.retain(|_, m| {
        m.remove(&k);
        !m.is_empty()
    });
}

/// Delivers a notification to every session listening on channel.
fn publish_notify(channel: &str, payload: &str, pid: i32) {
    let targets: Vec<Arc<NotifyState>> = NOTIFY_REGISTRY
        .locked()
        .get(channel)
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default();
    for s in targets {
        s.enqueue(PgNotify {
            pid,
            channel: channel.to_string(),
            payload: payload.to_string(),
        });
    }
}

impl Session {
    /// While busy, only this session writes to the connection.
    pub(crate) fn set_busy(&self) {
        self.notify.mu.locked().idle = false;
    }

    pub(crate) fn set_idle(&self) {
        let mut g = self.notify.mu.locked();
        g.idle = true;
        self.notify.deliver_locked(&mut g);
    }

    /// Handles LISTEN / UNLISTEN / NOTIFY, None for anything else. A channel is a
    /// name, folded unless quoted, as Postgres reads it.
    pub(crate) fn try_listen_notify(&mut self, sql: &str) -> Option<Command> {
        if !is_listen_notify(sql) {
            return None;
        }
        let node = tree::read(sql, |n| match n {
            NodeEnum::ListenStmt(_) | NodeEnum::UnlistenStmt(_) | NodeEnum::NotifyStmt(_) => Some(n.clone()),
            _ => None,
        })?;
        Some(match node {
            NodeEnum::ListenStmt(l) => {
                self.listens.insert(l.conditionname.clone());
                register_listen(&l.conditionname, &self.notify);
                Command::from("LISTEN")
            }
            NodeEnum::UnlistenStmt(u) if u.conditionname.is_empty() => {
                // UNLISTEN *
                for ch in self.listens.drain() {
                    unregister_listen(&ch, &self.notify);
                }
                Command::from("UNLISTEN")
            }
            NodeEnum::UnlistenStmt(u) => {
                self.listens.remove(&u.conditionname);
                unregister_listen(&u.conditionname, &self.notify);
                Command::from("UNLISTEN")
            }
            NodeEnum::NotifyStmt(n) => {
                publish_notify(&n.conditionname, &n.payload, self.pid);
                Command::from("NOTIFY")
            }
            _ => return None,
        })
    }
}

/// Whether sql is a LISTEN/UNLISTEN/NOTIFY statement.
pub(crate) fn is_listen_notify(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::listen_notify).unwrap_or_else(|| is_listen_notify_text(sql))
}

/// `is_listen_notify` by the text, for what Postgres' grammar does not read.
pub(crate) fn is_listen_notify_text(sql: &str) -> bool {
    matches!(first_word_upper(sql).as_str(), "LISTEN" | "UNLISTEN" | "NOTIFY")
}
