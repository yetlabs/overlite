use crate::common::*;

// Exercises the date/time surface end to end: timestamp storage, now(),
// current_date, date_trunc, extract/date_part, and to_char.
#[test]
fn date_time_functions() {
    let conn = connect(&start_server());

    // timestamp/date columns store and round-trip.
    must_exec(
        &conn,
        "CREATE TABLE eventos (id SERIAL PRIMARY KEY, quando TIMESTAMP, dia DATE)",
    );
    must_exec(
        &conn,
        "INSERT INTO eventos (quando, dia) VALUES ('2024-03-15 14:30:45', '2024-03-15')",
    );

    let (quando, dia): (String, String) = conn.query_row("SELECT quando, dia FROM eventos", &[]).scan().unwrap();
    assert_eq!(quando, "2024-03-15 14:30:45");
    assert_eq!(dia, "2024-03-15");

    // date_trunc.
    let trunc_month: String = conn
        .query_row("SELECT date_trunc('month', quando) FROM eventos", &[])
        .scan()
        .unwrap();
    assert_eq!(trunc_month, "2024-03-01 00:00:00");

    // extract / date_part.
    let (year, month, day): (i64, i64, i64) = conn
        .query_row(
            "SELECT extract(year FROM quando), extract(month FROM quando), date_part('day', quando) FROM eventos",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!([year, month, day], [2024, 3, 15]);

    // to_char.
    let formatted: String = conn
        .query_row("SELECT to_char(quando, 'YYYY-MM-DD') FROM eventos", &[])
        .scan()
        .unwrap();
    assert_eq!(formatted, "2024-03-15");

    // now() and current_date resolve to a real timestamp/date.
    let (now_str, today): (String, String) = conn.query_row("SELECT now(), current_date", &[]).scan().unwrap();
    let utc_today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    assert_contains!(now_str, utc_today.as_str(), "now() as 2006-01-02");
    assert_eq!(
        today,
        chrono::Utc::now().format("%Y-%m-%d").to_string(),
        "current_date as 2006-01-02"
    );
}

// rewrite_at_time_zone: the words in a literal are text, not an operator to rewrite.
// Finding them never ended, and took the server down.
#[test]
fn at_time_zone_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    let s: String = conn
        .query_row("SELECT 'AT TIME ZONE x' || now()::text", &[])
        .scan()
        .unwrap();
    assert!(s.starts_with("AT TIME ZONE x"), "{s}");
    must_exec(&conn, "CREATE TABLE t (id int, a text DEFAULT 'at time zone y')");
    let a: String = conn
        .query_row("INSERT INTO t (id) VALUES (1) RETURNING a", &[])
        .scan()
        .unwrap();
    assert_eq!(a, "at time zone y");
}
