use crate::common::*;

// TestTimestamptz: a timestamptz column stores an absolute instant (normalized to
// UTC), so offsets on input are honored, ordering/comparison is chronological,
// and it round-trips as a real timestamptz.
#[test]
fn timestamptz() {
    let conn = connect(&start_server()); // simple protocol (text), like psql

    conn.exec("CREATE TABLE t (id int, ts timestamptz)", &[]).unwrap();
    // 10:30+02 == 08:30 UTC; 09:00+00 == 09:00 UTC.
    conn.exec("INSERT INTO t VALUES (1, '2024-01-15 10:30:00+02')", &[])
        .unwrap();
    conn.exec("INSERT INTO t VALUES (2, '2024-01-15 09:00:00+00')", &[])
        .unwrap();

    // Read back as an instant.
    let ts: Time = conn.query_row("SELECT ts FROM t WHERE id = 1", &[]).scan().unwrap();
    assert_eq!(ts, time_utc(2024, 1, 15, 8, 30, 0, 0));

    // Chronological ordering (the offset was applied, not compared as text).
    let first: i64 = conn
        .query_row("SELECT id FROM t ORDER BY ts LIMIT 1", &[])
        .scan()
        .unwrap();
    assert_eq!(first, 1);

    // A comparison against an offset literal is normalized too.
    let cnt: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE ts < '2024-01-15 10:45:00+02'", &[])
        .scan()
        .unwrap();
    assert_eq!(cnt, 1); // only the 08:30 UTC row is before 08:45 UTC

    // AT TIME ZONE converts the instant to a wall clock in that zone.
    let wall: String = conn
        .query_row("SELECT ts AT TIME ZONE 'America/New_York' FROM t WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert_eq!(wall, "2024-01-15 03:30:00"); // 08:30 UTC == 03:30 EST
}

// A cast or a type word on the operand of AT TIME ZONE belongs to the operand. The
// direction follows the type: a timestamp is a wall clock in the zone and yields an
// instant, a timestamptz is an instant and yields a wall clock. Answers checked
// against PostgreSQL 17.
#[test]
fn at_time_zone_on_a_cast() {
    let conn = connect(&start_server());
    for (sql, want) in [
        (
            "SELECT '2024-01-02 03:04:05'::timestamp AT TIME ZONE 'UTC'",
            "2024-01-02 03:04:05+00",
        ),
        (
            "SELECT '2024-01-02 03:04:05'::timestamp AT TIME ZONE 'America/Sao_Paulo'",
            "2024-01-02 06:04:05+00",
        ),
        (
            "SELECT '2024-01-02 03:04:05.5'::timestamp without time zone at time zone 'UTC'",
            "2024-01-02 03:04:05.5+00",
        ),
        (
            "SELECT '2024-01-02 03:04:05'::timestamp(3) AT TIME ZONE 'UTC'",
            "2024-01-02 03:04:05+00",
        ),
        (
            "SELECT timestamp '2024-01-02 03:04:05' at time zone 'UTC'",
            "2024-01-02 03:04:05+00",
        ),
        (
            "SELECT CAST('2024-01-02 03:04:05' AS timestamp) AT TIME ZONE 'America/Sao_Paulo'",
            "2024-01-02 06:04:05+00",
        ),
        (
            "SELECT '2024-01-02 03:04:05+00'::timestamptz AT TIME ZONE 'America/Sao_Paulo'",
            "2024-01-02 00:04:05",
        ),
        (
            "SELECT '2024-01-02 03:04:05+00'::timestamp with time zone AT TIME ZONE 'America/Sao_Paulo'",
            "2024-01-02 00:04:05",
        ),
        (
            "SELECT timestamptz '2024-01-02 03:04:05+00' AT TIME ZONE 'America/Sao_Paulo'",
            "2024-01-02 00:04:05",
        ),
        // Daylight saving: New York is four hours behind UTC in July.
        (
            "SELECT '2024-07-02 03:04:05'::timestamp AT TIME ZONE 'America/New_York'",
            "2024-07-02 07:04:05+00",
        ),
        // Both directions in one expression.
        (
            "SELECT '2024-01-02 03:04:05'::timestamp AT TIME ZONE 'UTC' AT TIME ZONE 'America/Sao_Paulo'",
            "2024-01-02 00:04:05",
        ),
    ] {
        let got: String = conn.query_row(sql, &[]).scan().unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(got, want, "{sql}");
    }
}

// Which way AT TIME ZONE goes is the operand's declared type, however it is reached.
// Each answer is what Postgres 17 gave.
#[test]
fn at_time_zone_on_a_column_goes_by_its_type() {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    must_exec(
        &conn,
        "CREATE TABLE t (ts timestamp, tz timestamptz, d date, ts3 timestamp(3), tz2 timestamp with time zone)",
    );
    must_exec(
        &conn,
        "INSERT INTO t VALUES ('2024-01-02 03:04:05', '2024-01-02 03:04:05+00', '2024-01-02', '2024-01-02 03:04:05', '2024-01-02 03:04:05+00')",
    );
    for (sql, want) in [
        (
            "SELECT ts AT TIME ZONE 'America/Sao_Paulo' FROM t",
            "2024-01-02 06:04:05+00",
        ),
        (
            "SELECT tz AT TIME ZONE 'America/Sao_Paulo' FROM t",
            "2024-01-02 00:04:05",
        ),
        ("SELECT t.ts AT TIME ZONE 'UTC' FROM t", "2024-01-02 03:04:05+00"),
        ("SELECT t.tz AT TIME ZONE 'UTC' FROM t", "2024-01-02 03:04:05"),
        (
            "SELECT x.ts AT TIME ZONE 'Asia/Tokyo' FROM t x",
            "2024-01-01 18:04:05+00",
        ),
        ("SELECT ts3 AT TIME ZONE 'Asia/Tokyo' FROM t", "2024-01-01 18:04:05+00"),
        ("SELECT tz2 AT TIME ZONE 'Asia/Tokyo' FROM t", "2024-01-02 12:04:05"),
        ("SELECT d AT TIME ZONE 'UTC' FROM t", "2024-01-02 00:00:00"),
        (
            "SELECT (ts + interval '1 hour') AT TIME ZONE 'UTC' FROM t",
            "2024-01-02 04:04:05+00",
        ),
        (
            "SELECT ts AT TIME ZONE 'UTC' AT TIME ZONE 'Asia/Tokyo' FROM t",
            "2024-01-02 12:04:05",
        ),
        (
            "SELECT ts::timestamptz AT TIME ZONE 'Asia/Tokyo' FROM t",
            "2024-01-02 12:04:05",
        ),
        (
            "SELECT tz::timestamp AT TIME ZONE 'Asia/Tokyo' FROM t",
            "2024-01-01 18:04:05+00",
        ),
        (
            "SELECT s AT TIME ZONE 'UTC' FROM (SELECT ts AS s FROM t) q",
            "2024-01-02 03:04:05+00",
        ),
        (
            "SELECT s AT TIME ZONE 'UTC' FROM (SELECT tz AS s FROM t) q",
            "2024-01-02 03:04:05",
        ),
        ("SELECT (SELECT ts AT TIME ZONE 'UTC' FROM t)", "2024-01-02 03:04:05+00"),
    ] {
        for c in [&conn, &ext] {
            let rows = c
                .query_formats(sql, &[TEXT_FORMAT], &[])
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
            assert_eq!(rows.iter().next().unwrap().cell(0).text(), want, "{sql}");
        }
    }
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM t WHERE ts AT TIME ZONE 'Asia/Tokyo' < '2024-01-02'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1);
}

