use crate::common::*;

#[test]
fn connect_and_select_constant() {
    let conn = connect(&start_server());

    let n: i64 = conn.query_row("SELECT 1", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

#[test]
fn create_insert_select_over_wire() {
    let conn = connect(&start_server());

    conn.exec(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
        &[],
    )
    .unwrap();

    let tag = conn
        .exec("INSERT INTO users (name, age) VALUES ('alice', 30)", &[])
        .unwrap();
    assert_eq!(tag.rows_affected(), 1);

    conn.exec("INSERT INTO users (name, age) VALUES ('bob', 25)", &[])
        .unwrap();

    let rows = conn.query("SELECT id, name, age FROM users ORDER BY id", &[]).unwrap();
    let got: Vec<(i64, String, i64)> = rows.collect_rows().unwrap();

    let want = vec![(1, "alice".to_string(), 30), (2, "bob".to_string(), 25)];
    assert_eq!(got, want);
}

#[test]
fn null_over_wire() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE TABLE t (a TEXT, b INTEGER)");
    must_exec(&conn, "INSERT INTO t (a, b) VALUES (NULL, 5)");

    let (a, b): (Option<String>, i64) = conn.query_row("SELECT a, b FROM t", &[]).scan().unwrap();
    assert_eq!(a, None);
    assert_eq!(b, 5);
}

#[test]
fn query_error() {
    let conn = connect(&start_server());
    // A query against a missing table must surface as an error, and the
    // connection must remain usable afterwards.
    conn.exec("SELECT * FROM nope", &[]).unwrap_err();

    let n: i64 = conn
        .query_row("SELECT 42", &[])
        .scan()
        .expect("connection must stay usable after a query error");
    assert_eq!(n, 42);
}
