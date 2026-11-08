use crate::common::*;

// A client asking for binary results gets what the row description promised.
// Time columns were described with their real OID but sent as text, so a
// binary-mode client decoded 27 bytes of "2026-09-17 15:31:54…" as an 8-byte
// integer and failed. pgx asks for binary by default over the extended
// protocol.
#[test]
fn time_columns_in_binary_format() {
    let conn = connect_extended(&start_server());

    must_exec(
        &conn,
        "CREATE TABLE t (
		id int primary key,
		ts timestamp,
		tstz timestamptz,
		d date)",
    );
    must_exec(
        &conn,
        "INSERT INTO t VALUES (
		1,
		'2026-09-17 15:31:54',
		'2026-09-17 15:31:54+00',
		'2026-09-17')",
    );

    let (ts, tstz, d): (Time, Time, Time) = conn
        .query_row("SELECT ts, tstz, d FROM t WHERE id = 1", &[])
        .scan()
        .unwrap();

    assert_eq!(ts.year(), 2026);
    assert_eq!(ts.month(), 9);
    assert_eq!(ts.day(), 17);
    assert_eq!(ts.hour(), 15);
    assert_eq!(ts.minute(), 31);
    assert_eq!(ts.second(), 54);

    assert_eq!(tstz.timestamp(), ts.timestamp());

    assert_eq!(d.year(), 2026);
    assert_eq!(d.month(), 9);
    assert_eq!(d.day(), 17);

    // A NULL still reads as one.
    must_exec(&conn, "INSERT INTO t (id) VALUES (2)");
    let null_ts: Option<Time> = conn.query_row("SELECT ts FROM t WHERE id = 2", &[]).scan().unwrap();
    assert!(null_ts.is_none());

    // And a value that came from now() round-trips.
    must_exec(&conn, "INSERT INTO t (id, tstz) VALUES (3, now())");
    let from_now: Time = conn.query_row("SELECT tstz FROM t WHERE id = 3", &[]).scan().unwrap();
    let diff = chrono::Utc::now().signed_duration_since(from_now);
    assert!(
        diff.abs() <= chrono::Duration::hours(24),
        "{from_now} is {diff} away from now"
    );
}
