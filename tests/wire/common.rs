//! The harness of the wire suite: a client, servers to run it against, and
//! assertions.
//!
//! `Conn` is a small Postgres client written to behave as pgx v5 does, down to the
//! messages it sends: `connect` is pgx's simple mode, `connect_extended` its default
//! mode (statement cache, Parse/Describe/Sync then Bind/Execute/Sync, a format code
//! per column by OID). `RawConn` is the wire layer below it, open to tests that build
//! messages by hand. The `postgres` crate cannot stand in for `Conn`: its `prepare`
//! fails on any parameter the server describes as OID 0, its results are always
//! binary, and its simple queries hide OIDs. It does the `dial*` probes, which only
//! ask whether a login works, so authentication is also checked by a client that
//! shares no code with this file beyond the SCRAM and MD5 primitives.

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use postgres_protocol::authentication::md5_hash;
use postgres_protocol::authentication::sasl::{ChannelBinding, SCRAM_SHA_256, ScramSha256};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[allow(unused_imports)] // traits for the test modules that glob-import this one
pub use chrono::{Datelike, Timelike};

// ---------------------------------------------------------------------------
// Server processes
// ---------------------------------------------------------------------------

/// Read timeout of every connection unless a test sets its own: a hung test should
/// fail rather than wedge the run.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// The environment a test server is started with. It belongs to the child process,
/// so tests stay parallel.
#[derive(Default)]
pub struct Env {
    vars: Vec<(String, String)>,
    /// Flags put before the harness's own, as an image's ENTRYPOINT puts its own
    /// before the ones a compose file adds.
    args: Vec<String>,
    dirs: Vec<TempDir>,
}

impl Env {
    pub fn new() -> Env {
        Env::default()
    }

    pub fn setenv(&mut self, key: &str, value: &str) {
        self.vars.push((key.to_string(), value.to_string()));
    }

    pub fn with(mut self, key: &str, value: &str) -> Env {
        self.setenv(key, value);
        self
    }

    /// Flags the server is given before the harness's own `--host`, `--port` and `--db`.
    pub fn with_args(mut self, args: &[&str]) -> Env {
        self.args.extend(args.iter().map(|a| (*a).to_string()));
        self
    }

    pub fn password(pw: &str) -> Env {
        Env::new().with("POSTGRES_PASSWORD", pw)
    }
}

/// A running server, killed when the last handle drops. Connections hold a handle,
/// so `connect(&start_server())` keeps it alive.
#[derive(Clone)]
pub struct Server(Arc<ServerProc>);

struct ServerProc {
    addr: String,
    port: u16,
    child: Mutex<Option<Child>>,
    _dirs: Vec<TempDir>,
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        if let Some(mut child) = lock(&self.child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Server {
    pub fn addr(&self) -> &str {
        &self.0.addr
    }

    pub fn host(&self) -> &'static str {
        "127.0.0.1"
    }

    pub fn port(&self) -> u16 {
        self.0.port
    }

    /// The server's process id, which only Linux's tests read.
    #[cfg(target_os = "linux")]
    pub fn pid(&self) -> u32 {
        lock(&self.0.child).as_ref().expect("server is running").id()
    }

    /// The directory holding the database, when the harness made it.
    pub fn dir(&self) -> &Path {
        self.0._dirs.first().expect("server has no directory of its own").path()
    }
}

impl fmt::Display for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.addr())
    }
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.addr())
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A temporary directory, removed when the value drops.
pub fn temp_dir() -> TempDir {
    TempDir::new().expect("temp dir")
}

/// A port no other test of this process was given. It comes from a UDP socket: a
/// TCP listener would stay open in any child forked by another test before its exec,
/// long enough to answer `listening` and then reset the connection. `start` retries
/// when the port turns out to be taken for TCP.
fn free_port() -> u16 {
    static USED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
    loop {
        let port = UdpSocket::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("local addr")
            .port();
        let mut used = lock(&USED);
        if !used.contains(&port) {
            used.push(port);
            return port;
        }
    }
}

fn spawn(db: &Path, port: u16, max_open: usize, env: &Env) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_overlite"));
    cmd.args(&env.args);
    cmd.args(["--host", "127.0.0.1", "--port", &port.to_string(), "--db"])
        .arg(db);
    if max_open > 0 {
        cmd.args(["--max-open-databases", &max_open.to_string()]);
    }
    for key in [
        "POSTGRES_PASSWORD",
        "POSTGRES_USER",
        "POSTGRES_HOST_AUTH_METHOD",
        "POSTGRES_SSL",
        "OVERLITE_HBA_DIR",
    ] {
        cmd.env_remove(key);
    }
    cmd.envs(env.vars.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if std::env::var_os("OVERLITE_RS_LOG").is_some() {
        cmd.stderr(Stdio::inherit());
    } else {
        cmd.stderr(Stdio::null());
    }
    cmd.spawn().expect("spawn the server")
}

fn listening(addr: &str) -> bool {
    let sa = addr.parse().expect("socket address");
    // A probe of a free port in the ephemeral range can connect to itself.
    TcpStream::connect_timeout(&sa, Duration::from_millis(100)).is_ok_and(|c| c.local_addr().ok() != c.peer_addr().ok())
}

/// A server on `db`, which the caller owns.
pub fn start_server_on(db: impl AsRef<Path>, max_open: usize) -> Server {
    start_server_on_env(db, max_open, Env::new())
}

pub fn start_server_on_env(db: impl AsRef<Path>, max_open: usize, env: Env) -> Server {
    start(db.as_ref(), max_open, env, Vec::new())
}

fn start(db: &Path, max_open: usize, mut env: Env, mut dirs: Vec<TempDir>) -> Server {
    dirs.append(&mut env.dirs);
    for _ in 0..5 {
        let port = free_port();
        let addr = format!("127.0.0.1:{port}");
        let mut child = spawn(db, port, max_open, &env);
        for _ in 0..500 {
            if listening(&addr) {
                let proc = ServerProc {
                    addr,
                    port,
                    child: Mutex::new(Some(child)),
                    _dirs: dirs,
                };
                return Server(Arc::new(proc));
            }
            if child.try_wait().expect("wait").is_some() {
                break; // lost the port to something else: try another
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    panic!("server did not start on any of 5 ports");
}

/// A fresh server on a temp dir's test.db.
pub fn start_server() -> Server {
    start_server_env(Env::new())
}

/// A server on a fresh database, with this environment.
pub fn start_server_env(env: Env) -> Server {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    start(&db, 0, env, vec![dir])
}

/// The database a cluster test server is started with.
pub const ENTRY_DATABASE: &str = "postgres";

/// A directory of databases; `server.dir()` is the directory.
pub fn start_cluster(max_open: usize) -> Server {
    start_cluster_env(max_open, Env::new())
}

pub fn start_cluster_env(max_open: usize, env: Env) -> Server {
    let dir = temp_dir();
    let db = dir.path().join(format!("{ENTRY_DATABASE}.db"));
    start(&db, max_open, env, vec![dir])
}

/// A server the test can signal, on a database the test owns. Every server here can
/// be signalled; the name says what the test means by it.
pub fn start_proc(db: impl AsRef<Path>) -> Server {
    start_server_on(db, 0)
}

/// Delivers `sig`, returns the exit (Ok for status 0) and the time it took.
pub fn signal_and_wait(server: &Server, sig: i32) -> (std::result::Result<(), String>, Duration) {
    let mut guard = lock(&server.0.child);
    let child = guard.as_mut().expect("the server already exited");
    let t0 = Instant::now();
    // SAFETY: kill takes any pid and signal, and reports a bad one by its result.
    assert_eq!(unsafe { libc::kill(child.id() as i32, sig) }, 0, "kill");
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            let took = t0.elapsed();
            guard.take();
            return (
                if status.success() {
                    Ok(())
                } else {
                    Err(status.to_string())
                },
                took,
            );
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "server still running 20s after signal {sig}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Opens the files with no server and reports what survived.
pub fn offline_check(db: impl AsRef<Path>, query: &str) -> (String, i64) {
    let conn = rusqlite::Connection::open(db).expect("open the database");
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity_check");
    let n: i64 = conn.query_row(query, [], |r| r.get(0)).expect(query);
    (integrity, n)
}

/// The size of the WAL, -1 when there is none.
pub fn wal_size(db: impl AsRef<Path>) -> i64 {
    let mut wal = db.as_ref().as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(wal).map_or(-1, |m| m.len() as i64)
}

/// A pg_hba file in a directory of its own, named by the server's environment.
pub fn write_hba(env: &mut Env, name: &str, body: &str) {
    let dir = temp_dir();
    std::fs::write(dir.path().join(name), body).expect("write pg_hba");
    env.setenv("OVERLITE_HBA_DIR", dir.path().to_str().expect("utf-8 path"));
    env.dirs.push(dir);
}

/// The program on PATH, if any.
pub fn look_path(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| p.is_file())
}

/// Skips the test: it ends and counts as passed.
#[macro_export]
macro_rules! skip_test {
    ($($arg:tt)+) => {{
        println!("SKIP: {}", format!($($arg)+));
        return;
    }};
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An ErrorResponse from the server, as pgx reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PgError {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub detail: String,
    pub hint: String,
    pub position: String,
    pub where_: String,
    pub schema: String,
    pub table: String,
    pub column: String,
    pub constraint: String,
    /// The message as sent, for one that is not UTF-8.
    pub message_bytes: Vec<u8>,
}

impl fmt::Display for PgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} (SQLSTATE {})", self.severity, self.message, self.code)
    }
}

#[derive(Debug, Clone)]
pub enum WireError {
    /// An ErrorResponse.
    Pg(Box<PgError>),
    /// pgx.ErrNoRows.
    NoRows,
    /// The read deadline passed.
    Timeout(String),
    /// The socket failed or closed.
    Io(String),
    /// A scan or encoding failure in the client, or anything else that is not the server's.
    Client(String),
}

pub type Result<T> = std::result::Result<T, WireError>;

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Pg(e) => e.fmt(f),
            WireError::NoRows => f.write_str("no rows in result set"),
            WireError::Timeout(s) | WireError::Io(s) | WireError::Client(s) => f.write_str(s),
        }
    }
}

impl WireError {
    /// The server's error, when this is one.
    pub fn pg(&self) -> Option<&PgError> {
        match self {
            WireError::Pg(e) => Some(e),
            _ => None,
        }
    }

    /// The SQLSTATE, empty when this is not a server error.
    pub fn code(&self) -> &str {
        self.pg().map_or("", |e| &e.code)
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, WireError::Timeout(_))
    }

    pub fn is_no_rows(&self) -> bool {
        matches!(self, WireError::NoRows)
    }
}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> WireError {
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => WireError::Timeout(e.to_string()),
            _ => WireError::Io(e.to_string()),
        }
    }
}

fn client_err(msg: impl Into<String>) -> WireError {
    WireError::Client(msg.into())
}

/// The PgError inside `err`, failing the test if it is something else.
#[track_caller]
pub fn pg_error(err: &WireError) -> &PgError {
    err.pg().unwrap_or_else(|| panic!("want a PgError, got {err:?}"))
}

#[track_caller]
pub fn assert_sqlstate(err: &WireError, code: &str) {
    assert_eq!(pg_error(err).code, code, "SQLSTATE of {err}");
}

/// The SQLSTATE and a piece of the message of a PgError.
#[track_caller]
pub fn assert_pg_error(err: &WireError, code: &str, message: &str) {
    assert_sqlstate(err, code);
    assert!(
        pg_error(err).message.contains(message),
        "{err} does not mention {message:?}"
    );
}

// ---------------------------------------------------------------------------
// RawConn: the wire layer
// ---------------------------------------------------------------------------

/// One field of a RowDescription.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDesc {
    pub name: String,
    /// The name as sent, for one that is not UTF-8.
    pub name_bytes: Vec<u8>,
    pub table_oid: u32,
    pub table_attribute_number: u16,
    pub data_type_oid: u32,
    pub data_type_size: i16,
    pub type_modifier: i32,
    pub format: i16,
}

/// A backend message.
#[derive(Debug, Clone, PartialEq)]
pub enum Backend {
    /// Authentication request: the code and what follows it.
    Authentication(i32, Vec<u8>),
    ParameterStatus(String, String),
    KeyData {
        process_id: u32,
        secret_key: Vec<u8>,
    },
    ReadyForQuery(u8),
    RowDescription(Vec<FieldDesc>),
    DataRow(Vec<Option<Vec<u8>>>),
    CommandComplete(String),
    EmptyQueryResponse,
    ErrorResponse(PgError),
    NoticeResponse(PgError),
    ParseComplete,
    BindComplete,
    CloseComplete,
    NoData,
    PortalSuspended,
    ParameterDescription(Vec<u32>),
    /// Overall format and per-column formats.
    CopyInResponse(u8, Vec<i16>),
    CopyOutResponse(u8, Vec<i16>),
    CopyData(Vec<u8>),
    CopyDone,
    NotificationResponse(Notification),
    /// Anything else: the type byte and the body.
    Other(u8, Vec<u8>),
}

