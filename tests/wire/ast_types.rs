//! Answers that depended on a type the text passes could not see, for statements the
//! AST path now translates from its type environment (docs/adr/0001-sql-ast.md, stage
//! 3): each failed before the stage, held to Postgres 17's answer.

use crate::common::*;

/// The first column of each row, as text, over the simple protocol.
#[track_caller]
fn col(conn: &Conn, sql: &str) -> Vec<String> {
    raw_rows(conn, sql)
        .into_iter()
        .map(|r| {
            r[0].as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default()
        })
        .collect()
}

fn dt(conn: &Conn) {
    must_exec(
        conn,
        "CREATE TABLE dt (id int, d date, ts timestamp, tz timestamptz, du interval, s text, x text)",
    );
    must_exec(
        conn,
        "INSERT INTO dt VALUES (1, '2024-01-02', '2024-01-02 00:00:00', '2024-01-02 00:00:00+00', '1 day', \
         'word', 'wodr')",
    );
}

// A date compared with a timestamp is the timestamp at its midnight. The engine's pass
// typed that comparison only in a statement that said `date`, `interval` or ORDER BY
// somewhere, so two bare columns compared their texts, and '2024-01-02' is before
// '2024-01-02 00:00:00'.
#[test]
fn a_date_column_compares_with_a_timestamp_column_at_its_midnight() {
    let conn = connect(&start_server());
    dt(&conn);
    for sql in [
        "SELECT count(*) FROM dt WHERE d < tz",
        "SELECT count(*) FROM dt WHERE d < ts",
        "SELECT count(*) FROM dt WHERE ts > d",
    ] {
        assert_eq!(col(&conn, sql), ["0"], "{sql}");
    }
}

// An untyped literal or a parameter compared with a date, timestamp or timestamptz is
// read by that type's input function, as Postgres reads it. As text, a date alone was
// before the midnight of its day, a `T` sorted after a space and a date spelled with a
// month's name matched nothing.
#[test]
fn a_literal_compared_with_a_date_or_time_column_is_read_as_one() {
    let conn = connect(&start_server());
    dt(&conn);
    for (sql, want) in [
        ("SELECT count(*) FROM dt WHERE tz > '2024-01-02'", "0"),
        ("SELECT count(*) FROM dt WHERE ts = '2024-01-02T00:00:00'", "1"),
        ("SELECT count(*) FROM dt WHERE d = 'Jan 2 2024'", "1"),
        ("SELECT count(*) FROM dt WHERE tz IN ('2024-01-02 09:00:00+09')", "1"),
        (
            "SELECT count(*) FROM dt WHERE tz BETWEEN '2024-01-01T12:00:00Z' AND '2024-01-02'",
            "1",
        ),
    ] {
        assert_eq!(col(&conn, sql), [want], "{sql}");
    }
    // A parameter is typed as the column it meets, and read so.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM dt WHERE tz > $1", &[&"2024-01-02"])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
}

// An interval compared with a literal is compared by its span: `1 day` is more than
// `23 hours`. Without the word interval in the statement the texts were compared.
#[test]
fn an_interval_column_compares_with_a_literal_by_its_span() {
    let conn = connect(&start_server());
    dt(&conn);
    assert_eq!(col(&conn, "SELECT count(*) FROM dt WHERE du > '23 hours'"), ["1"]);
}

// An interval times a CASE is an interval: the pass read a sum's terms only up to a
// CASE, and SQLite multiplied the text '1 day' by 2.
#[test]
fn an_interval_times_a_case_is_an_interval() {
    let conn = connect(&start_server());
    assert_eq!(
        col(&conn, "SELECT interval '1 day' * CASE WHEN true THEN 2 END"),
        ["2 days"]
    );
}

// AT TIME ZONE goes by its operand's type: a CASE over a timestamp is a wall clock,
// which becomes an instant, where an expression was taken for an instant. Postgres
// labels the column timezone.
#[test]
fn at_time_zone_goes_by_the_type_of_an_expression() {
    let conn = connect(&start_server());
    dt(&conn);
    let r = conn
        .exec_params(
            "SELECT (CASE WHEN true THEN ts END) AT TIME ZONE 'America/Sao_Paulo', ts AT TIME ZONE 'UTC' FROM dt",
            &[] as &[&str],
            &[],
            &[],
            &[],
        )
        .unwrap();
    let names: Vec<&str> = r.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["timezone", "timezone"]);
    assert_eq!(
        col(
            &conn,
            "SELECT (CASE WHEN true THEN ts END) AT TIME ZONE 'America/Sao_Paulo' FROM dt"
        ),
        ["2024-01-02 03:00:00+00"]
    );
}

// pg_trgm's `%` between two text columns is their similarity: only a literal on a side
// told the pass it was not a remainder, and SQLite's remainder of two words is NULL.
#[test]
fn percent_between_text_columns_is_similarity() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE EXTENSION pg_trgm");
    dt(&conn);
    assert_eq!(col(&conn, "SELECT s % x FROM dt"), ["f"]);
}

// In a session whose TimeZone is not UTC, a timestamp compared with a timestamptz is a
// wall clock there; and the typed translation reads the zone when it runs, so the same
// text in another session reads it in that one's.
#[test]
fn a_timestamp_compared_with_a_timestamptz_is_read_in_the_session_zone() {
    let server = start_server();
    let conn = connect(&server);
    dt(&conn);
    let sql = "SELECT count(*) FROM dt WHERE tz < CAST('2024-01-01 20:00' AS timestamp)";
    must_exec(&conn, "SET TIME ZONE 'America/New_York'");
    assert_eq!(col(&conn, sql), ["1"]);
    let other = connect(&server);
    assert_eq!(col(&other, sql), ["0"]);
}
