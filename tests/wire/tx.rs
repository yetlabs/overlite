use crate::common::*;

#[test]
fn tx_commit_over_wire() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (n INTEGER)");

    let tx = conn.begin().unwrap();
    tx.exec("INSERT INTO t (n) VALUES (1)", &[]).unwrap();
    tx.commit().unwrap();

    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

#[test]
fn tx_rollback_over_wire() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (n INTEGER)");
    must_exec(&conn, "INSERT INTO t (n) VALUES (1)");

    let tx = conn.begin().unwrap();
    tx.exec("INSERT INTO t (n) VALUES (2)", &[]).unwrap();
    tx.rollback().unwrap();

    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1); // the rolled-back insert is gone
}

// A failed statement inside a transaction aborts it: further statements are
// rejected until ROLLBACK, and the whole transaction's effects are discarded.
#[test]
fn tx_aborted_state() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (n INTEGER)");

    conn.exec("BEGIN", &[]).unwrap();
    conn.exec("INSERT INTO t (n) VALUES (1)", &[]).unwrap();

    // Trigger an error → transaction becomes aborted.
    conn.exec("SELECT * FROM does_not_exist", &[]).unwrap_err();

    // Subsequent statements are rejected until the block ends.
    let err = conn.exec("INSERT INTO t (n) VALUES (2)", &[]).unwrap_err();
    assert_contains!(err, "aborted");

    conn.exec("ROLLBACK", &[]).unwrap();

    // Nothing from the aborted transaction persisted.
    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 0);
}
