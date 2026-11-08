//! Accepts TCP connections and hands each one to the protocol. One OS thread per
//! client: every statement blocks in SQLite anyway.

use crate::UNSTAMPED_LOG_TARGET;
use crate::error::Result;
use crate::protocol::Protocol;
use overlite_core::sync::MutexExt;
use overlite_engine::Cluster;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a shutdown waits for sessions to end. A statement stuck where an
/// interrupt cannot reach it (waiting out busy_timeout) must not hold the
/// process past a supervisor's patience; the WAL is safe either way.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a session may spend reporting its cancelled statement before its
/// socket is closed for writing too, for a client that stopped reading.
const DRAIN_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// The connections being served, by accept order.
type Conns = Arc<Mutex<HashMap<u64, TcpStream>>>;

#[derive(Debug)]
pub struct Server {
    proto: Arc<Protocol>,
    cluster: Arc<dyn Cluster>,
    ln: Option<TcpListener>,
    conns: Conns,
    next: AtomicU64,
    stopping: Arc<AtomicBool>,
}

/// Makes `serve` return, from any thread.
#[derive(Debug)]
pub struct Stopper {
    stopping: Arc<AtomicBool>,
    addr: SocketAddr,
}

impl Stopper {
    /// A blocking accept cannot be interrupted portably, so a connection to the
    /// listener wakes it to find the flag set.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_secs(1));
    }
}

impl Server {
    /// Binds the listener.
    ///
    /// # Errors
    ///
    /// When the address cannot be bound.
    pub fn new(addr: &str, proto: Protocol, cluster: Arc<dyn Cluster>) -> std::io::Result<Server> {
        let ln = TcpListener::bind(addr)?;
        Ok(Server {
            proto: Arc::new(proto),
            cluster,
            ln: Some(ln),
            conns: Conns::default(),
            next: AtomicU64::new(0),
            stopping: Arc::default(),
        })
    }

