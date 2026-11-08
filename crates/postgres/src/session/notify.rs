//! LISTEN/NOTIFY, delivered in-memory to every session of this process. A
//! session's writes and a NOTIFY's never overlap: a NOTIFY writes
//! to the socket only while the session is idle, under `NotifyState::mu`.

use crate::auth::grants::index_word;
use crate::rewrite::strings::end_of_string_literal;
use crate::session::Session;
use crate::session::utility::{first_word_upper, unquote_ident};
use crate::wire::{MSG_NOTIFICATION, NetStream, append_cstring, i32_be};
use overlite_core::Command;
use overlite_core::sync::MutexExt;
use overlite_core::text::{split_words, trim_end_chars};
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

    /// Handles LISTEN / UNLISTEN / NOTIFY, None for anything else.
    pub(crate) fn try_listen_notify(&mut self, sql: &str) -> Option<Command> {
        let f = split_words(sql);
        if f.is_empty() {
            return None;
        }
        match trim_end_chars(f[0], ";").to_uppercase().as_str() {
            "LISTEN" => {
                let ch = notify_channel(&f, 1);
                if !ch.is_empty() {
                    self.listens.insert(ch.clone());
                    register_listen(&ch, &self.notify);
                }
                Some(Command::from("LISTEN"))
            }
            "UNLISTEN" => {
                let arg = if f.len() > 1 { trim_end_chars(f[1], ";") } else { "" };
                if arg == "*" {
                    for ch in self.listens.drain() {
                        unregister_listen(&ch, &self.notify);
                    }
                } else {
                    let ch = notify_channel(&f, 1);
                    if !ch.is_empty() {
                        self.listens.remove(&ch);
                        unregister_listen(&ch, &self.notify);
                    }
                }
                Some(Command::from("UNLISTEN"))
            }
            "NOTIFY" => {
                let (ch, payload) = parse_notify(sql);
                if !ch.is_empty() {
                    publish_notify(&ch, &payload, self.pid);
                }
                Some(Command::from("NOTIFY"))
            }
            _ => None,
        }
    }
}

/// Whether sql is a LISTEN/UNLISTEN/NOTIFY statement.
pub(crate) fn is_listen_notify(sql: &str) -> bool {
    matches!(first_word_upper(sql).as_str(), "LISTEN" | "UNLISTEN" | "NOTIFY")
}

/// The (unquoted) channel identifier at position i.
fn notify_channel(f: &[&str], i: usize) -> String {
    if i >= f.len() {
        return String::new();
    }
    unquote_ident(trim_end_chars(f[i], ",;"))
}

/// Extracts the channel and optional 'payload' from a NOTIFY.
fn parse_notify(sql: &str) -> (String, String) {
    let low = sql.to_ascii_lowercase();
    let at = index_word(&low, "notify").unwrap_or(0) + "notify".len();
    let rest = sql[at..].trim();
    let b = rest.as_bytes();
    let mut comma: Option<usize> = None;
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_of_string_literal(rest, i);
            continue;
        }
        if b[i] == b'(' {
            depth += 1;
        } else if b[i] == b')' {
            depth -= 1;
        } else if b[i] == b',' && depth == 0 {
            comma = Some(i);
            break;
        }
        i += 1;
    }
    let Some(comma) = comma else {
        return (unquote_ident(trim_end_chars(rest.trim(), ";")), String::new());
    };
    let channel = unquote_ident(rest[..comma].trim());
    let p = trim_end_chars(&rest[comma + 1..], ";").trim();
    let mut payload = String::new();
    if p.len() >= 2 && p.as_bytes()[0] == b'\'' {
        payload = p[1..p.len() - 1].replace("''", "'");
    }
    (channel, payload)
}