/// A NotificationResponse.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub pid: u32,
    pub channel: String,
    pub payload: String,
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// A hand-driven frontend over a socket. `send_*` queues a message
/// and `flush` writes the queue, so pipelining is a matter of not reading in between.
pub struct RawConn {
    stream: Stream,
    wbuf: Vec<u8>,
    rbuf: Vec<u8>,
    rpos: usize,
    /// ParameterStatus values seen by `receive`.
    pub params: HashMap<String, String>,
    /// From KeyData.
    pub process_id: u32,
    pub secret_key: Vec<u8>,
}

impl RawConn {
    /// A bare TCP connection: not even a StartupMessage.
    pub fn dial(addr: &Server) -> Result<RawConn> {
        RawConn::dial_addr(addr.addr())
    }

    pub fn dial_addr(addr: &str) -> Result<RawConn> {
        let sa = addr.parse().map_err(|e| client_err(format!("{addr}: {e}")))?;
        let tcp = TcpStream::connect_timeout(&sa, Duration::from_secs(5))?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
        Ok(RawConn {
            stream: Stream::Plain(tcp),
            wbuf: Vec::new(),
            rbuf: Vec::new(),
            rpos: 0,
            params: HashMap::new(),
            process_id: 0,
            secret_key: Vec::new(),
        })
    }

    /// SetReadDeadline, as a duration. None waits forever.
    pub fn set_read_timeout(&mut self, d: Option<Duration>) -> Result<()> {
        Ok(self.stream.tcp().set_read_timeout(d)?)
    }

    pub fn is_tls(&self) -> bool {
        matches!(self.stream, Stream::Tls(_))
    }

    /// Closes both directions now, without a Terminate.
    pub fn shutdown(&self) {
        let _ = self.stream.tcp().shutdown(std::net::Shutdown::Both);
    }

    fn msg(&mut self, kind: u8, body: &[u8]) {
        self.wbuf.push(kind);
        self.wbuf.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
        self.wbuf.extend_from_slice(body);
    }

    fn untyped(&mut self, body: &[u8]) {
        self.wbuf.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
        self.wbuf.extend_from_slice(body);
    }

    pub fn send_startup(&mut self, params: &[(&str, &str)]) {
        let mut b = 196608u32.to_be_bytes().to_vec();
        for (k, v) in params {
            b.extend_from_slice(k.as_bytes());
            b.push(0);
            b.extend_from_slice(v.as_bytes());
            b.push(0);
        }
        b.push(0);
        self.untyped(&b);
    }

    /// Queues bytes as they are, for what a well-behaved client would not send.
    pub fn send_bytes(&mut self, bytes: &[u8]) {
        self.wbuf.extend_from_slice(bytes);
    }

    pub fn send_ssl_request(&mut self) {
        self.untyped(&80877103u32.to_be_bytes());
    }

    pub fn send_cancel_request(&mut self, process_id: u32, secret_key: &[u8]) {
        let mut b = 80877102u32.to_be_bytes().to_vec();
        b.extend_from_slice(&process_id.to_be_bytes());
        b.extend_from_slice(secret_key);
        self.untyped(&b);
    }

    pub fn send_query(&mut self, sql: impl AsRef<[u8]>) {
        let mut b = sql.as_ref().to_vec();
        b.push(0);
        self.msg(b'Q', &b);
    }

    pub fn send_parse(&mut self, name: &str, sql: impl AsRef<[u8]>, param_oids: &[u32]) {
        let mut b = name.as_bytes().to_vec();
        b.push(0);
        b.extend_from_slice(sql.as_ref());
        b.push(0);
        b.extend_from_slice(&(param_oids.len() as u16).to_be_bytes());
        for oid in param_oids {
            b.extend_from_slice(&oid.to_be_bytes());
        }
        self.msg(b'P', &b);
    }

    pub fn send_bind(
        &mut self,
        portal: &str,
        statement: &str,
        param_formats: &[i16],
        params: &[Option<&[u8]>],
        result_formats: &[i16],
    ) {
        let mut b = portal.as_bytes().to_vec();
        b.push(0);
        b.extend_from_slice(statement.as_bytes());
        b.push(0);
        b.extend_from_slice(&(param_formats.len() as u16).to_be_bytes());
        for f in param_formats {
            b.extend_from_slice(&f.to_be_bytes());
        }
        b.extend_from_slice(&(params.len() as u16).to_be_bytes());
        for p in params {
            match p {
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(v) => {
                    b.extend_from_slice(&(v.len() as i32).to_be_bytes());
                    b.extend_from_slice(v);
                }
            }
        }
        b.extend_from_slice(&(result_formats.len() as u16).to_be_bytes());
        for f in result_formats {
            b.extend_from_slice(&f.to_be_bytes());
        }
        self.msg(b'B', &b);
    }

    /// `kind` is b'S' for a statement, b'P' for a portal.
    pub fn send_describe(&mut self, kind: u8, name: &str) {
        let mut b = vec![kind];
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        self.msg(b'D', &b);
    }

    pub fn send_execute(&mut self, portal: &str, max_rows: u32) {
        let mut b = portal.as_bytes().to_vec();
        b.push(0);
        b.extend_from_slice(&max_rows.to_be_bytes());
        self.msg(b'E', &b);
    }

    pub fn send_sync(&mut self) {
        self.msg(b'S', &[]);
    }

    /// `kind` is b'S' for a statement, b'P' for a portal.
    pub fn send_close(&mut self, kind: u8, name: &str) {
        let mut b = vec![kind];
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        self.msg(b'C', &b);
    }

    pub fn send_password(&mut self, body: &[u8]) {
        self.msg(b'p', body);
    }

    pub fn send_copy_data(&mut self, data: &[u8]) {
        self.msg(b'd', data);
    }

    pub fn send_copy_done(&mut self) {
        self.msg(b'c', &[]);
    }

    pub fn send_copy_fail(&mut self, message: &str) {
        let mut b = message.as_bytes().to_vec();
        b.push(0);
        self.msg(b'f', &b);
    }

    pub fn send_terminate(&mut self) {
        self.msg(b'X', &[]);
    }

    /// Writes what is queued.
    pub fn flush(&mut self) -> Result<()> {
        let buf = std::mem::take(&mut self.wbuf);
        self.stream.write_all(&buf)?;
        self.stream.flush()?;
        Ok(())
    }

    fn fill(&mut self, n: usize) -> Result<()> {
        while self.rbuf.len() - self.rpos < n {
            if self.rpos > 0 {
                self.rbuf.drain(..self.rpos);
                self.rpos = 0;
            }
            let mut chunk = [0u8; 65536];
            let got = self.stream.read(&mut chunk)?;
            if got == 0 {
                return Err(WireError::Io("unexpected EOF".into()));
            }
            self.rbuf.extend_from_slice(&chunk[..got]);
        }
        Ok(())
    }

    /// Reads one byte outside message framing: the answer to an SSLRequest.
    pub fn read_byte(&mut self) -> Result<u8> {
        self.fill(1)?;
        self.rpos += 1;
        Ok(self.rbuf[self.rpos - 1])
    }

    /// The next message, of any kind.
    pub fn receive(&mut self) -> Result<Backend> {
        self.fill(5)?;
        let kind = self.rbuf[self.rpos];
        let len = u32::from_be_bytes(self.rbuf[self.rpos + 1..self.rpos + 5].try_into().unwrap()) as usize;
        if len < 4 {
            return Err(client_err(format!("bad message length {len}")));
        }
        self.fill(5 + len - 4)?;
        let body = self.rbuf[self.rpos + 5..self.rpos + 1 + len].to_vec();
        self.rpos += 1 + len;
        let msg = parse_backend(kind, body)?;
        match &msg {
            Backend::ParameterStatus(k, v) => {
                self.params.insert(k.clone(), v.clone());
            }
            Backend::KeyData { process_id, secret_key } => {
                self.process_id = *process_id;
                self.secret_key = secret_key.clone();
            }
            _ => {}
        }
        Ok(msg)
    }

    /// Reads until `want` says yes, returning that message.
    pub fn wait_for(&mut self, want: impl Fn(&Backend) -> bool) -> Result<Backend> {
        loop {
            let msg = self.receive()?;
            if want(&msg) {
                return Ok(msg);
            }
        }
    }

    /// Reads up to ReadyForQuery.
    pub fn wait_ready(&mut self) -> Result<Backend> {
        self.wait_for(|m| matches!(m, Backend::ReadyForQuery(_)))
    }

    /// Sends a StartupMessage and authenticates, up to ReadyForQuery.
    pub fn startup(&mut self, user: &str, database: &str, password: &str) -> Result<()> {
        self.send_startup(&[("database", database), ("user", user)]);
        self.flush()?;
        let mut scram: Option<ScramSha256> = None;
        loop {
            match self.receive()? {
                Backend::Authentication(0, _) => {}
                Backend::Authentication(3, _) => {
                    let mut b = password.as_bytes().to_vec();
                    b.push(0);
                    self.send_password(&b);
                    self.flush()?;
                }
                Backend::Authentication(5, salt) => {
                    let salt: [u8; 4] = salt[..4].try_into().unwrap();
                    let mut b = md5_hash(user.as_bytes(), password.as_bytes(), salt).into_bytes();
                    b.push(0);
                    self.send_password(&b);
                    self.flush()?;
                }
                Backend::Authentication(10, mechanisms) => {
                    let offered = mechanisms.split(|c| *c == 0).any(|m| m == SCRAM_SHA_256.as_bytes());
                    if !offered {
                        return Err(client_err("the server offers no SCRAM-SHA-256"));
                    }
                    let s = ScramSha256::new(password.as_bytes(), ChannelBinding::unsupported());
                    let mut b = SCRAM_SHA_256.as_bytes().to_vec();
                    b.push(0);
                    b.extend_from_slice(&(s.message().len() as i32).to_be_bytes());
                    b.extend_from_slice(s.message());
                    self.send_password(&b);
                    self.flush()?;
                    scram = Some(s);
                }
                Backend::Authentication(11, data) => {
                    let s = scram
                        .as_mut()
                        .ok_or_else(|| client_err("SASL continue without a start"))?;
                    s.update(&data).map_err(|e| client_err(e.to_string()))?;
                    let m = s.message().to_vec();
                    self.send_password(&m);
                    self.flush()?;
                }
                Backend::Authentication(12, data) => {
                    let s = scram.as_mut().ok_or_else(|| client_err("SASL final without a start"))?;
                    s.finish(&data).map_err(|e| client_err(e.to_string()))?;
                }
                Backend::Authentication(code, _) => {
                    return Err(client_err(format!("unsupported authentication {code}")));
                }
                Backend::ErrorResponse(e) => return Err(WireError::Pg(Box::new(e))),
                Backend::ReadyForQuery(_) => return Ok(()),
                _ => {}
            }
        }
    }

    /// The SSLRequest handshake, then TLS with no certificate check: sslmode=require.
    pub fn start_tls(self) -> Result<RawConn> {
        let mut raw = self;
        raw.send_ssl_request();
        raw.flush()?;
        let answer = raw.read_byte()?;
        if answer != b'S' {
            return Err(client_err(format!("server refused TLS: {:?}", answer as char)));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| client_err(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
        let tls = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| client_err(e.to_string()))?;
        let Stream::Plain(tcp) = raw.stream else {
            return Err(client_err("already TLS"));
        };
        raw.stream = Stream::Tls(Box::new(rustls::StreamOwned::new(tls, tcp)));
        Ok(raw)
    }
}

#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(client_err("short message"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn cstr(&mut self) -> Result<&'a [u8]> {
        let end = self
            .0
            .iter()
            .position(|c| *c == 0)
            .ok_or_else(|| client_err("unterminated string"))?;
        let s = self.take(end)?;
        self.take(1)?;
        Ok(s)
    }

    fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.cstr()?).into_owned())
    }

    fn formats(&mut self) -> Result<(u8, Vec<i16>)> {
        let overall = self.u8()?;
        let n = self.i16()?;
        let cols = (0..n).map(|_| self.i16()).collect::<Result<_>>()?;
        Ok((overall, cols))
    }
}

fn parse_fields(r: &mut Reader<'_>) -> Result<PgError> {
    let mut e = PgError::default();
    loop {
        let code = r.u8()?;
        if code == 0 {
            return Ok(e);
        }
        let raw = r.cstr()?;
        let value = String::from_utf8_lossy(raw).into_owned();
        match code {
            b'S' => e.severity = value,
            b'C' => e.code = value,
            b'M' => {
                e.message = value;
                e.message_bytes = raw.to_vec();
            }
            b'D' => e.detail = value,
            b'H' => e.hint = value,
            b'P' => e.position = value,
            b'W' => e.where_ = value,
            b's' => e.schema = value,
            b't' => e.table = value,
            b'c' => e.column = value,
            b'n' => e.constraint = value,
            _ => {}
        }
    }
}