    pub fn addr(&self) -> String {
        self.ln
            .as_ref()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.to_string())
            .unwrap_or_default()
    }

    /// Something that stops `serve`, reaching the listener over loopback when it
    /// is bound to every address.
    /// A handle that stops [`Server::serve`].
    ///
    /// # Errors
    ///
    /// When the server has stopped listening.
    pub fn stopper(&self) -> std::io::Result<Stopper> {
        let Some(ln) = self.ln.as_ref() else {
            return Err(std::io::ErrorKind::NotConnected.into());
        };
        let mut addr = ln.local_addr()?;
        if addr.ip().is_unspecified() {
            addr.set_ip(if addr.is_ipv4() {
                Ipv4Addr::LOCALHOST.into()
            } else {
                Ipv6Addr::LOCALHOST.into()
            });
        }
        Ok(Stopper {
            stopping: self.stopping.clone(),
            addr,
        })
    }

    /// Accepts connections until a Stopper fires, then closes the listener.
    ///
    /// # Errors
    ///
    /// When accepting fails for a reason other than the client going away.
    pub fn serve(&mut self) -> Result<()> {
        let served = self.accept_loop();
        self.ln = None;
        served
    }

    fn accept_loop(&self) -> Result<()> {
        let Some(ln) = self.ln.as_ref() else { return Ok(()) };
        loop {
            let accepted = ln.accept();
            if self.stopping.load(Ordering::SeqCst) {
                return Ok(());
            }
            let (conn, peer) = match accepted {
                Ok(c) => c,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let tracked = match conn.try_clone() {
                Ok(t) => t,
                Err(e) => {
                    log::error!(target: UNSTAMPED_LOG_TARGET, "postgres: connection {peer}: {e}");
                    continue;
                }
            };
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            self.conns.locked().insert(id, tracked);
            let proto = self.proto.clone();
            let cluster = self.cluster.clone();
            let conns = self.conns.clone();
            let spawned = std::thread::Builder::new()
                .stack_size(8 << 20)
                .spawn(move || handle(proto, conn, cluster, peer, Untrack { conns, id }));
            if let Err(e) = spawned {
                log::error!(target: UNSTAMPED_LOG_TARGET, "postgres: spawn: {e}");
                self.conns.locked().remove(&id);
            }
        }
    }

    /// Ends every session: a running statement is interrupted and reported, an
    /// open transaction rolls back, and an idle client sees its connection close.
    /// Waiting for that means each session closes its SQLite connection, so the
    /// last one out checkpoints the WAL.
    pub fn drain(&self) {
        let start = Instant::now();
        let mut writes_closed = false;
        loop {
            {
                let conns = self.conns.locked();
                if conns.is_empty() {
                    return;
                }
                // The write side alone, after the read side: macOS answers ENOTCONN to
                // Both on a socket whose read side is already shut, which left a
                // session blocked writing to a client that stopped reading.
                let how = if writes_closed { Shutdown::Write } else { Shutdown::Read };
                for c in conns.values() {
                    let _ = c.shutdown(how);
                }
            }
            // Again on every pass: a statement that began after the last one
            // missed it, as a batch moves on to its next statement.
            self.proto.cancel_running();
            if start.elapsed() >= DRAIN_TIMEOUT {
                let n = self.conns.locked().len();
                log::warn!("overlite: exiting with {n} connection(s) still open");
                return;
            }
            writes_closed = start.elapsed() >= DRAIN_WRITE_TIMEOUT;
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Forgets a connection once its session is over, panic included.
#[derive(Debug)]
#[must_use = "dropping it forgets the connection"]
struct Untrack {
    conns: Conns,
    id: u64,
}

impl Drop for Untrack {
    fn drop(&mut self) {
        self.conns.locked().remove(&self.id);
    }
}

fn handle(
    proto: Arc<Protocol>,
    conn: TcpStream,
    cluster: Arc<dyn Cluster>,
    peer: std::net::SocketAddr,
    _done: Untrack,
) {
    // One client must not be able to take the server down with it: a panic
    // ends this thread only (the default hook has already reported it).
    let _ = conn.set_nodelay(true); // a client waits on each small reply
    if let Err(e) = proto.serve(conn, cluster) {
        log::info!("{}: connection {}: {}", proto.name(), peer, e);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use overlite_engine::Result as EngineResult;
    use overlite_engine::Sqlite;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Panics on the first connection and serves the second normally, so the test
    /// can prove the first did not take the process (or the listener) down. The
    /// protocol is a concrete type, so the panic is injected one layer down, at
    /// the first thing a connection calls.
    #[derive(Debug)]
    struct PanicCluster {
        seen: AtomicUsize,
        eng: Arc<Sqlite>,
    }

    impl Cluster for PanicCluster {
        fn engine(&self, _database: &str) -> EngineResult<Arc<Sqlite>> {
            assert!(self.seen.fetch_add(1, Ordering::SeqCst) != 0, "boom");
            Ok(self.eng.clone())
        }
        fn release(&self, _database: &str) {}
        fn databases(&self) -> EngineResult<Vec<String>> {
            Ok(Vec::new())
        }
        fn create_database(&self, _database: &str) -> EngineResult<()> {
            Ok(())
        }
        fn drop_database(&self, _database: &str) -> EngineResult<()> {
            Ok(())
        }
        fn rename_database(&self, _database: &str, _new_name: &str) -> EngineResult<()> {
            Ok(())
        }
        fn close(&self) -> EngineResult<()> {
            Ok(())
        }
    }

    /// Connects and sends a v3 StartupMessage.
    fn start(addr: &str) -> std::io::Result<TcpStream> {
        let mut conn = TcpStream::connect(addr)?;
        conn.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut body = 196608i32.to_be_bytes().to_vec();
        body.extend_from_slice(b"user\0app\0database\0x\0\0");
        let mut msg = (body.len() as i32 + 4).to_be_bytes().to_vec();
        msg.extend_from_slice(&body);
        conn.write_all(&msg)?;
        Ok(conn)
    }

    // Pins the guarantee that one client cannot end the server for everyone else.
    #[test]
    fn handle_contains_panics() {
        let dir = tempfile::tempdir().unwrap();
        let eng = Sqlite::open(dir.path().join("panic.db").to_str().unwrap()).unwrap();
        let cluster = Arc::new(PanicCluster {
            seen: AtomicUsize::new(0),
            eng,
        });
        let proto = Protocol {
            password: String::new(),
            tls: None,
            configured_auth: "trust".to_string(),
            hba: None,
        };
        let mut srv = Server::new("127.0.0.1:0", proto, cluster.clone()).unwrap();
        let addr = srv.addr();

        std::thread::spawn(move || srv.serve());

        // First connection: the protocol panics while handling it.
        let mut first = start(&addr).unwrap();
        let _ = first.read(&mut [0u8; 1]); // wait for the handler to finish
        drop(first);
        assert_eq!(
            cluster.seen.load(Ordering::SeqCst),
            1,
            "the first connection never reached the panic"
        );

        // Second connection: the server is still up and serving. 'R' opens the
        // authentication exchange.
        let mut second = start(&addr).expect("the server died with the first connection");
        let mut buf = [0u8; 1];
        second
            .read_exact(&mut buf)
            .expect("read after the panicking connection");
        assert_eq!(buf[0], b'R');
        assert_eq!(cluster.seen.load(Ordering::SeqCst), 2);
    }
}