// A literal or a parameter that reads as a timestamp with an offset is only that where
// the type says so. To anything else it is text, kept byte for byte: it used to be
// turned to UTC wherever it stood. Each answer is what Postgres 17 gave.
#[test]
fn offset_stays_text_outside_a_timestamp() {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    must_exec(
        &conn,
        "CREATE TABLE t (id int, s text, v varchar(50), ts timestamp, tz timestamptz)",
    );
    let lit = "2024-01-02 03:04:05+02";
    for (c, id) in [(&conn, 1), (&ext, 2)] {
        c.exec(
            &format!("INSERT INTO t VALUES ({id}, '{lit}', '{lit}', '{lit}', '{lit}')"),
            &[],
        )
        .unwrap();
    }
    ext.exec("INSERT INTO t VALUES (3, $1, $2, $3, $4)", &[&lit, &lit, &lit, &lit])
        .unwrap();
    for id in 1..=3 {
        let q = format!("SELECT s, v, ts, tz FROM t WHERE id = {id}");
        let rows = conn.query_formats(&q, &[TEXT_FORMAT], &[]).unwrap();
        let row = rows.iter().next().unwrap();
        assert_eq!(row.cell(0).text(), lit, "text column {id}");
        assert_eq!(row.cell(1).text(), lit, "varchar column {id}");
        assert_eq!(row.cell(2).text(), "2024-01-02 03:04:05", "timestamp column {id}");
        assert_eq!(row.cell(3).text(), "2024-01-02 01:04:05+00", "timestamptz column {id}");
    }
    must_exec(&conn, "UPDATE t SET s = '2024-01-02 03:04:05-03' WHERE id = 1");
    assert_eq!(
        scan_str(&conn, "SELECT s FROM t WHERE id = 1"),
        "2024-01-02 03:04:05-03"
    );
    for (sql, want) in [
        ("SELECT '2024-01-02 03:04:05+02'", "2024-01-02 03:04:05+02"),
        ("SELECT '2024-01-02 03:04:05+02'::text", "2024-01-02 03:04:05+02"),
        ("SELECT '2024-01-02 03:04:05+02' || 'x'", "2024-01-02 03:04:05+02x"),
        ("SELECT count(*)::text FROM t WHERE s = '2024-01-02 03:04:05-03'", "1"),
        ("SELECT count(*)::text FROM t WHERE s = '2024-01-02 03:04:05+02'", "2"),
    ] {
        assert_eq!(scan_str(&conn, sql), want, "{sql}");
    }
    let n: i64 = conn
        .query_row("SELECT length('2024-01-02 03:04:05+02')", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 22);
}

