//! A cast answers as Postgres' input function for the type does. SQLite's own CAST
//! goes by affinity: `'2024-01-02 03:04:05'::timestamp` was 2024, and `'abc'::int` was 0.

use crate::common::*;

/// The text of the one column of a one-row query, over both protocols.
fn answers(conn: &Conn, ext: &Conn, sql: &str, want: &str) {
    assert_eq!(scan_str(conn, sql), want, "simple: {sql}");
    assert_eq!(scan_str(ext, sql), want, "extended: {sql}");
}

fn pair() -> (Conn, Conn) {
    let server = start_server();
    (connect(&server), connect_extended(&server))
}

// Each expected text is what Postgres 17 printed.
#[test]
fn casts_to_date_and_time_types() {
    let (conn, ext) = pair();
    for (sql, want) in [
        ("SELECT '2024-01-02 03:04:05'::timestamp::text", "2024-01-02 03:04:05"),
        (
            "SELECT '2024-01-02 03:04:05.5'::timestamp::text",
            "2024-01-02 03:04:05.5",
        ),
        ("SELECT '2024-01-02'::timestamp::text", "2024-01-02 00:00:00"),
        ("SELECT '2024-01-02T03:04:05'::timestamp::text", "2024-01-02 03:04:05"),
        (
            "SELECT CAST('2024-01-02 03:04:05' AS timestamp)::text",
            "2024-01-02 03:04:05",
        ),
        (
            "SELECT CAST('2024-01-02' AS timestamp without time zone)::text",
            "2024-01-02 00:00:00",
        ),
        ("SELECT TIMESTAMP '2024-01-02 03:04:05'::text", "2024-01-02 03:04:05"),
        ("SELECT '2024-01-02'::date::text", "2024-01-02"),
        ("SELECT '2024-1-2'::date::text", "2024-01-02"),
        ("SELECT 'Jan 2 2024'::date::text", "2024-01-02"),
        ("SELECT '2024-01-02 03:04:05'::date::text", "2024-01-02"),
        ("SELECT DATE '2024-01-02'::text", "2024-01-02"),
        ("SELECT '03:04:05'::time::text", "03:04:05"),
        ("SELECT '03:04'::time::text", "03:04:00"),
        ("SELECT '2024-01-02 03:04:05'::time::text", "03:04:05"),
        ("SELECT TIME '03:04:05'::text", "03:04:05"),
        ("SELECT '03:04:05+02'::timetz::text", "03:04:05+02"),
        ("SELECT 'infinity'::timestamp::text", "infinity"),
        ("SELECT '-infinity'::date::text", "-infinity"),
        ("SELECT 'epoch'::timestamp::text", "1970-01-01 00:00:00"),
        (
            "SELECT '2024-01-02 03:04:05.678'::timestamp(1)::text",
            "2024-01-02 03:04:05.7",
        ),
        ("SELECT '2024-01-02'::timestamp::date::text", "2024-01-02"),
    ] {
        answers(&conn, &ext, sql, want);
    }
}

// The rest of SQLite's NUMERIC and INTEGER affinity: none of these names a number.
#[test]
fn casts_to_types_sqlite_reads_as_numbers() {
    let (conn, ext) = pair();
    for (sql, want) in [
        ("SELECT '1 day'::interval::text", "1 day"),
        ("SELECT '1 day 02:00:00'::interval::text", "1 day 02:00:00"),
        ("SELECT '14 mons'::interval::text", "1 year 2 mons"),
        (
            "SELECT '123e4567-e89b-12d3-a456-426614174000'::uuid::text",
            "123e4567-e89b-12d3-a456-426614174000",
        ),
        (
            "SELECT '12345678-1234-1234-1234-123456789012'::uuid::text",
            "12345678-1234-1234-1234-123456789012",
        ),
        ("SELECT '1.50'::json::text", "1.50"),
        ("SELECT '\"x\"'::jsonb::text", "\"x\""),
        ("SELECT '{\"a\":1}'::json::text", "{\"a\":1}"),
        ("SELECT 'x'::name::text", "x"),
        ("SELECT 'abc'::varchar(2)::text", "ab"),
        ("SELECT '10.0.0.1'::inet::text", "10.0.0.1"),
        ("SELECT '(1,2)'::point::text", "(1,2)"),
        ("SELECT '101'::varbit::text", "101"),
    ] {
        answers(&conn, &ext, sql, want);
    }
}

