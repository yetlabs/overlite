use crate::common::*;

// A statement with nothing executable in it used to reach the engine, where the
// SQLite driver answered with no result and the counter call panicked — ending
// the process and every other session with it. pgx's Ping sends exactly such a
// statement (`-- ping`), so this hit every pgx-backed client on connect.

#[test]
fn comment_only_statements_are_empty_queries() {
    let addr = start_server();
    let conn = connect(&addr);

    for sql in ["-- ping", "--", "/* c */", "-- ping;", "/* outer /* inner */ */"] {
        conn.exec(sql, &[]).unwrap_or_else(|e| panic!("exec {sql:?}: {e}"));

        // The session survives and keeps working.
        let n: i64 = conn
            .query_row("SELECT 1", &[])
            .scan()
            .unwrap_or_else(|e| panic!("after {sql:?}: {e}"));
        assert_eq!(n, 1);
    }
}

#[test]
fn comment_only_statement_extended_protocol() {
    let conn = connect_extended(&start_server());

    conn.exec("-- ping", &[]).unwrap();

    let n: i64 = conn.query_row("SELECT 1", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

// ping_comment is the regression this is really about: a pool probes a
// connection with `-- ping`, so opening one used to kill the server.
#[test]
fn ping_comment() {
    let addr = start_server();

    let db = connect_config(&addr, &Config::new()).expect("sql.Open pgx");

    db.ping().expect("Ping");

    let n: i64 = db.query_row("SELECT 1", &[]).scan().unwrap();
    assert_eq!(n, 1);

    // A second ping on a pooled connection still works.
    db.ping().unwrap();
}