// Against a column of a time type a value is read as that type, literal or parameter.
#[test]
fn offset_compares_as_the_column_it_meets() {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    must_exec(&conn, "CREATE TABLE t (ts timestamp, tz timestamptz)");
    must_exec(
        &conn,
        "INSERT INTO t VALUES ('2024-01-02 03:04:05', '2024-01-02 01:04:05+00')",
    );
    for (sql, want) in [
        ("SELECT count(*) FROM t WHERE tz = '2024-01-02 03:04:05+02'", 1),
        ("SELECT count(*) FROM t WHERE '2024-01-02 03:04:05+02' = tz", 1),
        ("SELECT count(*) FROM t WHERE tz = '2024-01-02 01:04:05+00'", 1),
        ("SELECT count(*) FROM t WHERE tz > '2024-01-02 00:00:00-03'", 0),
        ("SELECT count(*) FROM t WHERE tz < '2024-01-02 00:00:00-03'", 1),
        ("SELECT count(*) FROM t WHERE ts = '2024-01-02 03:04:05+02'", 1),
        ("SELECT count(*) FROM t x WHERE x.tz = '2024-01-02 03:04:05+02'", 1),
    ] {
        for c in [&conn, &ext] {
            assert_eq!(c.query_row(sql, &[]).scan::<i64>().unwrap(), want, "{sql}");
        }
    }
    for (sql, want) in [
        ("SELECT count(*) FROM t WHERE tz = $1", 1),
        ("SELECT count(*) FROM t WHERE ts = $1", 1),
        ("SELECT count(*) FROM t WHERE $1 = tz", 1),
    ] {
        let got: i64 = ext.query_row(sql, &[&"2024-01-02 03:04:05+02"]).scan().unwrap();
        assert_eq!(got, want, "{sql}");
    }
}

// A timestamptz's default is the one place DDL holds such a value, and its column says
// what it is. A text column's default says it is text.
#[test]
fn offset_in_a_default() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE d (id int, a timestamptz DEFAULT '2024-01-02 03:04:05+02', b text DEFAULT '2024-01-02 03:04:05+02')",
    );
    must_exec(&conn, "INSERT INTO d (id) VALUES (1)");
    let rows = conn.query_formats("SELECT a, b FROM d", &[TEXT_FORMAT], &[]).unwrap();
    let row = rows.iter().next().unwrap();
    assert_eq!(row.cell(0).text(), "2024-01-02 01:04:05+00");
    assert_eq!(row.cell(1).text(), "2024-01-02 03:04:05+02");
}
