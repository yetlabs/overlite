//! Framed, buffered Postgres wire I/O. Reading and writing are
//! split so a NOTIFY from another thread can reach an idle client whose own
//! thread is blocked in a read.

pub(crate) mod tls;

use crate::error::{Result, errf};
use crate::session::cancel::CancelRequest;
use crate::session::format_for;
use crate::types::{Format, NoBinaryForm, encode_value, oid_for_column};
use overlite_core::sync::MutexExt;
use overlite_core::text::{text_from_bytes, text_to_bytes};
use overlite_core::{Column, Oid, ResultSet, SqlState, Value};
use overlite_engine::declared_typmod;
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::{Arc, Mutex};

// Frontend messages.
pub(crate) const MSG_QUERY: u8 = b'Q';
pub(crate) const MSG_PARSE: u8 = b'P';
const MSG_FUNCTION_CALL: u8 = b'F';
pub(crate) const MSG_BIND: u8 = b'B';
pub(crate) const MSG_DESCRIBE: u8 = b'D';
pub(crate) const MSG_EXECUTE: u8 = b'E';
pub(crate) const MSG_SYNC: u8 = b'S';
pub(crate) const MSG_FLUSH: u8 = b'H';
pub(crate) const MSG_CLOSE: u8 = b'C';
pub(crate) const MSG_PASSWORD_MESSAGE: u8 = b'p';
pub(crate) const MSG_TERMINATE: u8 = b'X';
// Backend messages.
pub(crate) const MSG_AUTHENTICATION: u8 = b'R';
const MSG_PARAMETER_STATUS: u8 = b'S';
pub(crate) const MSG_BACKEND_KEY_DATA: u8 = b'K';
pub(crate) const MSG_READY_FOR_QUERY: u8 = b'Z';
const MSG_ROW_DESCRIPTION: u8 = b'T';
const MSG_DATA_ROW: u8 = b'D';
const MSG_COMMAND_COMPLETE: u8 = b'C';
pub(crate) const MSG_EMPTY_QUERY: u8 = b'I';
const MSG_ERROR_RESPONSE: u8 = b'E';
const MSG_NOTICE_RESPONSE: u8 = b'N';
pub(crate) const MSG_PARSE_COMPLETE: u8 = b'1';
pub(crate) const MSG_BIND_COMPLETE: u8 = b'2';
pub(crate) const MSG_CLOSE_COMPLETE: u8 = b'3';
const MSG_PARAMETER_DESCRIPTION: u8 = b't';
pub(crate) const MSG_NO_DATA: u8 = b'n';
pub(crate) const MSG_NOTIFICATION: u8 = b'A';
pub(crate) const MSG_COPY_IN_RESPONSE: u8 = b'G';
pub(crate) const MSG_COPY_OUT_RESPONSE: u8 = b'H';
pub(crate) const MSG_COPY_DATA: u8 = b'd';
pub(crate) const MSG_COPY_DONE: u8 = b'c';
pub(crate) const MSG_COPY_FAIL: u8 = b'f';
pub(crate) const MSG_PORTAL_SUSPENDED: u8 = b's';

const PROTOCOL_VERSION3: i32 = 196608;
const CANCEL_REQUEST_CODE: i32 = 80877102;
const SSL_REQUEST_CODE: i32 = 80877103;
const GSS_REQUEST_CODE: i32 = 80877104;

/// The longest startup packet, length word included: Postgres' 10000 past that word.
const MAX_STARTUP_LENGTH: i32 = 10_000 + 4;
/// The longest message of any type, length word included: PQ_LARGE_MESSAGE_LIMIT.
const MAX_LARGE_LENGTH: i32 = (1 << 30) - 2;
/// The longest message with no reason to be long: PQ_SMALL_MESSAGE_LIMIT.
const MAX_SMALL_LENGTH: i32 = 10_000;
/// The longest message while authenticating: PG_MAX_AUTH_TOKEN_LENGTH.
const MAX_AUTH_LENGTH: i32 = 65_535;
/// What a message's length may have allocated before its bytes arrive.
const UNTRUSTED_ALLOCATION: usize = 1 << 20;

/// How long a message of this type may be, as Postgres' SocketBackend has it. A type
/// it does not know goes through as small and is refused once read.
fn max_message_length(typ: u8) -> i32 {
    match typ {
        MSG_QUERY | MSG_PARSE | MSG_BIND | MSG_COPY_DATA | MSG_FUNCTION_CALL => MAX_LARGE_LENGTH,
        _ => MAX_SMALL_LENGTH,
    }
}

