use crate::common::*;

// These cover the session/transaction/SHOW statements a JDBC driver fires
// during connection setup — the difference between DBeaver connecting or not.

#[test]
fn utility_simple_protocol() {
    let conn = connect(&start_server());

    // SET (what pgjdbc sends on connect) must succeed as a no-op.
    conn.exec("SET extra_float_digits = 3", &[]).unwrap();
    conn.exec("SET application_name = 'DBeaver'", &[]).unwrap();
    conn.exec("RESET ALL", &[]).unwrap();

    // Transaction control is accepted (no-op today).
    conn.exec("BEGIN", &[]).unwrap();
    conn.exec("COMMIT", &[]).unwrap();
}

#[test]
fn utility_show() {
    let conn = connect(&start_server());

    let search_path: String = conn.query_row("SHOW search_path", &[]).scan().unwrap();
    assert_contains!(search_path, "public");

    let iso: String = conn.query_row("SHOW TRANSACTION ISOLATION LEVEL", &[]).scan().unwrap();
    assert_eq!(iso, "read committed");

    let scs: String = conn.query_row("SHOW standard_conforming_strings", &[]).scan().unwrap();
    assert_eq!(scs, "on");
}

#[test]
fn utility_extended_protocol() {
    // The same statements through the extended protocol (Parse/Bind/Describe/
    // Execute), which pgjdbc uses for most queries.
    let conn = connect_extended(&start_server());

    conn.exec("SET extra_float_digits = 3", &[]).unwrap();

    let v: String = conn.query_row("SHOW server_version", &[]).scan().unwrap();
    assert_eq!(v, "16.0");
}

#[test]
fn utility_current_setting() {
    let conn = connect_extended(&start_server());
    let v: String = conn
        .query_row("SELECT current_setting('standard_conforming_strings')", &[])
        .scan()
        .unwrap();
    assert_eq!(v, "on");
}

// intercept_utility and has_function_keyword: these words name routines only where
// the routine's kind goes, not when they name a table or an index.
#[test]
fn routine_words_as_names() {
    let conn = connect_extended(&start_server());
    for name in ["procedure", "aggregate", "function"] {
        must_exec(&conn, &format!("CREATE TABLE {name} (id int)"));
        must_exec(&conn, &format!("INSERT INTO {name} VALUES (1)"));
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {name}"), &[])
            .scan()
            .unwrap();
        assert_eq!(n, 1, "{name}");
    }
    must_exec(&conn, "CREATE INDEX procedure_i ON procedure (id)");
    for name in ["procedure", "aggregate", "function"] {
        must_exec(&conn, &format!("DROP TABLE {name}"));
        conn.exec(&format!("SELECT * FROM {name}"), &[])
            .expect_err("the table is gone");
    }
    must_exec(&conn, "CREATE TABLE ix (a int)");
    must_exec(&conn, "CREATE INDEX aggregate ON ix (a)");
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'aggregate' AND type = 'index'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        conn.exec("CREATE PROCEDURE p() LANGUAGE sql AS 'SELECT 1'", &[])
            .unwrap()
            .as_str(),
        "CREATE PROCEDURE"
    );
    assert_eq!(
        conn.exec("CREATE OR REPLACE AGGREGATE a (int) (sfunc = x, stype = int)", &[])
            .unwrap()
            .as_str(),
        "CREATE AGGREGATE"
    );
}
