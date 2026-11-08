use crate::common::*;

// A client parses a value according to the type the column announced, and
// rejects anything that is not that type's text format. A driver binding a
// timestamp may send nanoseconds and a trailing Z, which used to be stored and
// handed back untouched: DBeaver's JDBC driver answered "Trailing junk on
// timestamp: 'Z'" and refused to show the row.

#[test]
fn timestamp_text_is_postgres_shaped() {
    let addr = start_server();
    // Writes go through bound parameters, the way a driver sends them; reads
    // come back as text, the way a JDBC client asks for them.
    let conn = connect_extended(&addr);
    let reader = connect(&addr);

    must_exec(
        &conn,
        "CREATE TABLE t (id int primary key, ts timestamptz, plain timestamp, d date)",
    );

    // Exactly what pgx sends for a timestamp in text: RFC3339 with nanoseconds.
    conn.exec(
        "INSERT INTO t VALUES ($1, $2, $3, $4)",
        &[
            &1,
            &"2026-09-17 17:19:19.398458334Z",
            &"2026-09-17 17:19:19.398458334",
            &"2026-09-17",
        ],
    )
    .unwrap();

    let (ts, plain, d): (String, String, String) = reader
        .query_row("SELECT ts, plain, d FROM t WHERE id = 1", &[])
        .scan()
        .unwrap();

    assert_eq!(ts, "2026-09-17 17:19:19.398458+00");
    assert_eq!(plain, "2026-09-17 17:19:19.398458");
    assert_eq!(d, "2026-09-17");

    for v in [&ts, &plain] {
        assert_not_contains!(v, "Z", "a trailing Z is not a Postgres timestamp");
        assert_not_contains!(v, "T", "Postgres separates date and time with a space");
    }
}

// A value written with an offset other than UTC is stored as the same instant.
#[test]
fn timestamp_with_offset_parameter() {
    let addr = start_server();
    let conn = connect_extended(&addr);

    must_exec(&conn, "CREATE TABLE t (id int primary key, ts timestamptz)");
    conn.exec("INSERT INTO t VALUES ($1, $2)", &[&1, &"2026-09-17 14:19:19-03:00"])
        .unwrap();

    let ts: String = connect(&addr).query_row("SELECT ts FROM t", &[]).scan().unwrap();
    assert_eq!(ts, "2026-09-17 17:19:19+00");
}

// A row already carrying the old shape still reads back correctly: the fix has
// to reach data written before it, not just new writes.
#[test]
fn stored_nanosecond_timestamp_reads_back() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE TABLE t (id int primary key, ts timestamptz)");
    // Write it the way it used to be stored, past the normalizing paths.
    must_exec(&conn, "INSERT INTO t (id) VALUES (1)");
    must_exec(&conn, "UPDATE t SET ts = '2026-09-17 17:19:19.398458334Z' WHERE id = 1");

    let raw: String = conn.query_row("SELECT CAST(ts AS TEXT) FROM t", &[]).scan().unwrap();

    let ts: String = conn.query_row("SELECT ts FROM t", &[]).scan().unwrap();
    assert_eq!(
        ts, "2026-09-17 17:19:19.398458+00",
        "a row stored in the old shape (raw {raw:?}) must still read back as a timestamp"
    );
}

// And the value still means the same instant to a client that reads it as one.
#[test]
fn timestamp_round_trips_through_text() {
    let conn = connect_extended(&start_server());

    must_exec(&conn, "CREATE TABLE t (id int primary key, ts timestamptz)");
    let want = time_utc(2026, 9, 17, 17, 19, 19, 398458000);
    conn.exec("INSERT INTO t VALUES ($1, $2)", &[&1, &want]).unwrap();

    let got: Time = conn.query_row("SELECT ts FROM t", &[]).scan().unwrap();
    assert_eq!(got, want, "got {got}, want {want}");
}

// A column's type is what a client reads to decide how to parse its values, so
// timestamptz and timestamp have to be told apart: the catalog reported both as
// "timestamp without time zone" while the wire announced timestamptz for one of
// them, leaving a tool to parse an offset it had been told would not be there.
#[test]
fn timestamptz_is_its_own_type() {
    let addr = start_server();
    let conn = connect(&addr);

    must_exec(
        &conn,
        "CREATE TABLE t (withzone timestamptz, without timestamp, d date)",
    );

    let types = |column: &str| -> (String, String) {
        conn.query_row(
            "
			SELECT data_type, udt_name FROM information_schema.columns
			WHERE table_name = 't' AND column_name = $1",
            &[&column],
        )
        .scan()
        .unwrap()
    };

    let (dt, udt) = types("withzone");
    assert_eq!(dt, "timestamp with time zone");
    assert_eq!(udt, "timestamptz");

    let (dt, udt) = types("without");
    assert_eq!(dt, "timestamp without time zone");
    assert_eq!(udt, "timestamp");

    let (dt, _) = types("d");
    assert_eq!(dt, "date");

    // And the type the catalog reports is the one the wire announces.
    let rows = conn.query("SELECT withzone, without FROM t", &[]).unwrap();
    let fds = &rows.fields;
    assert_eq!(fds.len(), 2);
    assert_eq!(fds[0].data_type_oid, 1184, "timestamptz column");
    assert_eq!(fds[1].data_type_oid, 1114, "timestamp column");
}