/// A TLS session whose reads wait for the socket without holding the lock, so
/// a write from another thread can get through.
#[derive(Debug)]
pub(crate) struct TlsStream {
    tcp: TcpStream,
    conn: Mutex<rustls::ServerConnection>,
}

impl TlsStream {
    fn handshake(tcp: TcpStream, cfg: Arc<rustls::ServerConfig>) -> Result<TlsStream> {
        let mut conn =
            rustls::ServerConnection::new(cfg).map_err(|e| errf!(SqlState::INTERNAL_ERROR, "tls handshake: {e}"))?;
        let mut io = &tcp;
        while conn.is_handshaking() {
            conn.complete_io(&mut io)
                .map_err(|e| errf!(SqlState::INTERNAL_ERROR, "tls handshake: {e}"))?;
        }
        Ok(TlsStream {
            tcp,
            conn: Mutex::new(conn),
        })
    }

    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            {
                let mut conn = self.conn.locked();
                match conn.reader().read(buf) {
                    Ok(n) => return Ok(n),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            let mut probe = [0u8; 1];
            if self.tcp.peek(&mut probe)? == 0 {
                return Ok(0);
            }
            let mut conn = self.conn.locked();
            let mut io = &self.tcp;
            conn.read_tls(&mut io)?;
            conn.process_new_packets().map_err(std::io::Error::other)?;
            while conn.wants_write() {
                conn.write_tls(&mut io)?;
            }
        }
    }

    fn write_all(&self, data: &[u8]) -> std::io::Result<()> {
        let mut conn = self.conn.locked();
        conn.writer().write_all(data)?;
        let mut io = &self.tcp;
        while conn.wants_write() {
            conn.write_tls(&mut io)?;
        }
        Ok(())
    }
}

/// The connection, plain or TLS, shareable between threads.
#[derive(Debug, Clone)]
pub(crate) enum NetStream {
    Plain(Arc<TcpStream>),
    Tls(Arc<TlsStream>),
}

impl NetStream {
    pub(crate) fn write_all(&self, data: &[u8]) -> std::io::Result<()> {
        match self {
            NetStream::Plain(s) => (&**s).write_all(data),
            NetStream::Tls(s) => s.write_all(data),
        }
    }
}

impl Read for NetStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            NetStream::Plain(s) => (&**s).read(buf),
            NetStream::Tls(s) => s.read(buf),
        }
    }
}

/// What the startup phase yields.
#[derive(Debug)]
pub(crate) enum Startup {
    Params(HashMap<String, String>),
    Cancel(CancelRequest),
}

/// A raw connection with buffered framed I/O.
#[derive(Debug)]
pub(crate) struct WireConn {
    raw: Arc<TcpStream>,
    r: BufReader<NetStream>,
    /// Pending output.
    w: Vec<u8>,
    pub(crate) sink: NetStream,
    /// The connection was upgraded to TLS.
    pub(crate) secured: bool,
    /// An ErrorResponse went out since this was last cleared.
    pub(crate) errored: bool,
}

/// Bytes buffered before a send writes through to the socket.
const WRITE_BUFFER: usize = 16 * 1024;

impl WireConn {
    pub(crate) fn new(c: TcpStream) -> WireConn {
        let raw = Arc::new(c);
        let sink = NetStream::Plain(raw.clone());
        WireConn {
            r: BufReader::with_capacity(8192, sink.clone()),
            raw,
            sink,
            w: Vec::with_capacity(WRITE_BUFFER),
            secured: false,
            errored: false,
        }
    }

    /// The client's address, None for a non-TCP peer.
    pub(crate) fn peer_ip(&self) -> Option<IpAddr> {
        self.raw.peer_addr().ok().map(|a| a.ip())
    }