fn parse_backend(kind: u8, body: Vec<u8>) -> Result<Backend> {
    let mut r = Reader(&body);
    Ok(match kind {
        b'R' => {
            let code = r.i32()?;
            Backend::Authentication(code, r.0.to_vec())
        }
        b'S' => {
            let k = r.string()?;
            Backend::ParameterStatus(k, r.string()?)
        }
        b'K' => {
            let process_id = r.u32()?;
            Backend::KeyData {
                process_id,
                secret_key: r.0.to_vec(),
            }
        }
        b'Z' => Backend::ReadyForQuery(r.u8()?),
        b'T' => {
            let n = r.i16()?;
            let mut fields = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let name_bytes = r.cstr()?.to_vec();
                fields.push(FieldDesc {
                    name: String::from_utf8_lossy(&name_bytes).into_owned(),
                    name_bytes,
                    table_oid: r.u32()?,
                    table_attribute_number: r.i16()? as u16,
                    data_type_oid: r.u32()?,
                    data_type_size: r.i16()?,
                    type_modifier: r.i32()?,
                    format: r.i16()?,
                });
            }
            Backend::RowDescription(fields)
        }
        b'D' => {
            let n = r.i16()?;
            let mut row = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let len = r.i32()?;
                row.push(if len < 0 {
                    None
                } else {
                    Some(r.take(len as usize)?.to_vec())
                });
            }
            Backend::DataRow(row)
        }
        b'C' => Backend::CommandComplete(r.string()?),
        b'I' => Backend::EmptyQueryResponse,
        b'E' => Backend::ErrorResponse(parse_fields(&mut r)?),
        b'N' => Backend::NoticeResponse(parse_fields(&mut r)?),
        b'1' => Backend::ParseComplete,
        b'2' => Backend::BindComplete,
        b'3' => Backend::CloseComplete,
        b'n' => Backend::NoData,
        b's' => Backend::PortalSuspended,
        b't' => {
            let n = r.i16()? as u16;
            Backend::ParameterDescription((0..n).map(|_| r.u32()).collect::<Result<_>>()?)
        }
        b'G' => {
            let (o, c) = r.formats()?;
            Backend::CopyInResponse(o, c)
        }
        b'H' => {
            let (o, c) = r.formats()?;
            Backend::CopyOutResponse(o, c)
        }
        b'd' => Backend::CopyData(body),
        b'c' => Backend::CopyDone,
        b'A' => {
            let pid = r.u32()?;
            let channel = r.string()?;
            Backend::NotificationResponse(Notification {
                pid,
                channel,
                payload: r.string()?,
            })
        }
        other => Backend::Other(other, body),
    })
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

pub const TEXT_FORMAT: i16 = 0;
pub const BINARY_FORMAT: i16 = 1;

/// One value of a result, whatever its type: its OID, its format and its bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub oid: u32,
    pub format: i16,
    pub bytes: Option<Vec<u8>>,
}

impl Cell {
    pub fn is_null(&self) -> bool {
        self.bytes.is_none()
    }

    pub fn raw(&self) -> &[u8] {
        self.bytes.as_deref().unwrap_or(&[])
    }

    /// The text of the value in either format, as far as a binary one can be read.
    pub fn text(&self) -> String {
        match self.format {
            TEXT_FORMAT => String::from_utf8_lossy(self.raw()).into_owned(),
            _ => binary_to_text(self).unwrap_or_else(|_| String::from_utf8_lossy(self.raw()).into_owned()),
        }
    }

    /// The cell as a string: the text, when pgx decodes this type to a string, which
    /// it also does for a type it does not know, in text.
    pub fn text_value(&self) -> Option<String> {
        self.bytes.as_ref()?;
        match pgx_type(self.oid) {
            Some(t) if t.1 == "TextCodec" || t.1 == "TextFormatOnlyCodec" => Some(self.text()),
            Some(_) => None,
            None if self.format == TEXT_FORMAT => Some(self.text()),
            None => None,
        }
    }

    fn fail(&self, into: &str) -> WireError {
        let format = if self.format == TEXT_FORMAT { "text" } else { "binary" };
        let name = pgx_type(self.oid).map_or("unknown type", |t| t.0);
        client_err(format!(
            "cannot scan {name} (OID {}) in {format} format into {into}",
            self.oid
        ))
    }

    fn present(&self, into: &str) -> Result<&[u8]> {
        self.bytes
            .as_deref()
            .ok_or_else(|| client_err(format!("cannot scan NULL into {into}")))
    }

    fn be<const N: usize>(&self, into: &str) -> Result<[u8; N]> {
        self.present(into)?
            .try_into()
            .map_err(|_| client_err(format!("invalid length for {into}: {:?}", self.raw())))
    }

    fn utf8(&self, into: &str) -> Result<&str> {
        std::str::from_utf8(self.present(into)?).map_err(|e| client_err(format!("{into}: {e}")))
    }
}

/// A destination of Scan: what pgx's scan plans accept for that type, no more. An
/// int cannot be scanned from a text column here either, so a test that reads a
/// column as a number still checks the OID the server announced.
pub trait FromCell: Sized {
    fn from_cell(cell: &Cell) -> Result<Self>;
}

impl FromCell for Cell {
    fn from_cell(cell: &Cell) -> Result<Cell> {
        Ok(cell.clone())
    }
}

impl<T: FromCell> FromCell for Option<T> {
    fn from_cell(cell: &Cell) -> Result<Option<T>> {
        if cell.is_null() {
            return Ok(None);
        }
        T::from_cell(cell).map(Some)
    }
}

/// Scan into an integer. `oid_ok` is true for a *uint32, the one integer the oid
/// family scans into.
fn scan_int(cell: &Cell, into: &str, oid_ok: bool) -> Result<i128> {
    let parse = |s: &str| {
        s.parse::<i64>()
            .map(i128::from)
            .map_err(|e| client_err(format!("{into}: {s:?}: {e}")))
    };
    let codec = pgx_type(cell.oid).map(|t| t.1);
    match (codec, cell.format) {
        (Some("Int2Codec" | "Int4Codec" | "Int8Codec" | "Float4Codec" | "Float8Codec" | "NumericCodec") | None, 0) => {
            parse(cell.utf8(into)?)
        }
        (Some("Int2Codec"), _) => Ok(i16::from_be_bytes(cell.be(into)?).into()),
        (Some("Int4Codec"), _) => Ok(i32::from_be_bytes(cell.be(into)?).into()),
        (Some("Int8Codec") | None, _) => Ok(i64::from_be_bytes(cell.be(into)?).into()),
        (Some("Float4Codec"), _) => whole(f32::from_be_bytes(cell.be(into)?).into()),
        (Some("Float8Codec"), _) => whole(f64::from_be_bytes(cell.be(into)?)),
        (Some("NumericCodec"), _) => parse(&numeric_binary_to_text(cell.present(into)?)?),
        (Some("Uint32Codec"), 0) if oid_ok => parse(cell.utf8(into)?),
        (Some("Uint32Codec"), _) if oid_ok => Ok(u32::from_be_bytes(cell.be(into)?).into()),
        _ => Err(cell.fail(into)),
    }
}

fn whole(f: f64) -> Result<i128> {
    let i = f as i64;
    if f != i as f64 {
        return Err(client_err(format!("cannot losslessly convert {f} to int64")));
    }
    Ok(i.into())
}

macro_rules! int_from_cell {
    ($($t:ty => $oid_ok:expr),*) => {$(
        impl FromCell for $t {
            fn from_cell(cell: &Cell) -> Result<$t> {
                let into = concat!("*", stringify!($t));
                let v = scan_int(cell, into, $oid_ok)?;
                <$t>::try_from(v).map_err(|_| client_err(format!("{v} is out of range for {into}")))
            }
        }
    )*};
}

int_from_cell!(i16 => false, i32 => false, i64 => false, u32 => true, u64 => false);

impl FromCell for f64 {
    fn from_cell(cell: &Cell) -> Result<f64> {
        let into = "*float64";
        let codec = pgx_type(cell.oid).map(|t| t.1);
        match (codec, cell.format) {
            (
                Some("Float4Codec" | "Float8Codec" | "NumericCodec" | "Int2Codec" | "Int4Codec" | "Int8Codec") | None,
                0,
            ) => {
                let s = cell.utf8(into)?;
                s.parse::<f64>().map_err(|e| client_err(format!("{into}: {s:?}: {e}")))
            }
            (Some("Float4Codec"), _) => Ok(f32::from_be_bytes(cell.be(into)?).into()),
            (Some("Float8Codec") | None, _) => Ok(f64::from_be_bytes(cell.be(into)?)),
            (Some("NumericCodec"), _) => numeric_binary_to_text(cell.present(into)?)?
                .parse::<f64>()
                .map_err(|e| client_err(format!("{into}: {e}"))),
            (Some("Int2Codec"), _) => Ok(i16::from_be_bytes(cell.be(into)?).into()),
            (Some("Int4Codec"), _) => Ok(i32::from_be_bytes(cell.be(into)?).into()),
            (Some("Int8Codec"), _) => Ok(i64::from_be_bytes(cell.be(into)?) as f64),
            _ => Err(cell.fail(into)),
        }
    }
}

impl FromCell for bool {
    fn from_cell(cell: &Cell) -> Result<bool> {
        let into = "*bool";
        let codec = pgx_type(cell.oid).map(|t| t.1);
        match (codec, cell.format) {
            (Some("BoolCodec") | None, 0) => {
                let raw = cell.utf8(into)?;
                if raw.is_empty() {
                    return Err(client_err(format!("cannot scan empty string into {into}")));
                }
                let s = raw.trim().to_lowercase();
                if "true".starts_with(&s) || "yes".starts_with(&s) || s == "on" || s == "1" {
                    Ok(true)
                } else if "false".starts_with(&s) || "no".starts_with(&s) || "off".starts_with(&s) || s == "0" {
                    Ok(false)
                } else {
                    Err(client_err(format!("unknown boolean string representation {raw:?}")))
                }
            }
            (Some("BoolCodec") | None, _) => Ok(cell.be::<1>(into)?[0] != 0),
            _ => Err(cell.fail(into)),
        }
    }
}

impl FromCell for String {
    fn from_cell(cell: &Cell) -> Result<String> {
        let into = "*string";
        if cell.format == TEXT_FORMAT {
            return cell.utf8(into).map(str::to_string);
        }
        cell.present(into)?;
        binary_to_text(cell)
    }
}

impl FromCell for Vec<u8> {
    /// NULL reads as empty: use Option to tell them apart.
    fn from_cell(cell: &Cell) -> Result<Vec<u8>> {
        let Some(raw) = &cell.bytes else { return Ok(Vec::new()) };
        if cell.format == TEXT_FORMAT && cell.oid == 17 {
            let hex = raw
                .strip_prefix(b"\\x")
                .ok_or_else(|| client_err("invalid hex format"))?;
            return hex::decode(hex).map_err(|e| client_err(format!("invalid hex format: {e}")));
        }
        match pgx_type(cell.oid).map(|t| t.1) {
            _ if cell.format == TEXT_FORMAT => Ok(raw.clone()),
            Some("ByteaCodec" | "TextCodec") | None => Ok(raw.clone()),
            _ => Err(cell.fail("*[]uint8")),
        }
    }
}

/// A timestamp: every value keeps its instant and, for text, its offset.
pub type Time = DateTime<FixedOffset>;

/// A timestamp in UTC.
pub fn time_utc(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32, ns: u32) -> Time {
    time_fixed(y, m, d, h, mi, s, ns, 0)
}

/// A timestamp at a fixed offset from UTC.
#[allow(clippy::too_many_arguments)] // the fields of a timestamp, then the offset
pub fn time_fixed(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32, ns: u32, offset_secs: i32) -> Time {
    let date = NaiveDate::from_ymd_opt(y, m, d).expect("date");
    let naive = date.and_hms_nano_opt(h, mi, s, ns).expect("time");
    FixedOffset::east_opt(offset_secs)
        .expect("offset")
        .from_local_datetime(&naive)
        .unwrap()
}

const Y2K_MICROS: i64 = 946_684_800 * 1_000_000;

fn micros_to_time(micros: i64) -> Result<Time> {
    DateTime::from_timestamp_micros(micros + Y2K_MICROS)
        .map(|t| t.fixed_offset())
        .ok_or_else(|| client_err(format!("timestamp out of range: {micros}")))
}

fn days_to_time(days: i32) -> Result<Time> {
    let epoch = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
    let date = epoch.checked_add_signed(chrono::Duration::days(days.into()));
    date.and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| Utc.from_utc_datetime(&d).fixed_offset())
        .ok_or_else(|| client_err(format!("date out of range: {days}")))
}

const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f";

fn parse_timestamp(s: &str) -> Result<Time> {
    NaiveDateTime::parse_from_str(s, TIMESTAMP_FORMAT)
        .map(|n| Utc.from_utc_datetime(&n).fixed_offset())
        .map_err(|e| client_err(format!("timestamp {s:?}: {e}")))
}

