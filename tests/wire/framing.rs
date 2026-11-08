//! A length field is a claim, not a fact: what it may cost the server is bounded the
//! way Postgres bounds it, before a client has proved anything.

use crate::common::*;
use std::time::{Duration, Instant};

const QUICK: Duration = Duration::from_secs(2);

/// Whether the server hung up by itself, without an answer, within `QUICK`.
fn closed_silently(fe: &mut RawConn) -> bool {
    fe.set_read_timeout(Some(QUICK)).unwrap();
    matches!(fe.read_byte(), Err(e) if !e.is_timeout())
}

fn raw(server: &Server, bytes: &[u8]) -> RawConn {
    let mut fe = RawConn::dial(server).unwrap();
    fe.send_bytes(bytes);
    fe.flush().unwrap();
    fe
}

fn logged_in(server: &Server) -> RawConn {
    let mut fe = RawConn::dial(server).unwrap();
    fe.startup("postgres", "test", "").unwrap();
    fe
}

fn typed(kind: u8, length: i32, body: &[u8]) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend_from_slice(&length.to_be_bytes());
    b.extend_from_slice(body);
    b
}

// Postgres closes without a word: invalid length of startup packet is a COMMERROR.
#[test]
fn startup_length_past_the_limit_closes() {
    let server = start_server();
    for length in [i32::MAX, 0x4000_0000, 10_005] {
        let mut b = length.to_be_bytes().to_vec();
        b.extend_from_slice(b"abc");
        let started = Instant::now();
        assert!(closed_silently(&mut raw(&server, &b)), "length {length}");
        assert!(started.elapsed() < QUICK, "length {length}");
    }
    connect(&server).ping().unwrap();
}

// The limit is on the packet past its length word, as in Postgres.
#[test]
fn startup_length_at_the_limit_waits() {
    let server = start_server();
    let mut b = 10_004i32.to_be_bytes().to_vec();
    b.extend_from_slice(b"abc");
    let mut fe = raw(&server, &b);
    fe.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    assert!(fe.read_byte().unwrap_err().is_timeout());
}

#[test]
fn startup_length_too_short_closes() {
    let server = start_server();
    for length in [-1i32, 0, 3, 7] {
        assert!(
            closed_silently(&mut raw(&server, &length.to_be_bytes())),
            "length {length}"
        );
    }
}

// Past Postgres' limit of 1 GiB less two bytes, whatever the type.
#[test]
fn message_length_past_the_limit_closes() {
    let server = start_server();
    for (kind, length) in [(b'Q', i32::MAX), (b'P', 0x3fff_ffff), (b'B', 0x7000_0000)] {
        let mut fe = logged_in(&server);
        fe.send_bytes(&typed(kind, length, b"x"));
        fe.flush().unwrap();
        assert!(closed_silently(&mut fe), "{} {length}", kind as char);
    }
    connect(&server).ping().unwrap();
}

#[test]
fn message_length_too_short_closes() {
    let server = start_server();
    let mut fe = logged_in(&server);
    fe.send_bytes(&typed(b'Q', 3, b""));
    fe.flush().unwrap();
    assert!(closed_silently(&mut fe));
}

// What the limits stop at, as Postgres answered: this much is still a message on its
// way, and a byte more is not.
#[test]
fn message_at_the_limit_waits() {
    let server = start_server();
    for (kind, length) in [(b'Q', 0x3fff_fffe), (b'S', 10_000)] {
        let mut fe = logged_in(&server);
        fe.send_bytes(&typed(kind, length, b"x"));
        fe.flush().unwrap();
        fe.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        assert!(fe.read_byte().unwrap_err().is_timeout(), "{}", kind as char);
    }
    let mut fe = logged_in(&server);
    fe.send_bytes(&typed(b'S', 10_001, b"x"));
    fe.flush().unwrap();
    assert!(closed_silently(&mut fe));
}