    /// Handles SSL/GSS negotiation and returns the StartupMessage parameters,
    /// or a CancelRequest.
    pub(crate) fn read_startup(&mut self, tls: Option<&Arc<rustls::ServerConfig>>) -> Result<Startup> {
        loop {
            let length = self.read_int32()?;
            // As Postgres: a packet this wrong gets no answer, only the hangup.
            if !(8..=MAX_STARTUP_LENGTH).contains(&length) {
                return Err(errf!(
                    SqlState::PROTOCOL_VIOLATION,
                    "invalid length of startup packet: {length}"
                ));
            }
            let mut body = vec![0u8; (length - 4) as usize];
            self.r.read_exact(&mut body)?;
            let word = |at: usize| {
                body.get(at..)
                    .and_then(<[u8]>::first_chunk)
                    .copied()
                    .map(i32::from_be_bytes)
            };
            let code = word(0).unwrap_or_default();
            match code {
                SSL_REQUEST_CODE => match tls {
                    None => self.write_raw(b"N")?,
                    Some(cfg) => {
                        self.write_raw(b"S")?;
                        self.upgrade_tls(cfg.clone())?;
                    }
                },
                GSS_REQUEST_CODE => self.write_raw(b"N")?, // no GSSAPI encryption
                CANCEL_REQUEST_CODE => {
                    let (Some(pid), Some(secret)) = (word(4), word(8)) else {
                        return Err(errf!(SqlState::PROTOCOL_VIOLATION, "cancel request: short body"));
                    };
                    return Ok(Startup::Cancel(CancelRequest { pid, secret }));
                }
                PROTOCOL_VERSION3 => return Ok(Startup::Params(parse_startup_params(&body[4..]))),
                _ => {
                    return Err(errf!(
                        SqlState::PROTOCOL_VIOLATION,
                        "unsupported protocol/startup code {code}"
                    ));
                }
            }
        }
    }

    /// The client sends only the SSLRequest before waiting for the reply, so no
    /// buffered plaintext is lost.
    fn upgrade_tls(&mut self, cfg: Arc<rustls::ServerConfig>) -> Result<()> {
        let tcp = self.raw.try_clone()?;
        let t = NetStream::Tls(Arc::new(TlsStream::handshake(tcp, cfg)?));
        self.r = BufReader::with_capacity(8192, t.clone());
        self.sink = t;
        self.secured = true;
        Ok(())
    }

    /// One typed frontend message of a logged-in client: its type byte and body.
    pub(crate) fn read_message(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        self.read_message_within(max_message_length)
    }

    /// The next message of a client still authenticating.
    pub(crate) fn read_auth_message(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        self.read_message_within(|_| MAX_AUTH_LENGTH)
    }

