use std::thread;
use std::time::Duration;

use crate::common::*;

// With one dedicated connection per client, a client holding an open
// transaction must not block another client's reads (this deadlocked under the
// old single-connection engine).
#[test]
fn concurrent_read_during_write_transaction() {
    let addr = start_server();
    let a = connect(&addr);
    let b = connect(&addr);

    must_exec(&a, "CREATE TABLE t (n INTEGER)");
    must_exec(&a, "INSERT INTO t VALUES (1)"); // committed

    // A opens a transaction and writes, but does not commit yet.
    let tx_a = a.begin().unwrap();
    tx_a.exec("INSERT INTO t VALUES (2)", &[]).unwrap();

    // B must read concurrently within a short timeout — no blocking on A's tx.
    b.set_read_timeout(Duration::from_secs(3));
    let n: i64 = b
        .query_row("SELECT count(*) FROM t", &[])
        .scan()
        .expect("read blocked by another client's open transaction");
    assert_eq!(n, 1, "B sees only committed rows, not A's uncommitted insert");
    b.set_read_timeout(Duration::from_secs(120));

    tx_a.commit().unwrap();
    let n: i64 = b.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 2);
}

// Many clients writing and reading at once all make progress.
#[test]
fn many_concurrent_clients() {
    let addr = start_server();
    let setup = connect(&addr);
    must_exec(&setup, "CREATE TABLE t (id INTEGER PRIMARY KEY, who INTEGER)");

    const CLIENTS: i64 = 20;
    let cfg = Config::new().user("postgres", "");

    let mut clients = Vec::new();
    for i in 0..CLIENTS {
        let (addr, cfg) = (addr.clone(), cfg.clone());
        clients.push(thread::spawn(move || -> std::result::Result<(), String> {
            let c = connect_config(&addr, &cfg).map_err(|e| e.to_string())?;
            for _ in 0..5 {
                c.exec("INSERT INTO t (who) VALUES ($1)", &[&i])
                    .map_err(|e| e.to_string())?;
            }
            let n: i64 = c
                .query_row("SELECT count(*) FROM t WHERE who = $1", &[&i])
                .scan()
                .map_err(|e| e.to_string())?;
            if n != 5 {
                return Err(format!("client {i} saw {n} of its rows, want 5"));
            }
            Ok(())
        }));
    }
    let errs: Vec<_> = clients.into_iter().map(|c| c.join().unwrap()).collect();

    for (i, err) in errs.into_iter().enumerate() {
        err.unwrap_or_else(|e| panic!("client {i}: {e}"));
    }
    let total: i64 = setup.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(total, CLIENTS * 5);
}

// A writer waiting for another client's transaction goes on as soon as it commits.
// It waited in SQLite's busy handler, which sleeps in steps that grow to 100ms, so
// a writer that had waited a second went on up to 100ms after the lock was free (and
// eight writers spent most of their time asleep); writers of one server now wait
// their turn on the server's own gate, which wakes the next one at the commit.
#[test]
fn a_waiting_writer_goes_on_at_the_commit() {
    let addr = start_server();
    let setup = connect(&addr);
    must_exec(&setup, "CREATE TABLE w (id int PRIMARY KEY, v int)");
    must_exec(&setup, "INSERT INTO w VALUES (1, 0)");
    for _ in 0..3 {
        let a = connect(&addr);
        let tx = a.begin().unwrap();
        tx.exec("UPDATE w SET v = v + 1 WHERE id = 1", &[]).unwrap();
        let waiter = {
            let addr = addr.clone();
            thread::spawn(move || {
                let b = connect(&addr);
                must_exec(&b, "UPDATE w SET v = v + 1 WHERE id = 1");
                std::time::Instant::now()
            })
        };
        // Long enough for the busy handler's sleeps to have reached 100ms.
        thread::sleep(Duration::from_millis(1200));
        let committed = std::time::Instant::now();
        tx.commit().unwrap();
        let done = waiter.join().unwrap();
        let late = done.saturating_duration_since(committed);
        assert!(
            late < Duration::from_millis(30),
            "the waiting writer went on {late:?} after the commit"
        );
    }
    assert_eq!(scan_str(&setup, "SELECT v FROM w"), "6");
}
