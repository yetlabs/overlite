use crate::common::*;

#[test]
fn extended_select_constant() {
    let conn = connect_extended(&start_server());

    let n: i64 = conn.query_row("SELECT 1", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

#[test]
fn extended_parametrized() {
    let conn = connect_extended(&start_server());

    conn.exec(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
        &[],
    )
    .unwrap();
    conn.exec("INSERT INTO users (name, age) VALUES ($1, $2)", &[&"alice", &30])
        .unwrap();
    conn.exec("INSERT INTO users (name, age) VALUES ($1, $2)", &[&"bob", &25])
        .unwrap();

    // Parametrized SELECT through the extended protocol.
    let rows = conn
        .query("SELECT name, age FROM users WHERE age >= $1 ORDER BY age", &[&26])
        .unwrap();
    let got: Vec<(String, i64)> = rows.collect_rows().unwrap();
    assert_eq!(got, vec![("alice".to_string(), 30)]);
}

#[test]
fn extended_cast() {
    // Honors the original breadcrumb: $1::int must work (needs dialect rewrite
    // of the Postgres :: cast into SQLite CAST()).
    let conn = connect_extended(&start_server());

    let n: i64 = conn.query_row("SELECT $1::int + 1", &[&7]).scan().unwrap();
    assert_eq!(n, 8);
}

#[test]
fn extended_prepared_reuse() {
    let conn = connect_extended(&start_server());

    // Explicitly prepare, then execute several times: exercises statement
    // caching / reuse across Bind+Execute cycles.
    conn.prepare("double", "SELECT $1 * 2").unwrap();

    for input in [1, 5, 21] {
        let out: i64 = conn.query_row("double", &[&input]).scan().unwrap();
        assert_eq!(out, (input * 2) as i64);
    }
}

#[test]
fn extended_insert_returning() {
    let conn = connect_extended(&start_server());

    conn.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
        .unwrap();

    let (id, v): (i64, String) = conn
        .query_row("INSERT INTO t (v) VALUES ($1) RETURNING id, v", &[&"hello"])
        .scan()
        .unwrap();
    assert_eq!(id, 1);
    assert_eq!(v, "hello");
}