    fn read_message_within(&mut self, limit: impl Fn(u8) -> i32) -> std::io::Result<(u8, Vec<u8>)> {
        let mut head = [0u8; 5];
        self.r.read_exact(&mut head)?;
        let [typ, a, b, c, d] = head;
        let length = i32::from_be_bytes([a, b, c, d]);
        if !(4..=limit(typ)).contains(&length) {
            // As Postgres: no answer, only the hangup.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid length {length} of message {}", typ as char),
            ));
        }
        Ok((typ, self.read_body((length - 4) as usize)?))
    }

    /// Reads n bytes. Past a size any client may simply claim, the buffer grows as the
    /// bytes arrive, so what a length costs is what was really sent.
    fn read_body(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        if n <= UNTRUSTED_ALLOCATION {
            let mut body = vec![0u8; n];
            self.r.read_exact(&mut body)?;
            return Ok(body);
        }
        let mut body = Vec::with_capacity(UNTRUSTED_ALLOCATION);
        (&mut self.r).take(n as u64).read_to_end(&mut body)?;
        if body.len() < n {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(body)
    }

    fn read_int32(&mut self) -> Result<i32> {
        let mut b = [0u8; 4];
        self.r.read_exact(&mut b)?;
        Ok(i32::from_be_bytes(b))
    }

    /// Queues a typed backend message from an already-built body.
    pub(crate) fn send(&mut self, typ: u8, body: &[u8]) -> Result<()> {
        self.w.push(typ);
        self.w.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
        self.w.extend_from_slice(body);
        if self.w.len() >= WRITE_BUFFER {
            self.flush()?;
        }
        Ok(())
    }

    fn write_raw(&mut self, b: &[u8]) -> Result<()> {
        self.w.extend_from_slice(b);
        self.flush()
    }

    pub(crate) fn flush(&mut self) -> Result<()> {
        if !self.w.is_empty() {
            self.sink.write_all(&self.w)?;
            self.w.clear();
        }
        Ok(())
    }

    pub(crate) fn send_parameter_status(&mut self, key: &str, val: &str) -> Result<()> {
        let mut b = Vec::new();
        append_cstring(&mut b, key);
        append_cstring(&mut b, val);
        self.send(MSG_PARAMETER_STATUS, &b)
    }

    pub(crate) fn send_ready_for_query(&mut self) -> Result<()> {
        self.send(MSG_READY_FOR_QUERY, b"I")
    }

    /// A RowDescription followed by the DataRows, as Simple Query requires.
    pub(crate) fn send_result_set(&mut self, rs: &ResultSet) -> Result<()> {
        let oids: Vec<Oid> = rs
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| oid_for_column(col, &rs.rows, i))
            .collect();
        self.send_row_description(&rs.columns, &oids)?;
        // Simple Query always uses text format for results, where every value has a form.
        self.send_data_rows(&rs.rows, &oids, &[]).map(|_| ())
    }

    /// Describes result columns. All columns use text format.
    pub(crate) fn send_row_description(&mut self, cols: &[Column], oids: &[Oid]) -> Result<()> {
        let mut t = Vec::new();
        append_int16(&mut t, cols.len() as i32);
        for (i, col) in cols.iter().enumerate() {
            append_cstring(&mut t, &col.name);
            append_int32(&mut t, 0); // table OID
            append_int16(&mut t, 0); // column attribute number
            append_uint32(&mut t, oids[i].get()); // type OID
            append_int16(&mut t, i32::from(type_size(oids[i])));
            // varchar(n) says n: clients read a column's length from here.
            let typmod = if col.decl_type.is_empty() {
                -1
            } else {
                declared_typmod(&col.decl_type)
            };
            append_int32(&mut t, typmod);
            append_int16(&mut t, 0); // format code: 0 = text
        }
        self.send(MSG_ROW_DESCRIPTION, &t)
    }

    /// Describes result columns, each in the format it goes out in: a binary cursor's
    /// rows are binary over the simple protocol too.
    pub(crate) fn send_row_description_formats(
        &mut self,
        cols: &[Column],
        oids: &[Oid],
        formats: &[Format],
    ) -> Result<()> {
        let mut t = Vec::new();
        append_int16(&mut t, cols.len() as i32);
        for (i, col) in cols.iter().enumerate() {
            append_cstring(&mut t, &col.name);
            append_int32(&mut t, 0);
            append_int16(&mut t, 0);
            append_uint32(&mut t, oids[i].get());
            append_int16(&mut t, -1);
            append_int32(&mut t, -1);
            append_int16(&mut t, i32::from(format_for(formats, i) == Format::Binary));
        }
        self.send(MSG_ROW_DESCRIPTION, &t)
    }

    /// One DataRow per row, each column in the format the client requested. Stops at a
    /// value with no form in that format and answers why; the rows before it are sent,
    /// as they are when any statement fails on the way through its results.
    pub(crate) fn send_data_rows(
        &mut self,
        rows: &[Vec<Value>],
        oids: &[Oid],
        formats: &[Format],
    ) -> Result<Option<NoBinaryForm>> {
        for row in rows {
            let mut d = Vec::new();
            append_int16(&mut d, row.len() as i32);
            for (i, cell) in row.iter().enumerate() {
                match encode_value(format_for(formats, i), oids[i], cell) {
                    Err(why) => return Ok(Some(why)),
                    Ok(None) => append_int32(&mut d, -1), // NULL
                    Ok(Some(enc)) => {
                        append_int32(&mut d, enc.len() as i32);
                        d.extend_from_slice(&enc);
                    }
                }
            }
            self.send(MSG_DATA_ROW, &d)?;
        }
        Ok(None)
    }

    /// Advertises n parameters of unspecified type (OID 0).
    pub(crate) fn send_parameter_description(&mut self, n: usize) -> Result<()> {
        let mut b = Vec::new();
        append_int16(&mut b, n as i32);
        for _ in 0..n {
            append_uint32(&mut b, 0);
        }
        self.send(MSG_PARAMETER_DESCRIPTION, &b)
    }

    /// Advertises a parameter per OID, 0 for one left to the client.
    pub(crate) fn send_parameter_types(&mut self, oids: &[Oid]) -> Result<()> {
        let mut b = Vec::new();
        append_int16(&mut b, oids.len() as i32);
        for oid in oids {
            append_uint32(&mut b, oid.get());
        }
        self.send(MSG_PARAMETER_DESCRIPTION, &b)
    }

    pub(crate) fn send_command_complete(&mut self, tag: &str) -> Result<()> {
        let mut b = Vec::with_capacity(tag.len() + 1);
        append_cstring(&mut b, tag);
        self.send(MSG_COMMAND_COMPLETE, &b)
    }

    pub(crate) fn send_error(&mut self, code: SqlState, message: &str) -> Result<()> {
        self.send_error_severity("ERROR", code, message)
    }

    pub(crate) fn send_fatal(&mut self, code: SqlState, message: &str) -> Result<()> {
        self.send_error_severity("FATAL", code, message)
    }

    /// A NOTICE, with the class Postgres gives one that reports success.
    pub(crate) fn send_notice(&mut self, message: &str) -> Result<()> {
        let b = error_fields("NOTICE", SqlState::SUCCESS, message);
        self.send(MSG_NOTICE_RESPONSE, &b)
    }

    /// A NOTICE with a class of its own: what Postgres says when it skips.
    pub(crate) fn send_notice_state(&mut self, code: SqlState, message: &str) -> Result<()> {
        let b = error_fields("NOTICE", code, message);
        self.send(MSG_NOTICE_RESPONSE, &b)
    }

    /// A WARNING: the statement goes on, with something said.
    pub(crate) fn send_warning(&mut self, code: SqlState, message: &str) -> Result<()> {
        let b = error_fields("WARNING", code, message);
        self.send(MSG_NOTICE_RESPONSE, &b)
    }

    /// An ERROR with a HINT after its message.
    pub(crate) fn send_error_hint(&mut self, code: SqlState, message: &str, hint: &str) -> Result<()> {
        let mut b = error_fields("ERROR", code, message);
        b.pop();
        b.push(b'H');
        append_cstring(&mut b, hint);
        b.push(0);
        self.errored = true;
        self.send(MSG_ERROR_RESPONSE, &b)
    }

    /// An ERROR as Postgres sends err: its SQLSTATE, message, HINT and position.
    pub(crate) fn send_error_of(&mut self, err: &crate::error::Error, message: &str) -> Result<()> {
        let mut b = error_fields("ERROR", err.sql_state(), message);
        b.pop();
        if let Some(hint) = err.hint() {
            b.push(b'H');
            append_cstring(&mut b, hint);
        }
        if let Some(p) = err.position() {
            b.push(b'P');
            append_cstring(&mut b, &p.to_string());
        }
        b.push(0);
        self.errored = true;
        self.send(MSG_ERROR_RESPONSE, &b)
    }

    fn send_error_severity(&mut self, severity: &str, code: SqlState, message: &str) -> Result<()> {
        let b = error_fields(severity, code, message);
        self.errored = true;
        self.send(MSG_ERROR_RESPONSE, &b)
    }
}

