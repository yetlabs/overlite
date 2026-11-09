//! LISTEN/NOTIFY, delivered in-memory to every session of this process. A
//! session's writes and a NOTIFY's never overlap: a NOTIFY writes
//! to the socket only while the session is idle, under `NotifyState::mu`.

use crate::session::Session;
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

    /// Handles LISTEN / UNLISTEN / NOTIFY, read from the statement's tree (the
    /// channel's name as the grammar folds it, the payload as the literal means it);
    /// None for anything else.
    pub(crate) fn try_listen_notify(&mut self, sql: &str) -> Option<Command> {
        let stmt = crate::ast::classify::read(sql, |n| match n {
            NodeEnum::ListenStmt(l) => Some(Notice::Listen(l.conditionname.clone())),
            NodeEnum::UnlistenStmt(u) => Some(Notice::Unlisten(u.conditionname.clone())),
            NodeEnum::NotifyStmt(n) => Some(Notice::Notify(n.conditionname.clone(), n.payload.clone())),
            _ => None,
        })??;
        Some(match stmt {
            Notice::Listen(ch) => {
                if !ch.is_empty() {
                    self.listens.insert(ch.clone());
                    register_listen(&ch, &self.notify);
                }
                Command::from("LISTEN")
            }
            Notice::Unlisten(ch) if ch.is_empty() => {
                // UNLISTEN *, which the grammar reads as no channel.
                for ch in self.listens.drain() {
                    unregister_listen(&ch, &self.notify);
                }
                Command::from("UNLISTEN")
            }
            Notice::Unlisten(ch) => {
                self.listens.remove(&ch);
                unregister_listen(&ch, &self.notify);
                Command::from("UNLISTEN")
            }
            Notice::Notify(ch, payload) => {
                if !ch.is_empty() {
                    publish_notify(&ch, &payload, self.pid);
                }
                Command::from("NOTIFY")
            }
        })
    }
}

/// LISTEN, UNLISTEN (of no channel: all) or NOTIFY, with its payload.
enum Notice {
    Listen(String),
    Unlisten(String),
    Notify(String, String),
}

/// Whether sql is a LISTEN/UNLISTEN/NOTIFY statement.
pub(crate) fn is_listen_notify(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::listen_notify).unwrap_or_default()
}
