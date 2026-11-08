use crate::common::*;

// comment_on: COMMENT ON stores comments, visible via pg_description /
// obj_description / col_description (what psql's \d+ reads).
#[test]
fn comment_on() {
    let conn = connect(&start_server());
    conn.exec(r#"CREATE TABLE t (id int, name text)"#, &[]).unwrap();
    conn.exec(r#"COMMENT ON TABLE t IS 'a table'"#, &[]).unwrap();
    conn.exec(r#"COMMENT ON COLUMN t.name IS 'the name'"#, &[]).unwrap();

    let tc: String = conn
        .query_row(
            r#"SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE relname='t'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(tc, "a table");
    let cc: String = conn
        .query_row(
            r#"SELECT col_description(a.attrelid, a.attnum) FROM pg_attribute a
		 JOIN pg_class c ON c.oid=a.attrelid WHERE c.relname='t' AND a.attname='name'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(cc, "the name");

    // pg_description carries a row per comment.
    let n: i64 = conn
        .query_row(
            r#"SELECT count(*) FROM pg_description d JOIN pg_class c ON c.oid=d.objoid WHERE c.relname='t'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 2);

    // COMMENT ... IS NULL removes it.
    conn.exec(r#"COMMENT ON TABLE t IS NULL"#, &[]).unwrap();
    let n: i64 = conn
        .query_row(
            r#"SELECT count(*) FROM pg_description d JOIN pg_class c ON c.oid=d.objoid WHERE c.relname='t'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1); // only the column comment remains
}

// A quoted name holding the word IS, or a space, is still the name.
#[test]
fn comment_on_name_with_is() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE "a is b" (id int, "c is d" text, "e f" text)"#);
    must_exec(&conn, r#"COMMENT ON TABLE "a is b" IS 'a table'"#);
    must_exec(&conn, r#"COMMENT ON COLUMN "a is b"."c is d" IS 'a column'"#);
    must_exec(&conn, r#"COMMENT ON COLUMN public."a is b"."e f" IS 'it is a column'"#);

    let got: Vec<String> = query_column(
        &conn,
        "SELECT objname || '/' || subname || '/' || comment FROM _overlite_comments ORDER BY subname",
        0,
    );
    assert_eq!(
        got,
        ["a is b//a table", "a is b/c is d/a column", "a is b/e f/it is a column"]
    );
    let tc: String = conn
        .query_row(
            r#"SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE relname = 'a is b'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(tc, "a table");
}

fn comment_errors_setup() -> (Conn, Conn) {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE t (a int, CONSTRAINT c1 CHECK (a > 0))");
    must_exec(&conn, "CREATE VIEW v AS SELECT 1 AS x");
    must_exec(&conn, "CREATE INDEX i ON t (a)");
    must_exec(&conn, "CREATE SEQUENCE s");
    must_exec(&conn, "CREATE SCHEMA sc");
    must_exec(&conn, "CREATE TABLE sc.t2 (b int)");
    (conn, connect_extended(&server))
}

// A comment on what is not there is the error Postgres gives. It answered success,
// keeping a row nothing would read.
#[test]
fn comment_on_what_is_not_there() {
    let (conn, ext) = comment_errors_setup();
    for (sql, code) in [
        ("COMMENT ON TABLE nope IS 'x'", "42P01"),
        ("COMMENT ON TABLE nope IS NULL", "42P01"),
        ("COMMENT ON TABLE sc.nope IS 'x'", "42P01"),
        ("COMMENT ON TABLE nosc.t IS 'x'", "3F000"),
        (r#"COMMENT ON TABLE "permission denied" IS 'x'"#, "42P01"),
        ("COMMENT ON COLUMN t.nope IS 'x'", "42703"),
        ("COMMENT ON COLUMN nope.a IS 'x'", "42P01"),
        ("COMMENT ON COLUMN sc.t2.nope IS 'x'", "42703"),
        ("COMMENT ON VIEW nope IS 'x'", "42P01"),
        ("COMMENT ON VIEW t IS 'x'", "42809"),
        ("COMMENT ON TABLE v IS 'x'", "42809"),
        ("COMMENT ON INDEX nope IS 'x'", "42P01"),
        ("COMMENT ON SEQUENCE nope IS 'x'", "42P01"),
        ("COMMENT ON MATERIALIZED VIEW nope IS 'x'", "42P01"),
        ("COMMENT ON FOREIGN TABLE nope IS 'x'", "42P01"),
        ("COMMENT ON TYPE nope IS 'x'", "42704"),
        ("COMMENT ON SCHEMA nope IS 'x'", "3F000"),
        ("COMMENT ON ROLE nope IS 'x'", "42704"),
        ("COMMENT ON CONSTRAINT nope ON t IS 'x'", "42704"),
        ("COMMENT ON CONSTRAINT c1 ON nope IS 'x'", "42P01"),
        ("COMMENT ON FUNCTION nope() IS 'x'", "42883"),
        ("COMMENT ON TRIGGER nope ON t IS 'x'", "42704"),
        ("COMMENT ON POLICY nope ON t IS 'x'", "42704"),
        ("COMMENT ON NOPE t IS 'x'", "42601"),
        ("COMMENT ON TABLE t", "42601"),
        ("COMMENT ON TABLE t IS 1", "42601"),
    ] {
        for c in [&conn, &ext] {
            let got = c.exec(sql, &[]);
            assert_eq!(
                got.as_ref().map_err(WireError::code).err(),
                Some(code),
                "{sql}: {got:?}"
            );
        }
    }
    for sql in [
        "COMMENT ON TABLE sc.t2 IS 'x'",
        "COMMENT ON COLUMN sc.t2.b IS 'x'",
        "COMMENT ON VIEW v IS 'x'",
        "COMMENT ON INDEX i IS 'x'",
        "COMMENT ON SEQUENCE s IS 'x'",
        "COMMENT ON SCHEMA sc IS 'x'",
        "COMMENT ON CONSTRAINT c1 ON t IS 'x'",
        "COMMENT ON ROLE postgres IS 'x'",
        "COMMENT ON EXTENSION plpgsql IS 'x'",
    ] {
        conn.exec(sql, &[]).unwrap();
        ext.exec(sql, &[]).unwrap();
    }
}

// A failed COMMENT fails the transaction, on either route.
#[test]
fn failed_comment_aborts_the_transaction() {
    let (conn, ext) = comment_errors_setup();
    for c in [&conn, &ext] {
        must_exec(c, "BEGIN");
        assert_sqlstate(&c.exec("COMMENT ON TABLE nope IS 'x'", &[]).unwrap_err(), "42P01");
        assert_sqlstate(&c.exec("SELECT 1", &[]).unwrap_err(), "25P02");
        must_exec(c, "ROLLBACK");
    }
}

// Quoted text stays what it says: IS inside a comment or a name is no keyword.
#[test]
fn comment_holding_the_word_is() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE "this is it" (id int)"#);
    must_exec(&conn, r#"COMMENT ON TABLE "this is it" IS 'what it is IS NULL'"#);
    let got: String = conn
        .query_row(
            r#"SELECT obj_description(c.oid, 'pg_class') FROM pg_class c WHERE relname = 'this is it'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(got, "what it is IS NULL");
}
