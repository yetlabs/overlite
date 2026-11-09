//! EXPLAIN needs the privileges of the statement it explains, as in Postgres (17),
//! whatever its options, and EXPLAIN ANALYZE runs the statement as a statement runs:
//! a write is held to its grants and its policies, and is done.

use crate::common::*;

fn setup() -> (Server, Conn, Conn) {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE secret (id int, s text)");
    must_exec(&admin, "INSERT INTO secret VALUES (1, 'topsecret')");
    must_exec(&admin, "CREATE TABLE pub (id int, v text)");
    must_exec(&admin, "GRANT SELECT, INSERT ON pub TO alice");
    let alice = connect_as(&addr, "alice", "a");
    (addr, admin, alice)
}

/// The plan, or the SQLSTATE, through the simple protocol and the extended one.
fn plans(conn: &Conn, sql: &str) -> [(&'static str, std::result::Result<String, String>); 2] {
    let text = |rows: &[Vec<Option<Vec<u8>>>]| {
        rows.iter()
            .map(|r| String::from_utf8_lossy(r[0].as_deref().unwrap_or_default()).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let simple = conn
        .pg_exec(sql)
        .map(|rs| rs.last().map(|r| text(&r.rows)).unwrap_or_default())
        .map_err(|e| e.code().to_string());
    let extended = conn
        .exec_params::<&[u8]>(sql, &[], &[], &[], &[])
        .map(|r| text(&r.rows))
        .map_err(|e| e.code().to_string());
    [("simple", simple), ("extended", extended)]
}

#[track_caller]
fn refused(conn: &Conn, sql: &str) {
    for (path, got) in plans(conn, sql) {
        assert_eq!(got, Err("42501".to_string()), "{path}: {sql}");
    }
}

fn count(admin: &Conn, table: &str) -> i64 {
    admin
        .query_row(&format!("SELECT count(*) FROM {table}"), &[])
        .scan()
        .unwrap()
}

// The report: EXPLAIN was never checked, and EXPLAIN ANALYZE ran what it explained.
#[test]
fn explain_needs_the_statements_privileges() {
    let (_addr, admin, alice) = setup();
    must_exec(
        &alice,
        "CREATE FUNCTION peek() RETURNS text LANGUAGE sql AS 'SELECT s FROM secret LIMIT 1'",
    );
    for sql in [
        "EXPLAIN SELECT * FROM secret",
        "EXPLAIN VERBOSE SELECT s FROM secret",
        "EXPLAIN ANALYZE SELECT * FROM secret",
        "EXPLAIN ANALYSE VERBOSE SELECT * FROM secret",
        "EXPLAIN (ANALYZE) SELECT * FROM secret",
        "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF) SELECT * FROM secret",
        "EXPLAIN (FORMAT JSON) SELECT * FROM secret",
        "EXPLAIN (ANALYZE true, FORMAT YAML, BUFFERS) SELECT * FROM secret",
        "EXPLAIN (ANALYZE, FORMAT XML) SELECT id FROM pub WHERE v IN (SELECT s FROM secret)",
        "explain analyze select count(*) from secret",
        "EXPLAIN (ANALYZE) SELECT * FROM pub WHERE v <> peek()",
        "EXPLAIN ANALYZE INSERT INTO secret VALUES (2, 'planted')",
        "EXPLAIN (ANALYZE, FORMAT JSON) UPDATE secret SET s = 'gone'",
        "EXPLAIN ANALYZE DELETE FROM secret",
        "EXPLAIN UPDATE secret SET s = 'gone'",
        "EXPLAIN DELETE FROM secret",
        "EXPLAIN ANALYZE INSERT INTO pub SELECT id, s FROM secret",
        "EXPLAIN ANALYZE INSERT INTO pub VALUES (3, 'x') RETURNING (SELECT s FROM secret)",
    ] {
        refused(&alice, sql);
    }
    assert_eq!(count(&admin, "pub"), 0);
    assert_eq!(
        query_column(&admin, "SELECT id || s FROM secret ORDER BY id", 0),
        ["1topsecret"]
    );

    // With the grant, the same EXPLAIN answers.
    must_exec(&admin, "GRANT SELECT ON secret TO alice");
    for (path, got) in plans(&alice, "EXPLAIN (ANALYZE, COSTS OFF) SELECT * FROM secret") {
        assert!(got.as_ref().is_ok_and(|p| p.contains("secret")), "{path}: {got:?}");
    }
}

// EXPLAIN ANALYZE runs a write, as Postgres does, and holds it to everything the
// write itself is held to; EXPLAIN alone does not run it.
#[test]
fn explain_analyze_runs_a_write_as_a_statement_runs() {
    let (_addr, admin, alice) = setup();
    must_exec(&alice, "EXPLAIN INSERT INTO pub VALUES (1, 'plan only')");
    assert_eq!(count(&admin, "pub"), 0);
    must_exec(&alice, "EXPLAIN ANALYZE INSERT INTO pub VALUES (1, 'simple')");
    assert_eq!(count(&admin, "pub"), 1);
    alice
        .exec_params::<&[u8]>(
            "EXPLAIN (ANALYZE) INSERT INTO pub VALUES (2, 'extended')",
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(count(&admin, "pub"), 2);
    // Parse alone runs nothing; each Execute runs it once.
    alice
        .prepare("ea", "EXPLAIN ANALYZE INSERT INTO pub VALUES (3, 'prepared')")
        .unwrap();
    assert_eq!(count(&admin, "pub"), 2);
    // UPDATE was not granted.
    refused(&alice, "EXPLAIN ANALYZE UPDATE pub SET v = 'u'");
    refused(&alice, "EXPLAIN UPDATE pub SET v = 'u'");

    // A policy holds an EXPLAIN ANALYZE as it holds the write.
    must_exec(&admin, "CREATE TABLE r (owner text, v text)");
    must_exec(&admin, "GRANT SELECT, INSERT ON r TO alice");
    must_exec(&admin, "ALTER TABLE r ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY own ON r USING (owner = current_user)");
    for sql in [
        "EXPLAIN ANALYZE INSERT INTO r VALUES ('bob', 'forged')",
        "EXPLAIN (ANALYZE, FORMAT JSON) INSERT INTO r SELECT 'bob', 'forged'",
    ] {
        for (path, got) in plans(&alice, sql) {
            assert_eq!(got, Err("42501".to_string()), "{path}: {sql}");
        }
    }
    assert_eq!(count(&admin, "r"), 0);
    must_exec(&alice, "EXPLAIN ANALYZE INSERT INTO r VALUES ('alice', 'mine')");
    assert_eq!(count(&admin, "r"), 1);
    // A read under ANALYZE sees what the policies let it.
    must_exec(&admin, "INSERT INTO r VALUES ('bob', 'b1'), ('bob', 'b2')");
    let plan = query_column(&alice, "EXPLAIN (ANALYZE, TIMING OFF, COSTS OFF) SELECT * FROM r", 0).join("\n");
    assert!(plan.contains("rows=1 "), "{plan}");
}