fn parse_timestamptz(s: &str) -> Result<Time> {
    let at = |i: usize| s.len() >= i && matches!(s.as_bytes()[s.len() - i], b'+' | b'-');
    let (body, secs) = if at(9) {
        let (body, z) = s.split_at(s.len() - 9);
        let p: Vec<&str> = z[1..].split(':').collect();
        (body, tz_secs(&z[..1], p[0], p.get(1).copied(), p.get(2).copied()))
    } else if at(6) {
        let (body, z) = s.split_at(s.len() - 6);
        let p: Vec<&str> = z[1..].split(':').collect();
        (body, tz_secs(&z[..1], p[0], p.get(1).copied(), None))
    } else if at(3) {
        let (body, z) = s.split_at(s.len() - 3);
        (body, tz_secs(&z[..1], &z[1..], None, None))
    } else if let Some(body) = s.strip_suffix('Z') {
        (body, Some(0))
    } else {
        return Err(client_err(format!("timestamptz {s:?}: no offset")));
    };
    let secs = secs.ok_or_else(|| client_err(format!("timestamptz {s:?}: bad offset")))?;
    let naive = NaiveDateTime::parse_from_str(body, TIMESTAMP_FORMAT)
        .map_err(|e| client_err(format!("timestamptz {s:?}: {e}")))?;
    let zone = FixedOffset::east_opt(secs).ok_or_else(|| client_err(format!("timestamptz {s:?}: bad offset")))?;
    Ok(zone.from_local_datetime(&naive).unwrap())
}

fn tz_secs(sign: &str, h: &str, m: Option<&str>, s: Option<&str>) -> Option<i32> {
    let n = |v: Option<&str>| v.map_or(Some(0), |x| x.parse::<i32>().ok());
    let total = h.parse::<i32>().ok()? * 3600 + n(m)? * 60 + n(s)?;
    Some(if sign == "-" { -total } else { total })
}

fn parse_date(s: &str) -> Result<Time> {
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| client_err("invalid date format"))?;
    Ok(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).fixed_offset())
}

impl FromCell for Time {
    fn from_cell(cell: &Cell) -> Result<Time> {
        let into = "*time.Time";
        let codec = pgx_type(cell.oid).map(|t| t.1);
        match (codec, cell.format) {
            (Some("DateCodec"), 0) => parse_date(cell.utf8(into)?),
            (Some("DateCodec"), _) => days_to_time(i32::from_be_bytes(cell.be(into)?)),
            (Some("TimestampCodec"), 0) => parse_timestamp(cell.utf8(into)?),
            (Some("TimestamptzCodec") | None, 0) => parse_timestamptz(cell.utf8(into)?),
            (Some("TimestampCodec" | "TimestamptzCodec") | None, _) => {
                micros_to_time(i64::from_be_bytes(cell.be(into)?))
            }
            _ => Err(cell.fail(into)),
        }
    }
}

impl<T: FromCell> FromCell for Vec<T> {
    /// An array type. NULL reads as empty.
    fn from_cell(cell: &Cell) -> Result<Vec<T>> {
        let Some(raw) = &cell.bytes else { return Ok(Vec::new()) };
        let elem = match pgx_type(cell.oid) {
            Some((_, "ArrayCodec", e)) => *e,
            Some(_) => return Err(cell.fail("a slice")),
            None => 25, // pgx takes the type from the destination
        };
        let cells = if cell.format == TEXT_FORMAT {
            let s = cell.utf8("a slice")?;
            parse_text_array(s)?
                .into_iter()
                .map(|v| Cell {
                    oid: elem,
                    format: TEXT_FORMAT,
                    bytes: v.map(String::into_bytes),
                })
                .collect()
        } else {
            parse_binary_array(raw)?
        };
        cells.iter().map(T::from_cell).collect()
    }
}

fn parse_text_array(s: &str) -> Result<Vec<Option<String>>> {
    let bad = || client_err(format!("invalid array literal {s:?}"));
    let inner = s
        .trim()
        .strip_prefix('{')
        .and_then(|r| r.strip_suffix('}'))
        .ok_or_else(bad)?;
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    while chars.peek().is_some() {
        let mut v = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next().ok_or_else(bad)? {
                    '"' => break,
                    '\\' => v.push(chars.next().ok_or_else(bad)?),
                    c => v.push(c),
                }
            }
            out.push(Some(v));
        } else {
            while let Some(c) = chars.peek().copied().filter(|c| *c != ',') {
                v.push(c);
                chars.next();
            }
            out.push(if v == "NULL" { None } else { Some(v) });
        }
        match chars.next() {
            None => break,
            Some(',') => {}
            Some(_) => return Err(bad()),
        }
    }
    Ok(out)
}

fn parse_binary_array(raw: &[u8]) -> Result<Vec<Cell>> {
    let mut r = Reader(raw);
    let ndim = r.i32()?;
    let _flags = r.i32()?;
    let elem = r.u32()?;
    if ndim == 0 {
        return Ok(Vec::new());
    }
    if ndim != 1 {
        return Err(client_err(format!("{ndim}-dimensional array")));
    }
    let n = r.i32()?;
    let _lower = r.i32()?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let len = r.i32()?;
        let bytes = if len < 0 {
            None
        } else {
            Some(r.take(len as usize)?.to_vec())
        };
        out.push(Cell {
            oid: elem,
            format: BINARY_FORMAT,
            bytes,
        });
    }
    Ok(out)
}

fn numeric_binary_to_text(raw: &[u8]) -> Result<String> {
    let mut r = Reader(raw);
    let ndigits = r.i16()? as usize;
    let weight = r.i16()? as i32;
    let sign = r.i16()? as u16;
    let dscale = r.i16()? as usize;
    match sign {
        0xC000 => return Ok("NaN".into()),
        0xD000 => return Ok("Infinity".into()),
        0xF000 => return Ok("-Infinity".into()),
        _ => {}
    }
    let digits: Vec<i16> = (0..ndigits).map(|_| r.i16()).collect::<Result<_>>()?;
    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    if weight < 0 || digits.is_empty() {
        out.push('0');
    } else {
        for w in 0..=weight {
            let d = digits.get(w as usize).copied().unwrap_or(0);
            out.push_str(&if w == 0 { d.to_string() } else { format!("{d:04}") });
        }
    }
    if dscale > 0 {
        out.push('.');
        let mut frac = String::new();
        let mut w = weight + 1;
        while frac.len() < dscale {
            let d = if w < 0 {
                0
            } else {
                digits.get(w as usize).copied().unwrap_or(0)
            };
            frac.push_str(&format!("{d:04}"));
            w += 1;
        }
        frac.truncate(dscale);
        out.push_str(&frac);
    }
    Ok(out)
}

/// The usual text of a binary value.
fn binary_to_text(cell: &Cell) -> Result<String> {
    let into = "text";
    let raw = cell.present(into)?;
    let codec = pgx_type(cell.oid).map(|t| t.1);
    Ok(match codec {
        Some("Int2Codec") => i16::from_be_bytes(cell.be(into)?).to_string(),
        Some("Int4Codec") => i32::from_be_bytes(cell.be(into)?).to_string(),
        Some("Int8Codec") => i64::from_be_bytes(cell.be(into)?).to_string(),
        Some("Uint32Codec") => u32::from_be_bytes(cell.be(into)?).to_string(),
        Some("Float4Codec") => f32::from_be_bytes(cell.be(into)?).to_string(),
        Some("Float8Codec") => f64::from_be_bytes(cell.be(into)?).to_string(),
        Some("BoolCodec") => (cell.be::<1>(into)?[0] != 0).to_string(),
        Some("NumericCodec") => numeric_binary_to_text(raw)?,
        Some("UUIDCodec") => {
            let h = hex::encode(raw);
            if h.len() != 32 {
                return Err(cell.fail("*string"));
            }
            format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
        }
        Some("ByteaCodec") => format!("\\x{}", hex::encode(raw)),
        Some("DateCodec") => days_to_time(i32::from_be_bytes(cell.be(into)?))?
            .format("%Y-%m-%d")
            .to_string(),
        Some("TimestampCodec") => micros_to_time(i64::from_be_bytes(cell.be(into)?))?
            .format("%Y-%m-%d %H:%M:%S%.f")
            .to_string(),
        Some("TimestamptzCodec") => micros_to_time(i64::from_be_bytes(cell.be(into)?))?
            .format("%Y-%m-%d %H:%M:%S%.f+00")
            .to_string(),
        _ => String::from_utf8(raw.to_vec()).map_err(|e| client_err(format!("*string: {e}")))?,
    })
}

/// A query argument. `&[&1, &"a"]` is the usual spelling at a call site.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Null,
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Bytes(Vec<u8>),
    Time(Time),
    StrArray(Vec<String>),
    IntArray(Vec<i64>),
}

pub trait ToArg {
    fn to_arg(&self) -> Arg;
}

impl<T: ToArg + ?Sized> ToArg for &T {
    fn to_arg(&self) -> Arg {
        (**self).to_arg()
    }
}

impl<T: ToArg> ToArg for Option<T> {
    fn to_arg(&self) -> Arg {
        self.as_ref().map_or(Arg::Null, ToArg::to_arg)
    }
}

impl ToArg for Arg {
    fn to_arg(&self) -> Arg {
        self.clone()
    }
}

impl ToArg for str {
    fn to_arg(&self) -> Arg {
        Arg::Str(self.to_string())
    }
}

impl ToArg for String {
    fn to_arg(&self) -> Arg {
        Arg::Str(self.clone())
    }
}

impl ToArg for bool {
    fn to_arg(&self) -> Arg {
        Arg::Bool(*self)
    }
}

impl ToArg for f64 {
    fn to_arg(&self) -> Arg {
        Arg::Float(*self)
    }
}

impl ToArg for Time {
    fn to_arg(&self) -> Arg {
        Arg::Time(*self)
    }
}

impl ToArg for [u8] {
    fn to_arg(&self) -> Arg {
        Arg::Bytes(self.to_vec())
    }
}

impl<const N: usize> ToArg for [u8; N] {
    fn to_arg(&self) -> Arg {
        Arg::Bytes(self.to_vec())
    }
}

impl ToArg for Vec<u8> {
    fn to_arg(&self) -> Arg {
        Arg::Bytes(self.clone())
    }
}

impl ToArg for [String] {
    fn to_arg(&self) -> Arg {
        Arg::StrArray(self.to_vec())
    }
}

impl ToArg for Vec<String> {
    fn to_arg(&self) -> Arg {
        Arg::StrArray(self.clone())
    }
}

impl ToArg for [&str] {
    fn to_arg(&self) -> Arg {
        Arg::StrArray(self.iter().map(std::string::ToString::to_string).collect())
    }
}

impl ToArg for Vec<i64> {
    fn to_arg(&self) -> Arg {
        Arg::IntArray(self.clone())
    }
}

macro_rules! int_to_arg {
    ($($t:ty),*) => {$(impl ToArg for $t {
        fn to_arg(&self) -> Arg {
            Arg::Int(*self as i64)
        }
    })*};
}

int_to_arg!(i16, i32, i64, u32, usize);

impl Arg {
    /// The value in text format, for this OID.
    fn text(&self, oid: u32) -> Option<Vec<u8>> {
        Some(match self {
            Arg::Null => return None,
            Arg::Str(s) => s.clone().into_bytes(),
            Arg::Int(i) => i.to_string().into_bytes(),
            Arg::Float(f) => f.to_string().into_bytes(),
            Arg::Bool(b) => vec![if *b { b't' } else { b'f' }],
            Arg::Bytes(b) if oid == 0 || oid == 17 => format!("\\x{}", hex::encode(b)).into_bytes(),
            Arg::Bytes(b) => b.clone(),
            Arg::Time(t) => timestamptz_text(t).into_bytes(),
            Arg::StrArray(v) => {
                let quoted: Vec<String> = v
                    .iter()
                    .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
                    .collect();
                format!("{{{}}}", quoted.join(",")).into_bytes()
            }
            Arg::IntArray(v) => {
                let items: Vec<String> = v.iter().map(i64::to_string).collect();
                format!("{{{}}}", items.join(",")).into_bytes()
            }
        })
    }

    /// The value in binary format, for this OID: None when this value has no binary
    /// form for that type.
    fn binary(&self, oid: u32) -> Option<Vec<u8>> {
        let codec = pgx_type(oid)?;
        Some(match (self, codec.1) {
            (Arg::Int(i), "Int2Codec") => i16::try_from(*i).ok()?.to_be_bytes().to_vec(),
            (Arg::Int(i), "Int4Codec") => i32::try_from(*i).ok()?.to_be_bytes().to_vec(),
            (Arg::Int(i), "Int8Codec") => i.to_be_bytes().to_vec(),
            (Arg::Int(i), "Uint32Codec") => u32::try_from(*i).ok()?.to_be_bytes().to_vec(),
            (Arg::Int(i), "Float4Codec") => (*i as f32).to_be_bytes().to_vec(),
            (Arg::Int(i), "Float8Codec") => (*i as f64).to_be_bytes().to_vec(),
            (Arg::Int(i), "NumericCodec") => numeric_binary(&i.to_string())?,
            (Arg::Float(f), "Float4Codec") => (*f as f32).to_be_bytes().to_vec(),
            (Arg::Float(f), "Float8Codec") => f.to_be_bytes().to_vec(),
            (Arg::Float(f), "NumericCodec") => numeric_binary(&f.to_string())?,
            (Arg::Bool(b), "BoolCodec") => vec![*b as u8],
            (Arg::Bytes(b), "ByteaCodec") => b.clone(),
            (Arg::Time(t), "TimestamptzCodec") => (t.timestamp_micros() - Y2K_MICROS).to_be_bytes().to_vec(),
            (Arg::Time(t), "TimestampCodec") => (t.naive_local().and_utc().timestamp_micros() - Y2K_MICROS)
                .to_be_bytes()
                .to_vec(),
            (Arg::Time(t), "DateCodec") => {
                let epoch = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
                ((t.date_naive() - epoch).num_days() as i32).to_be_bytes().to_vec()
            }
            (Arg::StrArray(v), "ArrayCodec") => {
                array_binary(codec.2, v.iter().map(|s| s.as_bytes().to_vec()).collect())
            }
            (Arg::IntArray(v), "ArrayCodec") => array_binary(
                codec.2,
                v.iter()
                    .map(|i| Arg::Int(*i).binary(codec.2))
                    .collect::<Option<Vec<_>>>()?,
            ),
            _ => return None,
        })
    }
}