#[test]
fn casts_of_columns() {
    let (conn, ext) = pair();
    must_exec(
        &conn,
        "CREATE TABLE t (ts timestamp, d date, tm time, s text, i int);
         INSERT INTO t VALUES ('2024-01-02 03:04:05', '2024-01-02', '03:04:05', '2024-01-02 03:04:05', 1)",
    );
    for (sql, want) in [
        ("SELECT ts::date::text FROM t", "2024-01-02"),
        ("SELECT d::timestamp::text FROM t", "2024-01-02 00:00:00"),
        ("SELECT s::timestamp::text FROM t", "2024-01-02 03:04:05"),
        ("SELECT s::date::text FROM t", "2024-01-02"),
        ("SELECT s::time::text FROM t", "03:04:05"),
        ("SELECT ts::time::text FROM t", "03:04:05"),
        (
            "SELECT count(*)::text FROM t WHERE ts = '2024-01-02 03:04:05'::timestamp",
            "1",
        ),
        ("SELECT count(*)::text FROM t WHERE d = '2024-01-02'::date", "1"),
        ("SELECT count(*)::text FROM t WHERE tm = '03:04:05'::time", "1"),
    ] {
        answers(&conn, &ext, sql, want);
    }
}

// A default cast in a table's DDL keeps the value, not what SQLite makes of it.
#[test]
fn cast_in_a_default() {
    let (conn, ext) = pair();
    must_exec(
        &conn,
        "CREATE TABLE t (id int, a timestamp DEFAULT '2024-01-02 03:04:05'::timestamp,
                         b date DEFAULT DATE '2024-01-02', c uuid DEFAULT CAST('12345678-1234-1234-1234-123456789012' AS uuid));
         INSERT INTO t (id) VALUES (1)",
    );
    answers(&conn, &ext, "SELECT a::text FROM t", "2024-01-02 03:04:05");
    answers(&conn, &ext, "SELECT b::text FROM t", "2024-01-02");
    answers(
        &conn,
        &ext,
        "SELECT c::text FROM t",
        "12345678-1234-1234-1234-123456789012",
    );
}

#[test]
fn casts_that_fail() {
    let (conn, ext) = pair();
    for (sql, code) in [
        ("SELECT 'abc'::timestamp", "22007"),
        ("SELECT 'abc'::timestamptz", "22007"),
        ("SELECT 'abc'::date", "22007"),
        ("SELECT 'abc'::time", "22007"),
        ("SELECT 'abc'::interval", "22007"),
        ("SELECT '2024-13-01'::date", "22008"),
        ("SELECT '2024-02-30'::date", "22008"),
        ("SELECT '25:00:00'::time", "22008"),
        ("SELECT '2024-01-01 12:00:00+24:30'::timestamp", "22009"),
        ("SELECT 'abc'::int", "22P02"),
        ("SELECT '1.5'::int", "22P02"),
        ("SELECT 'abc'::numeric", "22P02"),
        ("SELECT 'abc'::float8", "22P02"),
        ("SELECT 'abc'::uuid", "22P02"),
        ("SELECT '[1'::jsonb", "22P02"),
        ("SELECT 'abc'::boolean", "22P02"),
        (r"SELECT '\'::bytea", "22P02"),
        (r"SELECT '\xzz'::bytea", "22023"),
        ("SELECT '99999999999'::int", "22003"),
        ("SELECT 99999999999::int", "22003"),
        ("SELECT 'a'::nosuchtype", "42704"),
        ("SELECT CAST(1 AS nosuchtype)", "42704"),
        ("SELECT 1::date", "42846"),
        ("SELECT 1::timestamp", "42846"),
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
}

// What fails names the value and the type as the input function does.
#[test]
fn messages_of_a_failed_cast() {
    let conn = connect(&start_server());
    assert_pg_error(
        &conn.exec("SELECT 'abc'::timestamp", &[]).unwrap_err(),
        "22007",
        r#"invalid input syntax for type timestamp: "abc""#,
    );
    assert_pg_error(
        &conn.exec("SELECT 'abc'::int", &[]).unwrap_err(),
        "22P02",
        r#"invalid input syntax for type integer: "abc""#,
    );
    assert_pg_error(
        &conn.exec("SELECT 'a'::nosuchtype", &[]).unwrap_err(),
        "42704",
        r#"type "nosuchtype" does not exist"#,
    );
}

// A cast of an enum stays a cast of one, and a string that is one of its labels.
#[test]
fn casts_to_a_user_type() {
    let (conn, ext) = pair();
    must_exec(&conn, "CREATE TYPE mood AS ENUM ('sad', 'ok')");
    answers(&conn, &ext, "SELECT 'ok'::mood::text", "ok");
}

// Postgres labels a cast with what its operand is called, or else its type.
#[test]
fn cast_labels() {
    let (conn, _) = pair();
    must_exec(&conn, "CREATE TABLE t (a int)");
    for (sql, label) in [
        ("SELECT a::timestamp FROM t WHERE false", "a"),
        ("SELECT t.a::date FROM t WHERE false", "a"),
        ("SELECT '2024-01-02'::date", "date"),
        ("SELECT '2024-01-02 03:04:05'::timestamp", "timestamp"),
        ("SELECT '1'::int", "int4"),
        ("SELECT a::int AS b FROM t WHERE false", "b"),
    ] {
        let rows = conn.query(sql, &[]).unwrap();
        assert_eq!(rows.field_descriptions()[0].name, label, "{sql}");
    }
}
