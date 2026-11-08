use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use crate::common::*;

// Migration tools take an advisory lock so two deploys never migrate at once;
// Prisma's `migrate deploy` failed outright on "no such function:
// pg_advisory_lock". The locks are now held by the server, across connections.
#[test]
fn advisory_locks() {
    let addr = start_server();
    let (a, b, c) = (
        connect_extended(&addr),
        connect(&addr),
        Arc::new(connect_extended(&addr)),
    );

    let try_lock = |sql: &str| -> bool { b.query_row(sql, &[]).scan().unwrap() };

    // A holds 72707369, as Prisma takes it: B can't have it until A lets go.
    a.exec("SELECT pg_advisory_lock(72707369)", &[]).unwrap();
    assert!(!try_lock("SELECT pg_try_advisory_lock(72707369)"));
    assert!(
        try_lock("SELECT pg_try_advisory_lock(1, 2)"),
        "two keys are another lock"
    );
    assert!(!try_lock("SELECT pg_advisory_unlock(72707369)"), "B does not hold it");

    // C waits for it, by a bound parameter as migrate asks, until A
    // unlocks.
    let (send, got) = mpsc::channel();
    let waiter = Arc::clone(&c);
    thread::spawn(move || {
        let res = waiter.exec("SELECT pg_advisory_lock($1)", &[&72707369i64]);
        let _ = send.send(res);
    });
    match got.recv_timeout(Duration::from_millis(100)) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        _ => panic!("C took a lock A holds"),
    }
    let unlocked: bool = a.query_row("SELECT pg_advisory_unlock(72707369)", &[]).scan().unwrap();
    assert!(unlocked);
    got.recv_timeout(Duration::from_secs(2))
        .expect("C never got the lock")
        .unwrap();

    // The column is named after the function, as in Postgres.
    let rows = a.query("SELECT pg_try_advisory_lock(72707369)", &[]).unwrap();
    assert_eq!(rows.field_descriptions()[0].name, "pg_try_advisory_lock");
    let row = rows.iter().next().unwrap();
    let held: bool = row.scan().unwrap();
    assert!(!held, "C holds it now");

    // A transaction-level lock goes when the transaction does.
    let tx = a.begin().unwrap();
    tx.exec("SELECT pg_advisory_xact_lock(7)", &[]).unwrap();
    assert!(!try_lock("SELECT pg_try_advisory_lock(7)"));
    tx.commit().unwrap();
    assert!(try_lock("SELECT pg_try_advisory_lock(7)"));

    // Shared locks share; an exclusive one waits for them all.
    assert!(try_lock("SELECT pg_try_advisory_lock_shared(9)"));
    let shared: bool = a
        .query_row("SELECT pg_try_advisory_lock_shared(9)", &[])
        .scan()
        .unwrap();
    assert!(shared);
    let shared: bool = a.query_row("SELECT pg_try_advisory_lock(9)", &[]).scan().unwrap();
    assert!(!shared, "B shares it");

    // Closing the connection lets go of everything it held.
    c.close().unwrap();
    eventually(Duration::from_secs(2), Duration::from_millis(20), || {
        matches!(
            a.query_row("SELECT pg_try_advisory_lock(72707369)", &[]).scan::<bool>(),
            Ok(true)
        )
    });
}