fn array_binary(elem: u32, items: Vec<Vec<u8>>) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&i32::from(!items.is_empty()).to_be_bytes());
    b.extend_from_slice(&0i32.to_be_bytes());
    b.extend_from_slice(&elem.to_be_bytes());
    if !items.is_empty() {
        b.extend_from_slice(&(items.len() as i32).to_be_bytes());
        b.extend_from_slice(&1i32.to_be_bytes());
    }
    for item in items {
        b.extend_from_slice(&(item.len() as i32).to_be_bytes());
        b.extend_from_slice(&item);
    }
    b
}

/// Decimal text to the binary numeric form.
fn numeric_binary(text: &str) -> Option<Vec<u8>> {
    let (neg, t) = text.strip_prefix('-').map_or((false, text), |r| (true, r));
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    if !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let lead = (4 - int.len() % 4) % 4;
    let trail = (4 - frac.len() % 4) % 4;
    let all = format!("{}{int}{frac}{}", "0".repeat(lead), "0".repeat(trail));
    let mut digits: Vec<i16> = all
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap().parse().unwrap())
        .collect();
    let mut weight = ((lead + int.len()) / 4) as i16 - 1;
    while digits.first() == Some(&0) {
        digits.remove(0);
        weight -= 1;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    let mut b = Vec::new();
    for v in [
        digits.len() as i16,
        weight,
        if neg { 0x4000 } else { 0 },
        frac.len() as i16,
    ] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    for d in digits {
        b.extend_from_slice(&d.to_be_bytes());
    }
    Some(b)
}

/// "2006-01-02 15:04:05.999999999Z07:00:00" of the instant in UTC, as pgx writes it.
fn timestamptz_text(t: &Time) -> String {
    let utc = t.with_timezone(&Utc);
    let mut s = utc.format("%Y-%m-%d %H:%M:%S").to_string();
    let micros = utc.timestamp_subsec_micros();
    if micros != 0 {
        let frac = format!("{micros:06}");
        s.push('.');
        s.push_str(frac.trim_end_matches('0'));
    }
    s.push('Z');
    s
}