// A message that has no business being long: Postgres holds these to 10000 bytes.
#[test]
fn small_messages_are_held_to_the_small_limit() {
    let server = start_server();
    for kind in *b"SHXDEC" {
        let mut fe = logged_in(&server);
        fe.send_bytes(&typed(kind, 0x10_0000, b"x"));
        fe.flush().unwrap();
        assert!(closed_silently(&mut fe), "{}", kind as char);
    }
}

// Before a client has logged in, the password is the only thing it may send, and
// Postgres reads no more than 65535 bytes of it.
#[test]
fn password_message_is_bounded_before_authentication() {
    let server = start_server_env(Env::password("pw").with("POSTGRES_HOST_AUTH_METHOD", "password"));
    let mut fe = RawConn::dial(&server).unwrap();
    fe.send_startup(&[("database", "test"), ("user", "postgres")]);
    fe.flush().unwrap();
    fe.wait_for(|m| matches!(m, Backend::Authentication(3, _))).unwrap();
    fe.send_bytes(&typed(b'p', 0x1000_0000, b"x"));
    fe.flush().unwrap();
    assert!(closed_silently(&mut fe));
}

/// How much address space the process has held at its highest. A claim served by an
/// allocation shows here even when the allocator hands out pages it has not touched.
#[cfg(target_os = "linux")]
fn peak_memory(server: &Server) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{}/status", server.pid())).unwrap();
    let line = status.lines().find(|l| l.starts_with("VmPeak:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap() * 1024
}

// What a message may legally claim costs what has arrived, not what it claims.
#[cfg(target_os = "linux")]
#[test]
fn claimed_length_costs_nothing_until_the_bytes_arrive() {
    const CLAIM: i32 = 256 << 20;
    const CLIENTS: usize = 4;
    let server = start_server();
    connect(&server).ping().unwrap();
    let before = peak_memory(&server);
    let mut held = Vec::new();
    for _ in 0..CLIENTS {
        let mut fe = logged_in(&server);
        fe.send_bytes(&typed(b'Q', CLAIM, b"SELECT"));
        fe.flush().unwrap();
        held.push(fe);
    }
    connect(&server).ping().unwrap();
    let grew = peak_memory(&server) - before;
    // A thread's stack and a connection's cache each, nowhere near the claims.
    assert!(grew < 640 << 20, "address space grew {} MiB", grew >> 20);
}

// A long message still arrives whole, in however many pieces.
#[test]
fn long_message_in_pieces_is_read_whole() {
    let server = start_server();
    let mut fe = logged_in(&server);
    let text = "x".repeat(3 << 20);
    let sql = format!("SELECT length('{text}')\0");
    let bytes = typed(b'Q', sql.len() as i32 + 4, sql.as_bytes());
    for piece in bytes.chunks(1 << 20) {
        fe.send_bytes(piece);
        fe.flush().unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
    match fe
        .wait_for(|m| matches!(m, Backend::DataRow(_) | Backend::ErrorResponse(_)))
        .unwrap()
    {
        Backend::DataRow(row) => assert_eq!(row[0].as_deref(), Some(&b"3145728"[..])),
        other => panic!("{other:?}"),
    }
}

// Postgres ends the connection over a message it does not know.
#[test]
fn unknown_message_type_is_fatal() {
    let server = start_server();
    let mut fe = logged_in(&server);
    fe.send_bytes(&typed(b'?', 4, b""));
    fe.flush().unwrap();
    match fe.wait_for(|m| matches!(m, Backend::ErrorResponse(_))).unwrap() {
        Backend::ErrorResponse(e) => {
            assert_eq!((e.code.as_str(), e.severity.as_str()), ("08P01", "FATAL"));
            assert_eq!(e.message, "invalid frontend message type 63");
        }
        other => panic!("{other:?}"),
    }
    assert!(closed_silently(&mut fe));
}

// Postgres reads one string, and bytes past its end make the message malformed.
#[test]
fn query_with_a_nul_inside_is_malformed() {
    let server = start_server();
    let conn = connect(&server);
    let err = conn.pg_exec(&b"SELECT 1\0 garbage"[..]).unwrap_err();
    assert_pg_error(&err, "08P01", "invalid message format");
    conn.ping().unwrap();
}
