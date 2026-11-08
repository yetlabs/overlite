//! A SQLSTATE comes from what went wrong, not from the words of the message: a name is
//! a name, whatever it spells. The table is what Postgres 17 answered to each statement.

use crate::common::*;

fn setup() -> (Conn, Conn) {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE t (id int PRIMARY KEY, a text NOT NULL, b int UNIQUE, c int CHECK (c > 0))",
    );
    must_exec(
        &conn,
        "CREATE TABLE child (id int PRIMARY KEY, tid int REFERENCES t(id))",
    );
    must_exec(&conn, "INSERT INTO t VALUES (1, 'x', 1, 1)");
    (conn, connect_extended(&server))
}

#[track_caller]
fn both_answer(conn: &Conn, ext: &Conn, sql: &str, code: &str) {
    for c in [conn, ext] {
        let got = c.exec(sql, &[]);
        assert_eq!(
            got.as_ref().map_err(WireError::code).err(),
            Some(code),
            "{sql}: {got:?}"
        );
    }
}

#[test]
fn names_do_not_decide_a_code() {
    let (conn, ext) = setup();
    for (sql, code) in [
        ("SELECT * FROM interrupts", "42P01"),
        ("SELECT * FROM interrupted", "42P01"),
        (r#"SELECT * FROM "permission denied""#, "42P01"),
        (r#"SELECT * FROM "already exists""#, "42P01"),
        (r#"SELECT * FROM "no such column""#, "42P01"),
        (r#"SELECT * FROM "syntax error""#, "42P01"),
        (r#"SELECT * FROM "constraint ""x""#, "42P01"),
        (r#"SELECT "syntax error" FROM t"#, "42703"),
        (r#"SELECT "permission denied" FROM t"#, "42703"),
        (r#"SELECT "already exists" FROM t"#, "42703"),
        (r#"SELECT "no such table" FROM t"#, "42703"),
        (r#"INSERT INTO t ("already exists") VALUES (1)"#, "42703"),
        (r#"SELECT "permission denied"(1)"#, "42883"),
        (r#"DROP TABLE "no such column""#, "42P01"),
    ] {
        both_answer(&conn, &ext, sql, code);
    }
    must_exec(&conn, r#"CREATE TABLE "already exists" (id int)"#);
    both_answer(&conn, &ext, r#"CREATE TABLE "already exists" (id int)"#, "42P07");
    must_exec(&conn, r#"CREATE TABLE "permission denied" (id int)"#);
    both_answer(&conn, &ext, r#"CREATE TABLE "permission denied" (id int)"#, "42P07");
    both_answer(&conn, &ext, r#"SELECT nope FROM "permission denied""#, "42703");
}

#[test]
fn failing_statements() {
    let (conn, ext) = setup();
    must_exec(
        &conn,
        "CREATE SEQUENCE s1; CREATE TYPE e1 AS ENUM ('a'); CREATE ROLE r1; CREATE SCHEMA s",
    );
    must_exec(&conn, "CREATE VIEW v2 AS SELECT 1 AS x");
    must_exec(&conn, "CREATE INDEX t_i ON t (a)");
    for (sql, code) in [
        // Syntax.
        ("SELEC 1", "42601"),
        ("SELECT FROM WHERE", "42601"),
        ("SELECT 1 +", "42601"),
        ("SELECT (1", "42601"),
        ("SELECT 'abc", "42601"),
        (r#"SELECT '{"a":1}'::jsonb -> 1 -> "#, "42601"),
        // What is not there.
        ("SELECT nofunc(1)", "42883"),
        ("SELECT lower(1,2,3)", "42883"),
        ("SELECT nope FROM t", "42703"),
        ("SELECT t.nope FROM t", "42703"),
        ("SELECT nope.a FROM t", "42P01"),
        ("INSERT INTO nope VALUES (1)", "42P01"),
        ("INSERT INTO t (nope) VALUES (1)", "42703"),
        ("UPDATE nope SET a=1", "42P01"),
        ("UPDATE t SET nope=1", "42703"),
        ("DELETE FROM nope", "42P01"),
        ("DELETE FROM t WHERE nope=1", "42703"),
        ("DROP TABLE nope", "42P01"),
        ("DROP VIEW nope", "42P01"),
        ("DROP INDEX nope", "42704"),
        ("DROP SEQUENCE nope", "42P01"),
        ("DROP TYPE nope", "42704"),
        ("DROP ROLE nope", "42704"),
        ("DROP SCHEMA nope", "3F000"),
        ("DROP TRIGGER nope ON t", "42704"),
        ("DROP POLICY nope ON t", "42704"),
        ("CREATE INDEX i ON nope (a)", "42P01"),
        ("CREATE INDEX i ON t (nope)", "42703"),
        ("ALTER TABLE nope ADD COLUMN x int", "42P01"),
        ("ALTER TABLE t DROP COLUMN nope", "42703"),
        ("ALTER TABLE t RENAME COLUMN nope TO x", "42703"),
        ("ALTER TABLE t ALTER COLUMN nope TYPE text", "42703"),
        ("ALTER TABLE t ALTER COLUMN nope SET DEFAULT 1", "42703"),
        ("ALTER TABLE t DROP CONSTRAINT nope", "42704"),
        ("ALTER ROLE nope LOGIN", "42704"),
        ("GRANT SELECT ON nope TO r1", "42P01"),
        ("GRANT SELECT ON t TO nope", "42704"),
        ("REVOKE SELECT ON nope FROM r1", "42P01"),
        ("GRANT nope TO r1", "42704"),
        ("SET ROLE nope", "22023"),
        ("SELECT nextval('nope')", "42P01"),
        ("SELECT currval('nope')", "42P01"),
        ("SELECT currval('s1')", "55000"),
        ("VACUUM nope", "42P01"),
        ("WITH x AS (SELECT nope FROM t) SELECT * FROM x", "42703"),
        ("WITH x AS (SELECT 1) SELECT * FROM y", "42P01"),
        ("FETCH ALL FROM nope", "34000"),
        ("CLOSE nope", "34000"),
        // What is there already.
        ("CREATE TABLE t (id int)", "42P07"),
        ("CREATE TABLE u (a int, a int)", "42701"),
        ("CREATE INDEX t_i ON t (a)", "42P07"),
        ("CREATE VIEW v2 AS SELECT 1 AS x", "42P07"),
        ("CREATE SEQUENCE s1", "42P07"),
        ("CREATE TYPE e1 AS ENUM ('a')", "42710"),
        ("CREATE ROLE r1", "42710"),
        ("CREATE SCHEMA s", "42P06"),
        ("ALTER TABLE t ADD COLUMN a int", "42701"),
        ("ALTER TABLE t RENAME TO child", "42P07"),
        // Constraints.
        ("INSERT INTO t VALUES (1,'x',2,1)", "23505"),
        ("INSERT INTO t VALUES (2,NULL,2,1)", "23502"),
        ("INSERT INTO t VALUES (2,'x',1,1)", "23505"),
        ("INSERT INTO t VALUES (2,'x',2,-1)", "23514"),
        ("INSERT INTO child VALUES (1,99)", "23503"),
        ("UPDATE t SET b=NULL, a=NULL", "23502"),
        // Shape of a statement.
        ("INSERT INTO t (id) VALUES (1,2)", "42601"),
        ("INSERT INTO t (id,a) VALUES (1)", "42601"),
        ("INSERT INTO t VALUES (1,2,3,4,5)", "42601"),
        ("SELECT id, a FROM t UNION SELECT id FROM t", "42601"),
        // Aggregates and references.
        ("SELECT id FROM t HAVING id > 0", "42803"),
        ("SELECT count(count(*)) FROM t", "42803"),
        ("SELECT * FROM t WHERE count(*) > 0", "42803"),
        ("SELECT id FROM t ORDER BY 9", "42P10"),
        ("SELECT id FROM t a JOIN t b ON a.id = b.id", "42702"),
        // Regular expressions.
        ("SELECT 'a' ~ '('", "2201B"),
    ] {
        both_answer(&conn, &ext, sql, code);
    }
}

// Outside a transaction a savepoint is 25P01, and inside one an unknown name is 3B001.
#[test]
fn savepoints() {
    let (conn, ext) = setup();
    for c in [&conn, &ext] {
        assert_sqlstate(&c.exec("SAVEPOINT x", &[]).unwrap_err(), "25P01");
        must_exec(c, "BEGIN");
        assert_sqlstate(&c.exec("ROLLBACK TO nope", &[]).unwrap_err(), "3B001");
        must_exec(c, "ROLLBACK");
    }
}

// A dropped statement that names nothing says so and goes on, under IF EXISTS.
#[test]
fn if_exists_still_succeeds() {
    let (conn, ext) = setup();
    for c in [&conn, &ext] {
        for sql in [
            "DROP SEQUENCE IF EXISTS nope",
            "DROP TYPE IF EXISTS nope",
            "DROP ROLE IF EXISTS nope",
            "DROP POLICY IF EXISTS nope ON t",
            "DROP TRIGGER IF EXISTS nope ON t",
        ] {
            c.exec(sql, &[]).expect(sql);
        }
    }
}

// Bind holds a statement to its parameters, as Postgres does, and the simple route has
// no way to give any.
#[test]
fn missing_parameters() {
    let (conn, ext) = setup();
    assert_sqlstate(&conn.exec("SELECT $1", &[]).unwrap_err(), "42P02");
    assert_sqlstate(&ext.exec("SELECT $1", &[]).unwrap_err(), "42P02");

    let server = start_server();
    let mut raw = RawConn::dial(&server).unwrap();
    raw.startup("postgres", "test", "").unwrap();
    raw.send_parse("", "SELECT $1", &[]);
    raw.send_bind("", "", &[], &[], &[]);
    raw.send_sync();
    raw.flush().unwrap();
    match raw.wait_for(|m| matches!(m, Backend::ErrorResponse(_))).unwrap() {
        Backend::ErrorResponse(e) => assert_eq!(e.code, "08P01", "{e}"),
        other => panic!("{other:?}"),
    }
}

// A role's DDL has the extended route too, where it was left to SQLite.
#[test]
fn role_ddl_over_extended() {
    let (_, ext) = setup();
    must_exec(&ext, "CREATE ROLE r2");
    assert_sqlstate(&ext.exec("CREATE ROLE r2", &[]).unwrap_err(), "42710");
    must_exec(&ext, "ALTER ROLE r2 LOGIN");
    must_exec(&ext, "DROP ROLE r2");
    assert_sqlstate(&ext.exec("SET ROLE r2", &[]).unwrap_err(), "22023");
}