/// The arguments of a simple-protocol query, interpolated.
pub fn sanitize_sql(sql: &str, args: &[Arg]) -> Result<String> {
    let b = sql.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(sql.len());
    let mut used = vec![false; args.len()];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' => {
                // A doubled quote just closes and reopens.
                let end = b[i + 1..]
                    .iter()
                    .position(|x| *x == c)
                    .map_or(b.len(), |p| i + 1 + p + 1);
                out.extend_from_slice(&b[i..end]);
                i = end;
            }
            b'e' | b'E' if b.get(i + 1) == Some(&b'\'') => {
                let mut j = i + 2;
                while j < b.len() && b[j] != b'\'' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(b.len());
                out.extend_from_slice(&b[i..end]);
                i = end;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                let end = b[i..].iter().position(|x| *x == b'\n').map_or(b.len(), |p| i + p);
                out.extend_from_slice(&b[i..end]);
                i = end;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                let mut j = i + 2;
                while j < b.len() && depth > 0 {
                    if b[j..].starts_with(b"*/") {
                        depth -= 1;
                        j += 2;
                    } else if b[j..].starts_with(b"/*") {
                        depth += 1;
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                out.extend_from_slice(&b[i..j]);
                i = j;
            }
            b'$' if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                let n: usize = sql[i + 1..j].parse().map_err(|e| client_err(format!("{e}")))?;
                if n == 0 {
                    return Err(client_err("first sql argument must be > 0"));
                }
                let arg = args.get(n - 1).ok_or_else(|| client_err("insufficient arguments"))?;
                used[n - 1] = true;
                out.push(b' ');
                match arg.text(0) {
                    None => out.extend_from_slice(b"null"),
                    Some(v) => {
                        out.push(b'\'');
                        for ch in v {
                            if ch == b'\'' {
                                out.push(b'\'');
                            }
                            out.push(ch);
                        }
                        out.push(b'\'');
                    }
                }
                out.push(b' ');
                i = j;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    if let Some(unused) = used.iter().position(|u| !u) {
        return Err(client_err(format!("unused argument: {unused}")));
    }
    String::from_utf8(out).map_err(|e| client_err(e.to_string()))
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One row of a query.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub cells: Vec<Cell>,
}

impl Row {
    /// Scans the row: `let (a, b): (i64, String) = row.scan().unwrap();`.
    pub fn scan<T: FromRow>(&self) -> Result<T> {
        T::from_row(self)
    }

    /// One column.
    pub fn get<T: FromCell>(&self, col: usize) -> Result<T> {
        T::from_cell(&self.cells[col])
    }

    pub fn cell(&self, col: usize) -> &Cell {
        &self.cells[col]
    }
}

/// The destinations of a Scan: one type, or a tuple with one for each column.
pub trait FromRow: Sized {
    fn from_row(row: &Row) -> Result<Self>;
}

fn check_columns(row: &Row, want: usize) -> Result<()> {
    if row.cells.len() != want {
        return Err(client_err(format!(
            "number of field descriptions must equal number of destinations, got {} and {want}",
            row.cells.len()
        )));
    }
    Ok(())
}

impl<T: FromCell> FromRow for T {
    fn from_row(row: &Row) -> Result<T> {
        check_columns(row, 1)?;
        T::from_cell(&row.cells[0])
    }
}

macro_rules! tuple_from_row {
    ($($n:literal: ($($t:ident $i:tt),+);)*) => {$(
        impl<$($t: FromCell),+> FromRow for ($($t,)+) {
            fn from_row(row: &Row) -> Result<Self> {
                check_columns(row, $n)?;
                Ok(($($t::from_cell(&row.cells[$i])?,)+))
            }
        }
    )*};
}

tuple_from_row! {
    1: (A 0);
    2: (A 0, B 1);
    3: (A 0, B 1, C 2);
    4: (A 0, B 1, C 2, D 3);
    5: (A 0, B 1, C 2, D 3, E 4);
    6: (A 0, B 1, C 2, D 3, E 4, F 5);
    7: (A 0, B 1, C 2, D 3, E 4, F 5, G 6);
    8: (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
    9: (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
    10: (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
    11: (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
    12: (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);
}

/// A result, read to the end: an error comes out of `query`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rows {
    pub fields: Vec<FieldDesc>,
    pub rows: Vec<Row>,
    pub tag: Tag,
}

impl Rows {
    pub fn field_descriptions(&self) -> &[FieldDesc] {
        &self.fields
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Row> {
        self.rows.iter()
    }

    /// pgx.CollectRows with pgx.RowTo, or a Scan of each row in a loop.
    pub fn collect_rows<T: FromRow>(&self) -> Result<Vec<T>> {
        self.rows.iter().map(Row::scan).collect()
    }
}

impl<'a> IntoIterator for &'a Rows {
    type Item = &'a Row;
    type IntoIter = std::slice::Iter<'a, Row>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

/// What query_row returns: the error, if any, waits for `scan`.
#[derive(Debug)]
pub struct QueryRow(Result<Rows>);

impl QueryRow {
    pub fn scan<T: FromRow>(self) -> Result<T> {
        let rows = self.0?;
        rows.rows.first().ok_or(WireError::NoRows)?.scan()
    }
}

/// A CommandComplete tag.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tag(pub String);

impl Tag {
    pub fn rows_affected(&self) -> i64 {
        let digits = self.0.bytes().rev().take_while(u8::is_ascii_digit).count();
        self.0[self.0.len() - digits..].parse().unwrap_or(0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One statement's outcome, as bytes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PgResult {
    pub fields: Vec<FieldDesc>,
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
    pub tag: Tag,
}

impl PgResult {
    /// A cell, with NULL as empty.
    pub fn cell(&self, row: usize, col: usize) -> &[u8] {
        self.rows[row][col].as_deref().unwrap_or(&[])
    }

    pub fn text(&self, row: usize, col: usize) -> String {
        String::from_utf8_lossy(self.cell(row, col)).into_owned()
    }
}

/// What Describe says of a prepared statement.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementDesc {
    pub name: String,
    pub sql: String,
    pub param_oids: Vec<u32>,
    pub fields: Vec<FieldDesc>,
}

// ---------------------------------------------------------------------------
// Conn
// ---------------------------------------------------------------------------

/// Which client pgx would be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// QueryExecModeSimpleProtocol.
    Simple,
    /// QueryExecModeCacheStatement, pgx's default.
    Extended,
}

/// pgx.ParseConfig of a URL: who to log in as, and how to talk.
#[derive(Debug, Clone)]
pub struct Config {
    pub user: String,
    pub password: String,
    pub database: String,
    pub mode: Mode,
    /// sslmode=require.
    pub tls: bool,
}

impl Config {
    /// "postgres://overlite@addr/test?sslmode=disable", pgx's default mode.
    pub fn new() -> Config {
        Config {
            user: "overlite".into(),
            password: String::new(),
            database: "test".into(),
            mode: Mode::Extended,
            tls: false,
        }
    }

    pub fn user(mut self, user: &str, password: &str) -> Config {
        self.user = user.into();
        self.password = password.into();
        self
    }

    pub fn database(mut self, database: &str) -> Config {
        self.database = database.into();
        self
    }

    pub fn simple(mut self) -> Config {
        self.mode = Mode::Simple;
        self
    }

    pub fn tls(mut self) -> Config {
        self.tls = true;
        self
    }
}

impl Default for Config {
    fn default() -> Config {
        Config::new()
    }
}

struct Inner {
    raw: RawConn,
    statement_cache: HashMap<String, StatementDesc>,
    prepared: HashMap<String, StatementDesc>,
    /// Cached statements that failed: closed before the next query, as pgx does.
    invalid: Vec<String>,
    failed_describe: Option<String>,
    notifications: VecDeque<Notification>,
}

/// A pgx connection and the layer below it, in one type. Methods take `&self`, so
/// closures and threads can share a connection; each call owns it until its reply
/// ends.
pub struct Conn {
    inner: Mutex<Inner>,
    mode: Mode,
    _server: Server,
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Conn({:?} to {})", self.mode, self._server)
    }
}

/// pgx.ConnectConfig.
pub fn connect_config(addr: &Server, config: &Config) -> Result<Conn> {
    let mut raw = RawConn::dial(addr)?;
    raw.set_read_timeout(Some(Duration::from_secs(5)))?;
    if config.tls {
        raw = raw.start_tls()?;
    }
    // A read past its timeout is EAGAIN, "os error 35" on macOS and 11 on Linux,
    // which reads like a failed connect: say it is the startup that went unanswered.
    raw.startup(&config.user, &config.database, &config.password)
        .map_err(|e| match e {
            WireError::Timeout(e) => WireError::Timeout(format!("no answer to the startup within 5s: {e}")),
            e => e,
        })?;
    raw.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
    let inner = Inner {
        raw,
        statement_cache: HashMap::new(),
        prepared: HashMap::new(),
        invalid: Vec::new(),
        failed_describe: None,
        notifications: VecDeque::new(),
    };
    Ok(Conn {
        inner: Mutex::new(inner),
        mode: config.mode,
        _server: addr.clone(),
    })
}

/// A connection below `Conn`, logged in to database test.
#[track_caller]
pub fn raw_conn(addr: &Server, user: &str, pw: &str) -> RawConn {
    let mut raw = RawConn::dial(addr).expect("dial");
    raw.startup(user, "test", pw).expect("startup");
    raw
}

/// Runs sql by Parse, Bind and Execute, as a driver that prepares everything does, and
/// answers its tag. `Conn::exec` without arguments is a simple Query, in either mode.
pub fn extended_exec(raw: &mut RawConn, sql: &str) -> Result<String> {
    raw.send_parse("", sql, &[]);
    raw.send_bind("", "", &[], &[], &[]);
    raw.send_execute("", 0);
    raw.send_sync();
    raw.flush()?;
    let mut out = None;
    loop {
        match raw.receive()? {
            Backend::CommandComplete(tag) => out = Some(Ok(tag)),
            Backend::ErrorResponse(e) => out = Some(Err(WireError::Pg(Box::new(e)))),
            Backend::ReadyForQuery(_) => return out.expect("an answer before ReadyForQuery"),
            _ => {}
        }
    }
}

/// connect: user overlite, database test, simple protocol.
#[track_caller]
pub fn connect(addr: &Server) -> Conn {
    connect_config(addr, &Config::new().simple()).unwrap_or_else(|e| panic!("connect: {e}"))
}

/// pgx's default mode.
#[track_caller]
pub fn connect_extended(addr: &Server) -> Conn {
    connect_config(addr, &Config::new()).unwrap_or_else(|e| panic!("connect: {e}"))
}

/// pgx's default mode, as `pgx.Connect` of a URL is.
#[track_caller]
pub fn connect_as(addr: &Server, user: &str, pw: &str) -> Conn {
    connect_config(addr, &Config::new().user(user, pw)).unwrap_or_else(|e| panic!("connect as {user}: {e}"))
}

/// A simple-protocol connection to `database`, None if that fails.
pub fn dial_db(addr: &Server, database: &str) -> Option<Conn> {
    connect_config(addr, &Config::new().database(database).simple())
        .map_err(|e| println!("dial {database}: {e}"))
        .ok()
}

#[track_caller]
pub fn must_create_database(addr: &Server, admin: &Conn, name: &str) -> Conn {
    must_exec(admin, &format!("CREATE DATABASE {name}"));
    dial_db(addr, name).unwrap_or_else(|| panic!("could not connect to the database just created: {name}"))
}

fn probe(addr: &Server, user: &str, pw: Option<&str>) -> Result<()> {
    let mut cfg = postgres::Config::new();
    cfg.host(addr.host()).port(addr.port()).user(user).dbname("test");
    cfg.connect_timeout(Duration::from_secs(5));
    if let Some(pw) = pw {
        cfg.password(pw);
    }
    match cfg.connect(postgres::NoTls) {
        Ok(client) => {
            let _ = client.close();
            Ok(())
        }
        Err(e) => Err(match e.as_db_error() {
            Some(db) => WireError::Pg(Box::new(PgError {
                severity: db.severity().to_string(),
                code: db.code().code().to_string(),
                message: db.message().to_string(),
                message_bytes: db.message().as_bytes().to_vec(),
                ..PgError::default()
            })),
            None => WireError::Client(format!("{e:?}")),
        }),
    }
}

/// dial: can user postgres log in with this password? An empty one is no password.
pub fn dial(addr: &Server, pw: &str) -> Result<()> {
    probe(addr, "postgres", (!pw.is_empty()).then_some(pw))
}

/// Can this role log in with this password?
pub fn dial_user(addr: &Server, user: &str, pw: &str) -> Result<()> {
    probe(addr, user, Some(pw))
}

fn result_field(fields: &[FieldDesc], formats: &[i16]) -> Result<Vec<FieldDesc>> {
    let mut out = fields.to_vec();
    match formats.len() {
        0 => out.iter_mut().for_each(|f| f.format = TEXT_FORMAT),
        1 => out.iter_mut().for_each(|f| f.format = formats[0]),
        n if n == out.len() => out.iter_mut().zip(formats).for_each(|(f, c)| f.format = *c),
        n => {
            return Err(client_err(format!(
                "result format codes length {n} does not match field count {}",
                out.len()
            )));
        }
    }
    Ok(out)
}

fn to_rows(r: PgResult) -> Rows {
    let fields = r.fields;
    let cells = |row: Vec<Option<Vec<u8>>>| Row {
        cells: row
            .into_iter()
            .enumerate()
            .map(|(i, bytes)| {
                let f = fields.get(i);
                Cell {
                    oid: f.map_or(0, |f| f.data_type_oid),
                    format: f.map_or(0, |f| f.format),
                    bytes,
                }
            })
            .collect(),
    };
    let rows = r.rows.into_iter().map(cells).collect();
    Rows {
        fields,
        rows,
        tag: r.tag,
    }
}

impl Inner {
    fn note(&mut self, msg: Backend) -> Option<Backend> {
        match msg {
            Backend::NotificationResponse(n) => {
                self.notifications.push_back(n);
                None
            }
            Backend::NoticeResponse(_) | Backend::ParameterStatus(..) => None,
            other => Some(other),
        }
    }

    /// Reads to ReadyForQuery. A failed read after an ErrorResponse reports the
    /// error: a FATAL closes the connection right behind it.
    fn drain(&mut self, mut on: impl FnMut(&mut Inner, Backend) -> Result<()>) -> Result<()> {
        let mut first: Option<WireError> = None;
        loop {
            let msg = match self.raw.receive() {
                Ok(m) => m,
                Err(e) => return Err(first.unwrap_or(e)),
            };
            match self.note(msg) {
                None => {}
                Some(Backend::ReadyForQuery(_)) => return first.map_or(Ok(()), Err),
                Some(Backend::ErrorResponse(e)) => {
                    first.get_or_insert(WireError::Pg(Box::new(e)));
                }
                Some(m) => {
                    if let Err(e) = on(self, m) {
                        first.get_or_insert(e);
                    }
                }
            }
        }
    }

    /// The results of a simple Query.
    fn read_all(&mut self) -> Result<Vec<PgResult>> {
        let mut out: Vec<PgResult> = Vec::new();
        let mut cur: Option<PgResult> = None;
        self.drain(|inner, msg| {
            match msg {
                Backend::RowDescription(fields) => {
                    cur = Some(PgResult {
                        fields,
                        ..PgResult::default()
                    });
                }
                Backend::DataRow(row) => cur.get_or_insert_with(PgResult::default).rows.push(row),
                Backend::CommandComplete(tag) => {
                    let mut r = cur.take().unwrap_or_default();
                    r.tag = Tag(tag);
                    out.push(r);
                }
                Backend::EmptyQueryResponse => out.push(cur.take().unwrap_or_default()),
                Backend::CopyInResponse(..) => {
                    inner.raw.send_copy_fail("no COPY data in a plain query");
                    inner.raw.flush()?;
                }
                _ => {}
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// The result of Bind/Execute/Sync.
    fn read_one(&mut self, described: Option<Vec<FieldDesc>>) -> Result<PgResult> {
        let mut r = PgResult {
            fields: described.unwrap_or_default(),
            ..PgResult::default()
        };
        self.drain(|_, msg| {
            match msg {
                Backend::RowDescription(fields) => r.fields = fields,
                Backend::DataRow(row) => r.rows.push(row),
                Backend::CommandComplete(tag) => r.tag = Tag(tag),
                _ => {}
            }
            Ok(())
        })?;
        Ok(r)
    }

    fn deallocate_invalid(&mut self) -> Result<()> {
        if self.invalid.is_empty() {
            return Ok(());
        }
        for name in std::mem::take(&mut self.invalid) {
            self.raw.send_close(b'S', &name);
        }
        self.raw.send_sync();
        self.raw.flush()?;
        self.read_one(None).map(|_| ())
    }

    fn invalidate(&mut self, sql: &str) {
        if let Some(sd) = self.statement_cache.remove(sql) {
            self.invalid.push(sd.name);
        }
    }

    fn pg_prepare(&mut self, name: &str, sql: &str) -> Result<StatementDesc> {
        self.raw.send_parse(name, sql, &[]);
        self.raw.send_describe(b'S', name);
        self.raw.send_sync();
        self.raw.flush()?;
        let mut sd = StatementDesc {
            name: name.into(),
            sql: sql.into(),
            param_oids: Vec::new(),
            fields: Vec::new(),
        };
        let mut parsed = false;
        let res = self.drain(|_, msg| {
            match msg {
                Backend::ParseComplete => parsed = true,
                Backend::ParameterDescription(oids) => sd.param_oids = oids,
                Backend::RowDescription(fields) => sd.fields = fields,
                _ => {}
            }
            Ok(())
        });
        if let Err(e) = res {
            if parsed && !name.is_empty() {
                self.failed_describe = Some(name.to_string());
            }
            return Err(e);
        }
        Ok(sd)
    }

    fn prepare(&mut self, name: &str, sql: &str) -> Result<StatementDesc> {
        if let Some(failed) = self.failed_describe.take() {
            self.raw.send_close(b'S', &failed);
            self.raw.send_sync();
            self.raw.flush()?;
            self.read_one(None)?;
        }
        if !name.is_empty()
            && let Some(sd) = self.prepared.get(name).filter(|sd| sd.sql == sql)
        {
            return Ok(sd.clone());
        }
        let (stmt, key) = if name == sql {
            (
                format!("stmt_{}", hex::encode(&Sha256::digest(sql.as_bytes())[..24])),
                sql.to_string(),
            )
        } else {
            (name.to_string(), name.to_string())
        };
        let sd = self.pg_prepare(&stmt, sql)?;
        if !key.is_empty() {
            self.prepared.insert(key, sd.clone());
        }
        Ok(sd)
    }

    /// The statement a query runs: an explicitly prepared one or the cached one.
    fn describe(&mut self, sql: &str) -> Result<StatementDesc> {
        if let Some(sd) = self.prepared.get(sql).or_else(|| self.statement_cache.get(sql)) {
            return Ok(sd.clone());
        }
        let name = format!("stmtcache_{}", hex::encode(&Sha256::digest(sql.as_bytes())[..24]));
        let sd = self.prepare(&name, sql)?;
        self.prepared.remove(&name);
        self.statement_cache.insert(sql.to_string(), sd.clone());
        Ok(sd)
    }

    /// Builds the Bind from the arguments, then executes.
    fn exec_statement(&mut self, sd: &StatementDesc, args: &[Arg], formats: Option<&[i16]>) -> Result<PgResult> {
        if sd.param_oids.len() != args.len() {
            return Err(client_err(format!(
                "expected {} arguments, got {}",
                sd.param_oids.len(),
                args.len()
            )));
        }
        let mut param_formats = Vec::with_capacity(args.len());
        let mut values = Vec::with_capacity(args.len());
        for (arg, oid) in args.iter().zip(&sd.param_oids) {
            let (format, value) = encode_param(arg, *oid);
            param_formats.push(format);
            values.push(value);
        }
        let default: Vec<i16> = sd.fields.iter().map(|f| format_code_for_oid(f.data_type_oid)).collect();
        let formats = formats.unwrap_or(&default);
        let fields = result_field(&sd.fields, formats)?;
        let params: Vec<Option<&[u8]>> = values.iter().map(Option::as_deref).collect();
        self.raw.send_bind("", &sd.name, &param_formats, &params, formats);
        self.raw.send_execute("", 0);
        self.raw.send_sync();
        self.raw.flush()?;
        self.read_one(Some(fields))
    }
}

/// The preferred format of the value for this OID, then the other one.
fn encode_param(arg: &Arg, oid: u32) -> (i16, Option<Vec<u8>>) {
    if matches!(arg, Arg::Null) {
        return (format_code_for_oid(oid), None);
    }
    if matches!(arg, Arg::Str(_)) || format_code_for_oid(oid) == TEXT_FORMAT {
        return (TEXT_FORMAT, arg.text(oid));
    }
    match arg.binary(oid) {
        Some(v) => (BINARY_FORMAT, Some(v)),
        None => (TEXT_FORMAT, arg.text(oid)),
    }
}

fn args_of(args: &[&dyn ToArg]) -> Vec<Arg> {
    args.iter().map(ToArg::to_arg).collect()
}

impl Conn {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    /// The last command tag. With no arguments this is always a simple
    /// Query, in pgx's extended mode too, so a batch of statements works.
    pub fn exec(&self, sql: &str, args: &[&dyn ToArg]) -> Result<Tag> {
        let args = args_of(args);
        let mut c = self.lock();
        c.deallocate_invalid()?;
        let sd = c.prepared.get(sql).cloned();
        let simple = self.mode == Mode::Simple || args.is_empty();
        let res = (|| match sd {
            Some(sd) => c.exec_statement(&sd, &args, None).map(|r| r.tag),
            None if simple => {
                // Like pgx, which leaves SQL alone when there is nothing to bind:
                // a $1 may belong to a PREPARE or a function body.
                let q = if args.is_empty() {
                    Cow::Borrowed(sql)
                } else {
                    Cow::Owned(sanitize_sql(sql, &args)?)
                };
                c.raw.send_query(q.as_bytes());
                c.raw.flush()?;
                Ok(c.read_all()?.pop().unwrap_or_default().tag)
            }
            None => {
                let sd = c.describe(sql)?;
                c.exec_statement(&sd, &args, None).map(|r| r.tag)
            }
        })();
        if res.is_err() {
            c.invalidate(sql);
        }
        res
    }

    /// A query, read to the end. In simple mode this is the first statement's result.
    pub fn query(&self, sql: &str, args: &[&dyn ToArg]) -> Result<Rows> {
        self.query_inner(sql, None, args)
    }

    /// A query with these result formats.
    pub fn query_formats(&self, sql: &str, formats: &[i16], args: &[&dyn ToArg]) -> Result<Rows> {
        self.query_inner(sql, Some(formats), args)
    }

    fn query_inner(&self, sql: &str, formats: Option<&[i16]>, args: &[&dyn ToArg]) -> Result<Rows> {
        let args = args_of(args);
        let mut c = self.lock();
        c.deallocate_invalid()?;
        let sd = c.prepared.get(sql).cloned();
        if sd.is_none() && (self.mode == Mode::Simple || sql.is_empty()) {
            let q = sanitize_sql(sql, &args)?;
            c.raw.send_query(q);
            c.raw.flush()?;
            let mut results = c.read_all()?;
            return Ok(to_rows(if results.is_empty() {
                PgResult::default()
            } else {
                results.swap_remove(0)
            }));
        }
        let res = (|| {
            let sd = match sd {
                Some(sd) => sd,
                None => c.describe(sql)?,
            };
            c.exec_statement(&sd, &args, formats)
        })();
        if res.is_err() {
            c.invalidate(sql);
        }
        res.map(to_rows)
    }

    /// Reads one row: `let n: i64 = conn.query_row(sql, &[]).scan().unwrap();`.
    pub fn query_row(&self, sql: &str, args: &[&dyn ToArg]) -> QueryRow {
        QueryRow(self.query(sql, args))
    }

    /// One row with these result formats.
    pub fn query_row_formats(&self, sql: &str, formats: &[i16], args: &[&dyn ToArg]) -> QueryRow {
        QueryRow(self.query_formats(sql, formats, args))
    }

    /// Prepares a statement. An empty name is the unnamed statement.
    pub fn prepare(&self, name: &str, sql: &str) -> Result<StatementDesc> {
        self.lock().prepare(name, sql)
    }

    pub fn begin(&self) -> Result<Tx<'_>> {
        self.exec("begin", &[])?;
        Ok(Tx { conn: self })
    }

    /// What pgx's Ping sends.
    pub fn ping(&self) -> Result<()> {
        self.pg_exec("-- ping").map(|_| ())
    }

    /// A simple Query: takes bytes, as SQL here need not be UTF-8.
    pub fn pg_exec(&self, sql: impl AsRef<[u8]>) -> Result<Vec<PgResult>> {
        let mut c = self.lock();
        c.raw.send_query(sql);
        c.raw.flush()?;
        c.read_all()
    }

    /// An unnamed Parse/Bind/Describe/Execute cycle with these formats.
    pub fn exec_params<P: AsRef<[u8]>>(
        &self,
        sql: impl AsRef<[u8]>,
        params: &[P],
        oids: &[u32],
        param_formats: &[i16],
        result_formats: &[i16],
    ) -> Result<PgResult> {
        let mut c = self.lock();
        let values: Vec<Option<&[u8]>> = params.iter().map(|p| Some(p.as_ref())).collect();
        c.raw.send_parse("", sql, oids);
        c.raw.send_bind("", "", param_formats, &values, result_formats);
        c.raw.send_describe(b'P', "");
        c.raw.send_execute("", 0);
        c.raw.send_sync();
        c.raw.flush()?;
        c.read_one(None)
    }

    /// COPY ... FROM STDIN.
    pub fn copy_from(&self, mut data: impl Read, sql: &str) -> Result<Tag> {
        let mut c = self.lock();
        c.raw.send_query(sql);
        c.raw.flush()?;
        let mut tag = Tag::default();
        let mut started = false;
        loop {
            match c.raw.receive()? {
                Backend::CopyInResponse(..) => {
                    started = true;
                    break;
                }
                Backend::ErrorResponse(e) => {
                    c.raw.wait_ready()?;
                    return Err(WireError::Pg(Box::new(e)));
                }
                Backend::ReadyForQuery(_) => break,
                _ => {}
            }
        }
        if !started {
            return Ok(tag);
        }
        let mut chunk = vec![0u8; 65536];
        loop {
            match data.read(&mut chunk) {
                Ok(0) => {
                    c.raw.send_copy_done();
                    break;
                }
                Ok(n) => {
                    c.raw.send_copy_data(&chunk[..n]);
                    c.raw.flush()?;
                }
                Err(e) => {
                    c.raw.send_copy_fail(&e.to_string());
                    break;
                }
            }
        }
        c.raw.flush()?;
        c.drain(|_, msg| {
            if let Backend::CommandComplete(t) = msg {
                tag = Tag(t);
            }
            Ok(())
        })?;
        Ok(tag)
    }

    /// COPY ... TO STDOUT.
    pub fn copy_to(&self, out: &mut impl Write, sql: &str) -> Result<Tag> {
        let mut c = self.lock();
        c.raw.send_query(sql);
        c.raw.flush()?;
        let mut tag = Tag::default();
        c.drain(|_, msg| {
            match msg {
                Backend::CopyData(d) => out.write_all(&d)?,
                Backend::CommandComplete(t) => tag = Tag(t),
                _ => {}
            }
            Ok(())
        })?;
        Ok(tag)
    }

    /// Waits for a notification under a deadline.
    pub fn wait_for_notification(&self, timeout: Duration) -> Result<Notification> {
        let mut c = self.lock();
        if let Some(n) = c.notifications.pop_front() {
            return Ok(n);
        }
        c.raw.set_read_timeout(Some(timeout))?;
        let res = loop {
            match c.raw.receive() {
                Ok(Backend::NotificationResponse(n)) => break Ok(n),
                Ok(_) => {}
                Err(e) => break Err(e),
            }
        };
        c.raw.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
        res
    }

    /// A deadline for the calls that follow, which then fail with `is_timeout`.
    pub fn set_read_timeout(&self, d: Duration) {
        self.lock().raw.set_read_timeout(Some(d)).expect("set the read timeout");
    }

    /// The TLS session, when the connection has one.
    pub fn is_tls(&self) -> bool {
        self.lock().raw.is_tls()
    }

    /// Terminate, and the socket is closed at once. The value stays
    /// usable, and every call on it fails.
    pub fn close(&self) -> Result<()> {
        let mut c = self.lock();
        c.raw.send_terminate();
        let res = c.raw.flush();
        c.raw.shutdown();
        res
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let mut c = self.lock();
        c.raw.send_terminate();
        let _ = c.raw.flush();
    }
}

/// pgx.Tx: the connection, plus commit and rollback.
#[derive(Debug)]
pub struct Tx<'a> {
    conn: &'a Conn,
}

impl std::ops::Deref for Tx<'_> {
    type Target = Conn;

    fn deref(&self) -> &Conn {
        self.conn
    }
}

impl Tx<'_> {
    pub fn commit(self) -> Result<()> {
        let tag = self.conn.exec("commit", &[])?;
        if tag.as_str() == "ROLLBACK" {
            return Err(client_err("commit unexpectedly resulted in rollback"));
        }
        Ok(())
    }

    pub fn rollback(self) -> Result<()> {
        self.conn.exec("rollback", &[]).map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Runs sql, which must succeed.
#[track_caller]
pub fn must_exec(conn: &Conn, sql: &str) {
    if let Err(e) = conn.exec(sql, &[]) {
        panic!("exec {sql:?}: {e}");
    }
}

#[track_caller]
pub fn scan_str(conn: &Conn, q: &str) -> String {
    conn.query_row(q, &[])
        .scan()
        .unwrap_or_else(|e| panic!("query {q:?}: {e}"))
}

#[track_caller]
pub fn seq_val(conn: &Conn, q: &str) -> i64 {
    conn.query_row(q, &[])
        .scan()
        .unwrap_or_else(|e| panic!("query {q:?}: {e}"))
}

/// The strings of one column, skipping values that are not strings.
#[track_caller]
pub fn query_column(conn: &Conn, q: &str, col: usize) -> Vec<String> {
    let rows = conn
        .query(q, &[])
        .unwrap_or_else(|e| panic!("query must execute:\n{q}\n{e}"));
    rows.iter().filter_map(|r| r.cell(col).text_value()).collect()
}

#[track_caller]
pub fn run_psql_query(conn: &Conn, i: usize, q: &str) {
    if let Err(e) = conn.query(q, &[]) {
        panic!("psql \\d sub-query #{i} must execute, with no row error:\n{q}\n{e}");
    }
}

/// The rows of the last result of a simple query, as bytes.
#[track_caller]
pub fn raw_rows(conn: &Conn, sql: impl AsRef<[u8]>) -> Vec<Vec<Option<Vec<u8>>>> {
    let sql = sql.as_ref();
    let mut rs = conn
        .pg_exec(sql)
        .unwrap_or_else(|e| panic!("{}: {e}", String::from_utf8_lossy(sql)));
    rs.pop().unwrap_or_default().rows
}

pub fn hex_of(b: &[u8]) -> String {
    hex::encode_upper(b)
}

/// stored: what SQLite holds, bypassing the text path.
#[track_caller]
pub fn stored(conn: &Conn, table: &str, id: i64) -> String {
    let rows = raw_rows(conn, format!("SELECT hex(v) FROM {table} WHERE id = {id}"));
    assert_eq!(rows.len(), 1);
    String::from_utf8(rows[0][0].clone().unwrap_or_default()).expect("hex is ASCII")
}

/// DataRows until PortalSuspended (true) or CommandComplete (false).
#[track_caller]
pub fn collect_execute(fe: &mut RawConn) -> (Vec<String>, bool) {
    let mut rows = Vec::new();
    loop {
        match fe.receive().expect("receive") {
            Backend::DataRow(row) => {
                rows.push(String::from_utf8_lossy(row[0].as_deref().unwrap_or(&[])).into_owned());
            }
            Backend::PortalSuspended => return (rows, true),
            Backend::CommandComplete(_) => return (rows, false),
            Backend::RowDescription(_) | Backend::ParseComplete | Backend::BindComplete => {}
            other => panic!("unexpected message {other:?}"),
        }
    }
}

/// `method` takes the right password and refuses the wrong one.
#[track_caller]
pub fn auth_method_check(method: &str) {
    let mut env = Env::password("pw123");
    if !method.is_empty() {
        env.setenv("POSTGRES_HOST_AUTH_METHOD", method);
    }
    let addr = start_server_env(env);
    let dial = |pw: &str| dial(&addr, pw);
    assert!(
        dial("pw123").is_ok(),
        "correct password should connect ({method}): {:?}",
        dial("pw123")
    );
    assert!(dial("nope").is_err(), "wrong password should be rejected ({method})");
}

/// A docs table under RLS, each role seeing the rows it owns.
#[track_caller]
pub fn rls_setup() -> Server {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'b'");
    must_exec(&admin, "CREATE TABLE docs (id int, owner text, body text)");
    must_exec(
        &admin,
        "INSERT INTO docs VALUES (1,'alice','a1'),(2,'bob','b1'),(3,'alice','a2')",
    );
    must_exec(&admin, "GRANT ALL ON docs TO alice");
    must_exec(&admin, "GRANT ALL ON docs TO bob");
    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY own ON docs FOR ALL USING (owner = current_user)");
    addr
}

/// digits: a pseudo-random string of n digits.
pub fn digits(n: usize, seed: u8) -> String {
    let mut b = String::with_capacity(n);
    b.push((b'1' + seed % 9) as char);
    for i in 1..n {
        b.push((b'0' + ((i * 7 + seed as usize * 13 + i * i % 11) % 10) as u8) as char);
    }
    b
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// Something assert_contains! can search.
pub trait Haystack {
    fn has(&self, needle: &str) -> bool;
}

impl Haystack for str {
    fn has(&self, needle: &str) -> bool {
        self.contains(needle)
    }
}

impl Haystack for String {
    fn has(&self, needle: &str) -> bool {
        self.contains(needle)
    }
}

impl<T: Haystack + ?Sized> Haystack for &T {
    fn has(&self, needle: &str) -> bool {
        (**self).has(needle)
    }
}

impl Haystack for WireError {
    fn has(&self, needle: &str) -> bool {
        self.to_string().contains(needle)
    }
}

/// A list holds the element.
impl Haystack for [String] {
    fn has(&self, needle: &str) -> bool {
        self.iter().any(|s| s == needle)
    }
}

impl Haystack for Vec<String> {
    fn has(&self, needle: &str) -> bool {
        self.iter().any(|s| s == needle)
    }
}

/// A substring of a string or WireError, an element of a list.
#[macro_export]
macro_rules! assert_contains {
    ($hay:expr, $needle:expr $(,)?) => {{
        let (hay, needle) = (&$hay, &$needle);
        assert!($crate::common::Haystack::has(hay, needle), "{:?} does not contain {:?}", hay, needle);
    }};
    ($hay:expr, $needle:expr, $($msg:tt)+) => {{
        let (hay, needle) = (&$hay, &$needle);
        assert!($crate::common::Haystack::has(hay, needle), "{}: {:?} does not contain {:?}", format!($($msg)+), hay, needle);
    }};
}

/// The opposite of assert_contains!.
#[macro_export]
macro_rules! assert_not_contains {
    ($hay:expr, $needle:expr $(,)?) => {{
        let (hay, needle) = (&$hay, &$needle);
        assert!(!$crate::common::Haystack::has(hay, needle), "{:?} should not contain {:?}", hay, needle);
    }};
    ($hay:expr, $needle:expr, $($msg:tt)+) => {{
        let (hay, needle) = (&$hay, &$needle);
        assert!(!$crate::common::Haystack::has(hay, needle), "{}: {:?} should not contain {:?}", format!($($msg)+), hay, needle);
    }};
}

/// The text matches the pattern.
#[track_caller]
pub fn assert_regexp(re: &str, s: &str) {
    assert!(
        regex::Regex::new(re).expect("regexp").is_match(s),
        "{s:?} does not match {re}"
    );
}

/// The two are the same JSON.
#[track_caller]
pub fn assert_json_eq(want: &str, got: &str) {
    let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap_or_else(|e| panic!("{s:?}: {e}"));
    assert_eq!(parse(want), parse(got));
}

/// Every element of `want` is in `list`.
#[track_caller]
pub fn assert_subset(list: &[String], want: &[&str]) {
    for w in want {
        assert!(list.iter().any(|s| s == w), "{list:?} does not contain {w:?}");
    }
}

/// The file exists.
#[track_caller]
pub fn assert_file_exists(path: impl AsRef<Path>) {
    assert!(path.as_ref().is_file(), "{} does not exist", path.as_ref().display());
}

/// The file does not exist.
#[track_caller]
pub fn assert_no_file_exists(path: impl AsRef<Path>) {
    assert!(!path.as_ref().exists(), "{} exists", path.as_ref().display());
}

/// Polls until cond holds, failing after wait.
#[track_caller]
pub fn eventually(wait: Duration, tick: Duration, mut cond: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < wait {
        if cond() {
            return;
        }
        std::thread::sleep(tick);
    }
    panic!("condition never satisfied within {wait:?}");
}

// ---------------------------------------------------------------------------
// Quoting, for the parity goldens
// ---------------------------------------------------------------------------

fn is_printable(c: char) -> bool {
    static PRINT: OnceLock<regex::Regex> = OnceLock::new();
    let re = PRINT.get_or_init(|| regex::Regex::new(r"^[\p{L}\p{M}\p{N}\p{P}\p{S}]$").expect("regex"));
    c == ' ' || re.is_match(c.encode_utf8(&mut [0; 4]))
}

/// Bytes double-quoted, as the goldens hold them.
pub fn quote_bytes(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() + 2);
    out.push('"');
    for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\x07' => out.push_str("\\a"),
                '\x08' => out.push_str("\\b"),
                '\x0c' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\x0b' => out.push_str("\\v"),
                c if is_printable(c) => out.push(c),
                c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
                c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push_str(&format!("\\U{:08x}", c as u32)),
            }
        }
        for byte in chunk.invalid() {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    out.push('"');
    out
}

/// The bytes `quote_bytes` wrote.
pub fn unquote_bytes(s: &str) -> Vec<u8> {
    let inner = s
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .expect("a quoted string");
    let mut out = Vec::with_capacity(inner.len());
    let mut chars = inner.chars();
    let hex_of = |chars: &mut std::str::Chars<'_>, n: usize| {
        let h: String = chars.by_ref().take(n).collect();
        u32::from_str_radix(&h, 16).expect("hex escape")
    };
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            continue;
        }
        match chars.next().expect("escape") {
            'a' => out.push(7),
            'b' => out.push(8),
            'f' => out.push(12),
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            'v' => out.push(11),
            'x' => out.push(hex_of(&mut chars, 2) as u8),
            'u' => {
                let c = char::from_u32(hex_of(&mut chars, 4)).expect("code point");
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            'U' => {
                let c = char::from_u32(hex_of(&mut chars, 8)).expect("code point");
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            c => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
        }
    }
    out
}

/// The error text, as bytes: a message need not be UTF-8.
pub fn err_text(err: &WireError) -> Vec<u8> {
    match err {
        WireError::Pg(e) => [format!("{} {} ", e.severity, e.code).as_bytes(), &e.message_bytes].concat(),
        other => other.to_string().into_bytes(),
    }
}

/// outcome: everything a client can observe of one statement, as bytes.
pub fn outcome(conn: &Conn, sql: impl AsRef<[u8]>, binary: bool) -> Vec<u8> {
    let sql = sql.as_ref();
    conn.set_read_timeout(Duration::from_secs(30));
    let mut b = Vec::new();
    if binary {
        let r = match conn.exec_params::<&[u8]>(sql, &[], &[], &[], &[BINARY_FORMAT]) {
            Ok(r) => r,
            Err(e) => return err_outcome(&e),
        };
        for row in &r.rows {
            let cols: Vec<String> = row.iter().map(|c| hex::encode(c.as_deref().unwrap_or(&[]))).collect();
            b.extend_from_slice(format!("[{}]|", cols.join(" ")).as_bytes());
        }
        return b;
    }
    let rs = match conn.pg_exec(sql) {
        Ok(rs) => rs,
        Err(e) => return err_outcome(&e),
    };
    for r in &rs {
        for f in &r.fields {
            b.extend_from_slice(format!("{}:{} ", quote_bytes(&f.name_bytes), f.data_type_oid).as_bytes());
        }
        for row in &r.rows {
            for c in row {
                b.extend_from_slice(quote_bytes(c.as_deref().unwrap_or(&[])).as_bytes());
                b.push(b',');
            }
            b.push(b';');
        }
        b.extend_from_slice(r.tag.as_str().as_bytes());
        b.push(b'\n');
    }
    b
}

fn err_outcome(e: &WireError) -> Vec<u8> {
    [b"ERR ".as_slice(), &err_text(e)].concat()
}

// ---------------------------------------------------------------------------
// pgx's default type map
// ---------------------------------------------------------------------------

/// The format pgx prefers for an OID.
pub fn format_code_for_oid(oid: u32) -> i16 {
    match pgx_type(oid) {
        Some(t) if pgx_binary(t.1, t.2) => BINARY_FORMAT,
        _ => TEXT_FORMAT,
    }
}

fn pgx_binary(codec: &str, elem: u32) -> bool {
    match codec {
        "TextCodec" | "TextFormatOnlyCodec" | "JSONCodec" | "JSONBCodec" | "XMLCodec" => false,
        // Arrays are binary whenever their element can be, text included.
        "ArrayCodec" => pgx_type(elem).is_none_or(|t| t.1 != "TextFormatOnlyCodec"),
        _ => true,
    }
}

fn pgx_type(oid: u32) -> Option<&'static (&'static str, &'static str, u32)> {
    static MAP: OnceLock<HashMap<u32, &'static (&'static str, &'static str, u32)>> = OnceLock::new();
    MAP.get_or_init(|| PGX_TYPES.iter().map(|(oid, t)| (*oid, t)).collect())
        .get(&oid)
        .copied()
}

/// The default type map of pgx v5.10.0: OID, name, codec, element OID of an array.
const PGX_TYPES: &[(u32, (&str, &str, u32))] = &[
    (16, ("bool", "BoolCodec", 0)),
    (17, ("bytea", "ByteaCodec", 0)),
    (18, ("char", "QCharCodec", 0)),
    (19, ("name", "TextCodec", 0)),
    (20, ("int8", "Int8Codec", 0)),
    (21, ("int2", "Int2Codec", 0)),
    (23, ("int4", "Int4Codec", 0)),
    (25, ("text", "TextCodec", 0)),
    (26, ("oid", "Uint32Codec", 0)),
    (27, ("tid", "TIDCodec", 0)),
    (28, ("xid", "Uint32Codec", 0)),
    (29, ("cid", "Uint32Codec", 0)),
    (114, ("json", "JSONCodec", 0)),
    (142, ("xml", "XMLCodec", 0)),
    (143, ("_xml", "ArrayCodec", 142)),
    (199, ("_json", "ArrayCodec", 114)),
    (271, ("_xid8", "ArrayCodec", 5069)),
    (600, ("point", "PointCodec", 0)),
    (601, ("lseg", "LsegCodec", 0)),
    (602, ("path", "PathCodec", 0)),
    (603, ("box", "BoxCodec", 0)),
    (604, ("polygon", "PolygonCodec", 0)),
    (628, ("line", "LineCodec", 0)),
    (629, ("_line", "ArrayCodec", 628)),
    (650, ("cidr", "InetCodec", 0)),
    (651, ("_cidr", "ArrayCodec", 650)),
    (700, ("float4", "Float4Codec", 0)),
    (701, ("float8", "Float8Codec", 0)),
    (705, ("unknown", "TextCodec", 0)),
    (718, ("circle", "CircleCodec", 0)),
    (719, ("_circle", "ArrayCodec", 718)),
    (774, ("macaddr8", "MacaddrCodec", 0)),
    (829, ("macaddr", "MacaddrCodec", 0)),
    (869, ("inet", "InetCodec", 0)),
    (1000, ("_bool", "ArrayCodec", 16)),
    (1001, ("_bytea", "ArrayCodec", 17)),
    (1002, ("_char", "ArrayCodec", 18)),
    (1003, ("_name", "ArrayCodec", 19)),
    (1005, ("_int2", "ArrayCodec", 21)),
    (1007, ("_int4", "ArrayCodec", 23)),
    (1009, ("_text", "ArrayCodec", 25)),
    (1010, ("_tid", "ArrayCodec", 27)),
    (1011, ("_xid", "ArrayCodec", 28)),
    (1012, ("_cid", "ArrayCodec", 29)),
    (1014, ("_bpchar", "ArrayCodec", 1042)),
    (1015, ("_varchar", "ArrayCodec", 1043)),
    (1016, ("_int8", "ArrayCodec", 20)),
    (1017, ("_point", "ArrayCodec", 600)),
    (1018, ("_lseg", "ArrayCodec", 601)),
    (1019, ("_path", "ArrayCodec", 602)),
    (1020, ("_box", "ArrayCodec", 603)),
    (1021, ("_float4", "ArrayCodec", 700)),
    (1022, ("_float8", "ArrayCodec", 701)),
    (1027, ("_polygon", "ArrayCodec", 604)),
    (1028, ("_oid", "ArrayCodec", 26)),
    (1033, ("aclitem", "TextFormatOnlyCodec", 0)),
    (1034, ("_aclitem", "ArrayCodec", 1033)),
    (1040, ("_macaddr", "ArrayCodec", 829)),
    (1041, ("_inet", "ArrayCodec", 869)),
    (1042, ("bpchar", "TextCodec", 0)),
    (1043, ("varchar", "TextCodec", 0)),
    (1082, ("date", "DateCodec", 0)),
    (1083, ("time", "TimeCodec", 0)),
    (1114, ("timestamp", "TimestampCodec", 0)),
    (1115, ("_timestamp", "ArrayCodec", 1114)),
    (1182, ("_date", "ArrayCodec", 1082)),
    (1183, ("_time", "ArrayCodec", 1083)),
    (1184, ("timestamptz", "TimestamptzCodec", 0)),
    (1185, ("_timestamptz", "ArrayCodec", 1184)),
    (1186, ("interval", "IntervalCodec", 0)),
    (1187, ("_interval", "ArrayCodec", 1186)),
    (1231, ("_numeric", "ArrayCodec", 1700)),
    (1560, ("bit", "BitsCodec", 0)),
    (1561, ("_bit", "ArrayCodec", 1560)),
    (1562, ("varbit", "BitsCodec", 0)),
    (1563, ("_varbit", "ArrayCodec", 1562)),
    (1700, ("numeric", "NumericCodec", 0)),
    (2249, ("record", "RecordCodec", 0)),
    (2287, ("_record", "ArrayCodec", 2249)),
    (2950, ("uuid", "UUIDCodec", 0)),
    (2951, ("_uuid", "ArrayCodec", 2950)),
    (3614, ("tsvector", "TSVectorCodec", 0)),
    (3643, ("_tsvector", "ArrayCodec", 3614)),
    (3802, ("jsonb", "JSONBCodec", 0)),
    (3807, ("_jsonb", "ArrayCodec", 3802)),
    (3904, ("int4range", "RangeCodec", 0)),
    (3905, ("_int4range", "ArrayCodec", 3904)),
    (3906, ("numrange", "RangeCodec", 0)),
    (3907, ("_numrange", "ArrayCodec", 3906)),
    (3908, ("tsrange", "RangeCodec", 0)),
    (3909, ("_tsrange", "ArrayCodec", 3908)),
    (3910, ("tstzrange", "RangeCodec", 0)),
    (3911, ("_tstzrange", "ArrayCodec", 3910)),
    (3912, ("daterange", "RangeCodec", 0)),
    (3913, ("_daterange", "ArrayCodec", 3912)),
    (3926, ("int8range", "RangeCodec", 0)),
    (3927, ("_int8range", "ArrayCodec", 3926)),
    (4072, ("jsonpath", "TextFormatOnlyCodec", 0)),
    (4073, ("_jsonpath", "ArrayCodec", 4072)),
    (4451, ("int4multirange", "MultirangeCodec", 0)),
    (4532, ("nummultirange", "MultirangeCodec", 0)),
    (4533, ("tsmultirange", "MultirangeCodec", 0)),
    (4534, ("tstzmultirange", "MultirangeCodec", 0)),
    (4535, ("datemultirange", "MultirangeCodec", 0)),
    (4536, ("int8multirange", "MultirangeCodec", 0)),
    (5069, ("xid8", "Uint64Codec", 0)),
];