/// The body of an ErrorResponse or a NoticeResponse.
fn error_fields(severity: &str, code: SqlState, message: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(b'S');
    append_cstring(&mut b, severity);
    b.push(b'C');
    append_cstring(&mut b, code.as_str());
    b.push(b'M');
    append_cstring(&mut b, message);
    b.push(0); // terminator
    b
}

fn parse_startup_params(b: &[u8]) -> HashMap<String, String> {
    let mut params = HashMap::new();
    let parts = split_cstrings(b);
    let mut i = 0;
    while i + 1 < parts.len() {
        params.insert(parts[i].clone(), parts[i + 1].clone());
        i += 2;
    }
    params
}

/// Splits a buffer of NUL-terminated strings.
fn split_cstrings(b: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, ch) in b.iter().enumerate() {
        if *ch == 0 {
            out.push(text_from_bytes(&b[start..i]).into_owned());
            start = i + 1;
        }
    }
    out
}

/// Builds the CommandComplete tag, e.g. "SELECT 3", "INSERT 0 1".
pub(crate) fn i32_be(v: i32) -> [u8; 4] {
    v.to_be_bytes()
}

pub(crate) fn append_int16(b: &mut Vec<u8>, v: i32) {
    b.extend_from_slice(&(v as u16).to_be_bytes());
}

pub(crate) fn append_uint64(b: &mut Vec<u8>, u: u64) {
    b.extend_from_slice(&u.to_be_bytes());
}

fn append_int32(b: &mut Vec<u8>, v: i32) {
    b.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn append_uint32(b: &mut Vec<u8>, u: u32) {
    b.extend_from_slice(&u.to_be_bytes());
}

pub(crate) fn append_cstring(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&text_to_bytes(s));
    b.push(0);
}

/// pg_type.typlen of a type, as a RowDescription carries it: -1 for one of variable
/// length.
fn type_size(oid: Oid) -> i16 {
    match oid.get() {
        16 | 18 => 1,                       // bool, char
        21 => 2,                            // int2
        23 | 26 | 700 | 1082 => 4,          // int4, oid, float4, date
        20 | 701 | 1083 | 1114 | 1184 => 8, // int8, float8, time, timestamp, timestamptz
        2950 => 16,                         // uuid
        19 => 64,                           // name
        _ => -1,
    }
}
