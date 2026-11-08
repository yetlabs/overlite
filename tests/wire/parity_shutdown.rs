use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::common::*;

const SIGINT: i32 = libc::SIGINT;
const SIGTERM: i32 = libc::SIGTERM;

fn test_db(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("test.db")
}

#[test]
fn parity_shutdown_clients_connected() {
    for sig in [SIGTERM, SIGINT] {
        println!("signal {sig}");
        let dir = temp_dir();
        let db = test_db(&dir);
        let addr = start_proc(&db);

        let idle = connect(&addr);
        must_exec(&idle, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        must_exec(&idle, "INSERT INTO t VALUES (1,'a'),(2,'b')");

        // An open transaction must not survive the shutdown.
        let intx = connect(&addr);
        must_exec(&intx, "BEGIN");
        must_exec(&intx, "INSERT INTO t VALUES (3,'uncommitted')");

        // Neither must a statement still running hold the process up.
        let busy = connect(&addr);
        let (busy_tx, busy_err) = mpsc::channel();
        thread::spawn(move || {
            let r = busy
                .query_row(
                    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT count(*) FROM c",
                    &[],
                )
                .scan::<i64>();
            let _ = busy_tx.send(r);
        });
        thread::sleep(Duration::from_millis(300));

        let (exit, took) = signal_and_wait(&addr, sig);
        println!("exit={exit:?} took={took:?} wal={}", wal_size(&db));
        exit.expect("a signal is a clean exit");
        assert!(took < Duration::from_secs(5), "{took:?}");

        let res = busy_err
            .recv_timeout(Duration::from_secs(5))
            .expect("client of a running statement never saw the end");
        println!("running statement ended with: {res:?}");
        res.unwrap_err();

        idle.set_read_timeout(Duration::from_secs(3));
        let qerr = idle.query_row("SELECT 1", &[]).scan::<i64>().unwrap_err();
        println!("idle client next query: {qerr}");
        assert!(
            !qerr.is_timeout(),
            "the idle client must be disconnected, not left hanging"
        );

        let (integrity, rows) = offline_check(&db, "SELECT count(*) FROM t");
        assert_eq!(integrity, "ok");
        assert_eq!(rows, 2, "committed rows stay, the open transaction does not");
    }
}

// Everything the server acknowledged before the signal must be on disk after.
#[test]
fn parity_shutdown_keeps_acknowledged_writes() {
    let dir = temp_dir();
    let db = test_db(&dir);
    let addr = start_proc(&db);
    let c = connect(&addr);
    must_exec(&c, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");

    let writer = thread::spawn(move || {
        let mut last = 0i64;
        for i in 1.. {
            if c.exec("INSERT INTO t VALUES ($1, 'x')", &[&i]).is_err() {
                break;
            }
            last = i;
        }
        last
    });
    thread::sleep(Duration::from_millis(500));
    let (exit, took) = signal_and_wait(&addr, SIGTERM);
    let last = writer.join().unwrap();
    println!("exit={exit:?} took={took:?} acked={last} wal={}", wal_size(&db));
    exit.unwrap();
    let (integrity, rows) = offline_check(&db, "SELECT count(*) FROM t");
    assert_eq!(integrity, "ok");
    assert!(rows >= last, "{rows} < {last}");
    assert!(rows <= last + 1, "{rows} > {last} + 1");
}

// With no client left the WAL is folded into the database, as closing the last
// connection does.
#[test]
fn parity_shutdown_no_clients_checkpoints() {
    let dir = temp_dir();
    let db = test_db(&dir);
    let addr = start_proc(&db);
    let c = connect(&addr);
    must_exec(&c, "CREATE TABLE t (id INTEGER PRIMARY KEY)");
    must_exec(&c, "INSERT INTO t VALUES (1),(2),(3)");
    c.close().unwrap();
    thread::sleep(Duration::from_millis(200));

    let (exit, took) = signal_and_wait(&addr, SIGTERM);
    println!("exit={exit:?} took={took:?} wal={}", wal_size(&db));
    exit.unwrap();
    assert_eq!(wal_size(&db), -1, "the WAL is gone after a clean close");
    let (integrity, rows) = offline_check(&db, "SELECT count(*) FROM t");
    assert_eq!(integrity, "ok");
    assert_eq!(rows, 3);
}

// A client in the middle of COPY FROM STDIN loses that COPY, and no more.
#[test]
fn parity_shutdown_during_copy() {
    let dir = temp_dir();
    let db = test_db(&dir);
    let addr = start_proc(&db);
    let c = connect(&addr);
    must_exec(&c, "CREATE TABLE t (id INTEGER)");
    must_exec(&c, "INSERT INTO t VALUES (1)");

    let (pr, mut pw) = std::io::pipe().unwrap();
    let copy = thread::spawn(move || {
        let _ = c.copy_from(pr, "COPY t FROM STDIN");
    });
    std::io::Write::write_all(&mut pw, b"2\n3\n").unwrap();
    thread::sleep(Duration::from_millis(300));

    let (exit, took) = signal_and_wait(&addr, SIGTERM);
    println!("exit={exit:?} took={took:?} wal={}", wal_size(&db));
    exit.unwrap();
    assert!(took < Duration::from_secs(5), "{took:?}");
    drop(pw);
    copy.join().unwrap();
    let (integrity, rows) = offline_check(&db, "SELECT count(*) FROM t");
    assert_eq!(integrity, "ok");
    assert_eq!(rows, 1);
}

// A client that stopped reading must not hold the shutdown: the server blocks
// writing its result and is closed for writes too.
#[test]
fn parity_shutdown_client_not_reading() {
    let dir = temp_dir();
    let db = test_db(&dir);
    let addr = start_proc(&db);
    let mut c = RawConn::dial(&addr).unwrap();
    // Startup, then a query far larger than the socket buffers, then silence.
    c.send_startup(&[("user", "overlite"), ("database", "test")]);
    c.flush().unwrap();
    thread::sleep(Duration::from_millis(200));
    c.send_query(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 3000000) SELECT x, 'padding padding padding padding' FROM c",
    );
    c.flush().unwrap();
    thread::sleep(Duration::from_millis(1500));

    let (exit, took) = signal_and_wait(&addr, SIGTERM);
    println!("exit={exit:?} took={took:?} wal={}", wal_size(&db));
    exit.unwrap();
    assert!(took < Duration::from_secs(3), "{took:?}");
}
