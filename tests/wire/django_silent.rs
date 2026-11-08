//! Answers Django's test suite found wrong without an error, each held to what
//! Postgres 17 answers, or to the error that says it cannot be answered here.

use crate::common::*;

/// The type OIDs and the text cells of one simple-protocol statement.
#[track_caller]
fn texts(conn: &Conn, sql: &str) -> (Vec<u32>, Vec<Vec<Option<String>>>) {
    let mut results = conn.pg_exec(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let r = results.remove(0);
    let oids = r.fields.iter().map(|f| f.data_type_oid).collect();
    let rows = r
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| c.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()))
                .collect()
        })
        .collect();
    (oids, rows)
}

/// The cells of the first row, NULL as "NULL".
#[track_caller]
fn row(conn: &Conn, sql: &str) -> Vec<String> {
    let (_, rows) = texts(conn, sql);
    rows.first()
        .unwrap_or_else(|| panic!("{sql}: no row"))
        .iter()
        .map(|c| c.clone().unwrap_or_else(|| "NULL".into()))
        .collect()
}

/// The first column of every row, NULL as "NULL".
#[track_caller]
fn col(conn: &Conn, sql: &str) -> Vec<String> {
    let (_, rows) = texts(conn, sql);
    rows.iter()
        .map(|r| r[0].clone().unwrap_or_else(|| "NULL".into()))
        .collect()
}

/// The SQLSTATE sql fails with; it must fail.
#[track_caller]
fn fails(conn: &Conn, sql: &str) -> String {
    match conn.exec(sql, &[]) {
        Ok(_) => panic!("{sql}: succeeded"),
        Err(e) => e.code().to_string(),
    }
}

// ALTER TABLE ADD CONSTRAINT ... CHECK went to SQLite as written, so a `::` cast, a
// jsonb operator or an interval made the rebuild fail, and the failure was taken
// for a constraint SQLite cannot hold: the statement answered ALTER TABLE and the
// constraint was nowhere. NOT VALID was dropped the same way.
#[test]
fn an_added_check_is_enforced_and_listed() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        r#"CREATE TABLE ck (id int, name varchar(100), pink int, j jsonb, d interval, "Q x" int)"#,
    );
    for (name, expr) in [
        ("ck_a", r#""name"::text LIKE 'A%'"#),
        ("ck_b", "CASE WHEN pink > 10 THEN false ELSE true END"),
        ("ck_c", "pink IN (1, 2, 3, 4, 5)"),
        ("ck_d", "j ? 'k' OR j IS NULL"),
        ("ck_e", "d < interval '2 days' OR d IS NULL"),
        ("\"constraint\"", r#""Q x"::integer > 0 OR "Q x" IS NULL"#),
    ] {
        must_exec(&conn, &format!("ALTER TABLE ck ADD CONSTRAINT {name} CHECK ({expr})"));
    }
    assert_eq!(
        col(
            &conn,
            "SELECT conname FROM pg_constraint WHERE conrelid = 'ck'::regclass AND contype = 'c' ORDER BY 1"
        ),
        ["ck_a", "ck_b", "ck_c", "ck_d", "ck_e", "constraint"]
    );
    must_exec(&conn, "INSERT INTO ck (id, name, pink) VALUES (1, 'Al', 1)");
    let err = conn
        .exec("INSERT INTO ck (id, name, pink) VALUES (2, 'Bob', 1)", &[])
        .unwrap_err();
    assert_pg_error(&err, "23514", r#"violates check constraint "ck_a""#);
    for bad in [
        "INSERT INTO ck (id, name, pink) VALUES (2, 'Bob', 1)",
        "INSERT INTO ck (id, name, pink) VALUES (2, 'Al', 11)",
        "INSERT INTO ck (id, name, pink) VALUES (2, 'Al', 6)",
        r#"INSERT INTO ck (id, name, pink, j) VALUES (2, 'Al', 1, '{"x": 1}')"#,
        "INSERT INTO ck (id, name, pink, d) VALUES (2, 'Al', 1, '3 days')",
        r#"INSERT INTO ck (id, name, pink, "Q x") VALUES (2, 'Al', 1, -1)"#,
    ] {
        assert_eq!(fails(&conn, bad), "23514", "{bad}");
    }
    // A row already there that the constraint refuses fails the statement.
    let err = conn
        .exec("ALTER TABLE ck ADD CONSTRAINT ck_f CHECK (pink::int > 1)", &[])
        .unwrap_err();
    assert_pg_error(
        &err,
        "23514",
        r#"check constraint "ck_f" of relation "ck" is violated by some row"#,
    );
    assert_eq!(
        row(&conn, "SELECT count(*) FROM pg_constraint WHERE conname = 'ck_f'"),
        ["0"]
    );
    // A column's CHECK lets its NULL through: `?` over NULL is NULL.
    must_exec(&conn, "ALTER TABLE ck ADD COLUMN z jsonb CHECK (z ? 'a')");
    assert_eq!(
        fails(
            &conn,
            r#"INSERT INTO ck (id, name, pink, z) VALUES (3, 'Al', 1, '{"b": 1}')"#
        ),
        "23514"
    );
}

// NOT VALID holds new rows to the constraint and leaves the rows already there;
// VALIDATE CONSTRAINT then checks those.
#[test]
fn a_not_valid_check_holds_new_rows_until_validated() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE nv (id int, pink int)");
    must_exec(&conn, "INSERT INTO nv VALUES (1, 1)");
    must_exec(
        &conn,
        "ALTER TABLE nv ADD CONSTRAINT nv_pink CHECK (pink::int >= 4) NOT VALID",
    );
    assert_eq!(fails(&conn, "INSERT INTO nv VALUES (2, 2)"), "23514");
    must_exec(&conn, "INSERT INTO nv VALUES (3, 5)");
    assert_eq!(fails(&conn, "UPDATE nv SET pink = 0 WHERE id = 3"), "23514");
    assert_eq!(
        row(
            &conn,
            "SELECT convalidated FROM pg_constraint WHERE conname = 'nv_pink'"
        ),
        ["f"]
    );
    // A rebuild for another reason keeps the old row and the mark.
    must_exec(&conn, "ALTER TABLE nv ADD COLUMN extra timestamptz DEFAULT now()");
    assert_eq!(col(&conn, "SELECT id FROM nv ORDER BY id"), ["1", "3"]);
    let err = conn
        .exec("ALTER TABLE nv VALIDATE CONSTRAINT nv_pink", &[])
        .unwrap_err();
    assert_pg_error(
        &err,
        "23514",
        r#"check constraint "nv_pink" of relation "nv" is violated by some row"#,
    );
    must_exec(&conn, "DELETE FROM nv WHERE pink < 4");
    must_exec(&conn, "ALTER TABLE nv VALIDATE CONSTRAINT nv_pink");
    assert_eq!(
        row(
            &conn,
            "SELECT convalidated FROM pg_constraint WHERE conname = 'nv_pink'"
        ),
        ["t"]
    );
    assert_eq!(fails(&conn, "INSERT INTO nv (id, pink) VALUES (4, 1)"), "23514");
}

// A range's bounds compared only as numbers, so a date or a timestamp was in no
// range, and a range was stored and cast as written, `[1,5]` beside `[1,6)`.
#[test]
fn range_bounds_compare_by_type_in_canonical_ranges() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE rg (id int, ints int4range, d date, dr daterange, tr tstzrange, nr numrange)",
    );
    must_exec(
        &conn,
        r#"INSERT INTO rg VALUES (1, '[-5,0]', '2015-05-05', '[2015-01-01,2015-06-01]', '["2020-01-01 10:00+02",)', '(1.5,2.5]')"#,
    );
    assert_eq!(
        row(&conn, "SELECT ints, dr, tr, nr FROM rg"),
        [
            "[-5,1)",
            "[2015-01-01,2015-06-02)",
            r#"["2020-01-01 08:00:00+00",)"#,
            "(1.5,2.5]"
        ]
    );
    assert_eq!(row(&conn, "SELECT upper(ints), lower(ints) FROM rg"), ["1", "-5"]);
    for q in [
        "SELECT count(*) FROM rg WHERE upper(ints) = 1",
        "SELECT count(*) FROM rg WHERE d <@ '[2015-01-01,2015-05-06)'::daterange",
        "SELECT count(*) FROM rg WHERE dr @> '2015-03-01'::date",
        "SELECT count(*) FROM rg WHERE tr @> '2020-01-01 09:00+00'::timestamptz",
    ] {
        assert_eq!(row(&conn, q), ["1"], "{q}");
    }
    assert_eq!(
        row(
            &conn,
            "SELECT '[1,5]'::int4range, int4range(1, 5, '[]'), daterange('2015-01-01', '2015-01-05', '(]'), 'EMPTY'::int4range"
        ),
        ["[1,6)", "[1,6)", "[2015-01-02,2015-01-06)", "empty"]
    );
    assert_eq!(fails(&conn, "SELECT '[5,1)'::int4range"), "22000");
    assert_eq!(fails(&conn, "SELECT '[a,1)'::int4range"), "22P02");
}

// ALTER TABLE ADD CONSTRAINT ... EXCLUDE was taken without effect and the CREATE
// TABLE form was a syntax error. Each is now a pair of triggers that refuse a row
// conflicting with one already there.
#[test]
fn an_exclusion_constraint_refuses_conflicting_rows() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE ex (id int, r int4range, CONSTRAINT ex_r EXCLUDE USING GIST (r WITH &&))",
    );
    must_exec(&conn, "INSERT INTO ex VALUES (1, '[1,5)')");
    assert_eq!(fails(&conn, "INSERT INTO ex VALUES (2, '[4,8)')"), "23P01");
    must_exec(&conn, "INSERT INTO ex VALUES (2, '[5,8)')");
    assert_eq!(fails(&conn, "UPDATE ex SET r = '[3,9)' WHERE id = 2"), "23P01");
    must_exec(&conn, "UPDATE ex SET r = '[5,9)' WHERE id = 2");

    must_exec(
        &conn,
        "CREATE TABLE exb (id int, ints int4range, decimals numeric, cancelled bool DEFAULT false)",
    );
    must_exec(&conn, "INSERT INTO exb (id, ints) VALUES (1, '[1,5)'), (2, '[10,20)')");
    must_exec(
        &conn,
        r#"ALTER TABLE exb ADD CONSTRAINT "exb_x" EXCLUDE USING GIST ("ints" range_ops WITH -|-) INCLUDE ("decimals") WHERE (NOT "cancelled") DEFERRABLE INITIALLY DEFERRED"#,
    );
    assert_eq!(fails(&conn, "INSERT INTO exb (id, ints) VALUES (3, '[5,10)')"), "23P01");
    must_exec(
        &conn,
        "INSERT INTO exb (id, ints, cancelled) VALUES (3, '[5,10)', true)",
    );
    let (_, rows) = texts(
        &conn,
        "SELECT conname, contype, condeferrable, condeferred FROM pg_constraint WHERE conrelid IN ('ex'::regclass, 'exb'::regclass) ORDER BY 1",
    );
    assert_eq!(
        rows,
        [
            ["ex_r", "x", "f", "f"].map(|s| Some(s.to_string())),
            ["exb_x", "x", "t", "t"].map(|s| Some(s.to_string())),
        ]
    );
    must_exec(&conn, "ALTER TABLE exb DROP CONSTRAINT exb_x");
    must_exec(&conn, "INSERT INTO exb (id, ints) VALUES (4, '[5,10)')");

    // Expressions, an equality and a predicate, as Django's HotelReservation has them.
    must_exec(
        &conn,
        r#"CREATE TABLE hr (id int, room int, "start" timestamptz, "end" timestamptz, cancelled boolean, req jsonb)"#,
    );
    must_exec(
        &conn,
        r#"ALTER TABLE hr ADD CONSTRAINT hr_x EXCLUDE USING GIST ((TSTZRANGE("start", "end", '[)')) range_ops WITH &&, "room" WITH =, (("req" ->> 'smoking')) WITH =) WHERE (NOT "cancelled")"#,
    );
    let ins = |id: i32, room: i32, from: &str, to: &str, smoking: &str| {
        format!(
            r#"INSERT INTO hr VALUES ({id}, {room}, '2020-01-01 {from}+00', '2020-01-01 {to}+00', false, '{{"smoking": "{smoking}"}}')"#
        )
    };
    must_exec(&conn, &ins(1, 1, "10:00", "12:00", "y"));
    assert_eq!(fails(&conn, &ins(2, 1, "11:00", "13:00", "y")), "23P01");
    must_exec(&conn, &ins(3, 1, "11:00", "13:00", "n"));
    must_exec(&conn, &ins(4, 2, "11:00", "13:00", "y"));
    must_exec(&conn, &ins(5, 1, "12:00", "13:00", "y"));

    // Rows already in conflict refuse the constraint; an operator it cannot hold is 0A000.
    must_exec(&conn, "CREATE TABLE dup (id int, n int)");
    must_exec(&conn, "INSERT INTO dup VALUES (1, 1), (2, 1)");
    assert_eq!(
        fails(&conn, "ALTER TABLE dup ADD CONSTRAINT dup_x EXCLUDE (n WITH =)"),
        "23P01"
    );
    assert_eq!(
        fails(
            &conn,
            "ALTER TABLE dup ADD CONSTRAINT dup_y EXCLUDE USING gist (n WITH <)"
        ),
        "0A000"
    );
    assert_eq!(
        row(
            &conn,
            "SELECT count(*) FROM pg_constraint WHERE conrelid = 'dup'::regclass"
        ),
        ["0"]
    );
}

// SQLite makes one index for two UNIQUE constraints over the same key, and the
// catalog listed constraints by index: the second was nowhere until the first went.
#[test]
fn a_second_unique_over_a_key_is_listed() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE uq (id int, a int UNIQUE, b int, c int)");
    must_exec(&conn, "ALTER TABLE uq ADD CONSTRAINT uq_a_uniq UNIQUE (a)");
    must_exec(&conn, "ALTER TABLE uq ADD CONSTRAINT uq_bc_1 UNIQUE (b, c)");
    must_exec(&conn, "ALTER TABLE uq ADD CONSTRAINT uq_bc_2 UNIQUE (b, c) DEFERRABLE");
    let names = "SELECT conname FROM pg_constraint WHERE conrelid = 'uq'::regclass ORDER BY 1";
    assert_eq!(col(&conn, names), ["uq_a_key", "uq_a_uniq", "uq_bc_1", "uq_bc_2"]);
    assert_eq!(
        col(
            &conn,
            "SELECT condeferrable FROM pg_constraint WHERE conrelid = 'uq'::regclass ORDER BY conname"
        ),
        ["f", "f", "f", "t"]
    );
    must_exec(&conn, "ALTER TABLE uq DROP CONSTRAINT uq_a_key");
    must_exec(&conn, "ALTER TABLE uq DROP CONSTRAINT uq_bc_1");
    assert_eq!(col(&conn, names), ["uq_a_uniq", "uq_bc_2"]);
    must_exec(&conn, "INSERT INTO uq VALUES (1, 1, 1, 1)");
    assert_eq!(fails(&conn, "INSERT INTO uq VALUES (2, 1, 2, 2)"), "23505");
    assert_eq!(fails(&conn, "INSERT INTO uq VALUES (3, 3, 1, 1)"), "23505");
}

// A numeric that was not a column compared with a number as text with a number, which
// SQLite orders by class: `23.09::numeric(6,2) > 40` was true, a GREATEST, a CASE or a
// SUM of numerics equal to no number. Arithmetic outside the select list was a double's,
// and a literal past a double's digits lost them on its way into a column.
#[test]
fn numerics_compare_and_compute_as_numbers() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE nm (id int, n1 numeric(10,2), n2 numeric(10,2), big numeric(40,30), i int, f float8)",
    );
    must_exec(
        &conn,
        "INSERT INTO nm VALUES (1, 1.1, 1.2, 0.100000000000000000000000000005, 5, 2.5), (2, 9.5, 10.25, 1, 50, 0.1)",
    );
    assert_eq!(
        row(
            &conn,
            "SELECT 23.09::numeric(6,2) > 40, 1::numeric > 40, '1.5'::numeric = 1.5, 40 < 23.09::numeric"
        ),
        ["f", "f", "t", "f"]
    );
    for (q, want) in [
        ("SELECT id FROM nm WHERE GREATEST(n1, n2) = 1.2", vec!["1"]),
        ("SELECT id FROM nm WHERE LEAST(n1, n2) = 1.1", vec!["1"]),
        ("SELECT id FROM nm GROUP BY id HAVING SUM(n1) > 5", vec!["2"]),
        (
            "SELECT id FROM nm WHERE CASE WHEN id = 1 THEN n1 ELSE NULL END = 1.1",
            vec!["1"],
        ),
        (
            "SELECT id FROM nm WHERE CASE WHEN id = 1 THEN n1 + 0.1 END = 1.2",
            vec!["1"],
        ),
        ("SELECT id FROM nm WHERE i > 9.5::numeric", vec!["2"]),
        ("SELECT id FROM nm WHERE f < 2.5::numeric", vec!["2"]),
        ("SELECT id FROM nm WHERE n1 - 0.4 = 0.7", vec!["1"]),
        ("SELECT id FROM nm ORDER BY n1 + 0 DESC", vec!["2", "1"]),
        ("SELECT id FROM nm ORDER BY GREATEST(n1, n2)", vec!["1", "2"]),
        ("SELECT id FROM nm WHERE coalesce(n2, 0) > 9", vec!["2"]),
    ] {
        assert_eq!(col(&conn, q), want, "{q}");
    }
    assert_eq!(
        row(
            &conn,
            "SELECT GREATEST(n1, n2), LEAST(n1, n2), n2 - 1.1, n1 * 3, n1 / 3 FROM nm WHERE id = 2"
        ),
        ["10.25", "9.50", "9.15", "28.50", "3.1666666666666667"]
    );
    assert_eq!(
        row(&conn, "SELECT sum(n1), avg(n1) FROM nm"),
        ["10.60", "5.3000000000000000"]
    );
    assert_eq!(
        row(&conn, "SELECT big FROM nm WHERE id = 1"),
        ["0.100000000000000000000000000005"]
    );
    assert_eq!(
        row(
            &conn,
            "SELECT 12345678901234567890.123456789::numeric, 0.100000000000000000000000000005 > 0.1"
        ),
        ["12345678901234567890.123456789", "t"]
    );
    // Arithmetic in a write is exact, as Django's F() - Decimal('0.4') is.
    must_exec(&conn, "UPDATE nm SET n1 = (n1 - 0.4), big = big - 0.4 WHERE id = 1");
    assert_eq!(
        row(&conn, "SELECT n1, big FROM nm WHERE id = 1"),
        ["0.70", "-0.299999999999999999999999999995"]
    );
    // A parameter compares with a numeric expression as a number.
    let ext = connect_extended(&server);
    let rows = ext
        .query("SELECT id FROM nm GROUP BY id HAVING SUM(n2) > $1 ORDER BY id", &[&"5"])
        .unwrap();
    assert_eq!(rows.len(), 1);
}

// Products of intervals were SQLite's numbers (`interval '1 day' * 5` was 5), an
// interval compared with another as text (`1 mon` before `29 days`), a date with a
// timestamp as text, a scalar subquery's timestamp went out as text, and AT TIME
// ZONE with an offset or a zone name in another case was NULL.
#[test]
fn intervals_dates_and_zones_compute_as_postgres() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE dt (id int, d date, d2 date, ts timestamp, du interval, tz timestamptz)",
    );
    must_exec(
        &conn,
        "INSERT INTO dt VALUES (1, '2010-06-25', '2010-06-30', '2010-06-25 12:15:30.747', '1 mon', '2010-06-25 12:00+00'), \
         (2, '2010-06-26', '2010-06-26', '2010-06-24 00:00', '29 days', '2010-06-25 00:00+00')",
    );
    assert_eq!(
        row(
            &conn,
            "SELECT interval '1 day' * (d2 - d), 2 * du, du / 2, du * 1.5, '1 day 2 hours'::interval * 1.5 FROM dt WHERE id = 1"
        ),
        ["5 days", "2 mons", "15 days", "1 mon 15 days", "1 day 15:00:00"]
    );
    assert_eq!(fails(&conn, "SELECT ts * '0:00:00'::interval FROM dt"), "42883");
    assert_eq!(
        row(
            &conn,
            "SELECT interval '1 day' * CASE WHEN false THEN 1 END, '1 day 01:00:00.1'::interval * 3.2, '1 day'::interval / 3"
        ),
        ["NULL", "3 days 08:00:00.32", "08:00:00"]
    );
    for (q, want) in [
        (
            "SELECT id FROM dt WHERE (interval '1 day' * (d2 - d)) >= '5 days'::interval",
            vec!["1"],
        ),
        ("SELECT id FROM dt WHERE du > '29 days 23:00' ORDER BY id", vec!["1"]),
        ("SELECT id FROM dt ORDER BY du", vec!["2", "1"]),
        (
            "SELECT id FROM dt WHERE d >= (ts - '12:15:30.747'::interval) ORDER BY id",
            vec!["1", "2"],
        ),
        (
            "SELECT id FROM dt WHERE ts::date = DATE_TRUNC('day', ts) ORDER BY id",
            vec!["1", "2"],
        ),
        ("SELECT id FROM dt WHERE d < tz ORDER BY id", vec!["1"]),
        (
            "SELECT id FROM dt WHERE d::date = '2010-06-25 00:00+00'::timestamptz",
            vec!["1"],
        ),
    ] {
        assert_eq!(col(&conn, q), want, "{q}");
    }
    let (oids, rows) = texts(
        &conn,
        "SELECT (SELECT max(tz) FROM dt), DATE_TRUNC('year', (SELECT MAX(ts) FROM dt)), COALESCE(MIN(ts) FILTER (WHERE false), '1970-01-01'::timestamp) FROM dt",
    );
    assert_eq!(oids, [1184, 1114, 1114]);
    assert_eq!(rows[0][0].as_deref(), Some("2010-06-25 12:00:00+00"));
    assert_eq!(
        row(
            &conn,
            "SELECT DATE_TRUNC('year', '2015-06-15 14:30:50'::timestamptz AT TIME ZONE '+10:00'), \
             EXTRACT(DAY FROM '2015-06-15 23:30:50'::timestamptz AT TIME ZONE '+10:00'), \
             EXTRACT(DAY FROM '2015-06-15 23:30:50'::timestamptz AT TIME ZONE 'Australia/MELBOURNE')"
        ),
        ["2015-01-01 00:00:00", "15", "16"]
    );
}

// SET TIME ZONE was taken without effect: SHOW said UTC, no ParameterStatus came,
// a timestamptz went out in UTC and one given without an offset was read as UTC.
#[test]
fn the_session_time_zone_reads_and_writes_timestamptz() {
    let server = start_server();
    let mut fe = RawConn::dial(&server).unwrap();
    fe.startup("postgres", "test", "").unwrap();
    assert_eq!(fe.params.get("TimeZone").map(String::as_str), Some("UTC"));
    fe.send_query("SET TIME ZONE 'africa/nairobi'");
    fe.flush().unwrap();
    fe.wait_ready().unwrap();
    assert_eq!(fe.params.get("TimeZone").map(String::as_str), Some("Africa/Nairobi"));
    // Django sets it with set_config.
    fe.send_query("SELECT set_config('TimeZone', 'Europe/Paris', false)");
    fe.flush().unwrap();
    fe.wait_ready().unwrap();
    assert_eq!(fe.params.get("TimeZone").map(String::as_str), Some("Europe/Paris"));

    let conn = connect(&server);
    must_exec(&conn, "SET TIME ZONE 'Africa/Nairobi'");
    assert_eq!(row(&conn, "SHOW TimeZone"), ["Africa/Nairobi"]);
    assert_eq!(row(&conn, "SELECT current_setting('TimeZone')"), ["Africa/Nairobi"]);
    must_exec(&conn, "CREATE TABLE tzs (id int, dt timestamptz, d date)");
    must_exec(
        &conn,
        "INSERT INTO tzs VALUES (1, '2011-09-01 13:20:30+03', '2011-09-01'), (2, '2011-09-01 13:20:30', '2011-09-02')",
    );
    assert_eq!(
        row(
            &conn,
            "SELECT dt, dt::text, EXTRACT(HOUR FROM dt), DATE_TRUNC('day', dt), dt::date FROM tzs WHERE id = 2"
        ),
        [
            "2011-09-01 13:20:30+03",
            "2011-09-01 13:20:30+03",
            "13",
            "2011-09-01 00:00:00+03",
            "2011-09-01"
        ]
    );
    assert_eq!(
        col(&conn, "SELECT id FROM tzs WHERE dt = '2011-09-01 13:20:30' ORDER BY id"),
        ["1", "2"]
    );
    assert_eq!(col(&conn, "SELECT id FROM tzs WHERE d < dt ORDER BY id"), ["1"]);
    assert_eq!(
        row(&conn, "SELECT '2011-09-01'::date::timestamptz"),
        ["2011-09-01 00:00:00+03"]
    );
    assert_eq!(
        row(
            &conn,
            "SELECT current_date = (now() AT TIME ZONE 'Africa/Nairobi')::date"
        ),
        ["t"]
    );
    // A timestamp beside a timestamptz in COALESCE is one in the zone; a date compared
    // with DATE_TRUNC's timestamptz is the zone's midnight.
    assert_eq!(
        row(
            &conn,
            "SELECT COALESCE(MIN(dt) FILTER (WHERE false), '2011-09-01 13:20:30'::timestamp) FROM tzs"
        ),
        ["2011-09-01 13:20:30+03"]
    );
    assert_eq!(
        row(&conn, "SELECT count(*) FROM tzs WHERE dt::date = DATE_TRUNC('day', dt)"),
        ["2"]
    );
    assert_eq!(
        row(&conn, "SELECT count(*) FROM tzs WHERE d = DATE_TRUNC('day', d)"),
        ["2"]
    );
    // A rollback undoes a SET, the end of the transaction a SET LOCAL.
    must_exec(&conn, "BEGIN");
    must_exec(&conn, "SET TIME ZONE 'Asia/Kolkata'");
    assert_eq!(
        row(&conn, "SELECT dt FROM tzs WHERE id = 1"),
        ["2011-09-01 15:50:30+05:30"]
    );
    must_exec(&conn, "ROLLBACK");
    assert_eq!(row(&conn, "SHOW timezone"), ["Africa/Nairobi"]);
    must_exec(&conn, "BEGIN");
    must_exec(&conn, "SET LOCAL TIME ZONE 'Asia/Kolkata'");
    must_exec(&conn, "COMMIT");
    assert_eq!(row(&conn, "SHOW timezone"), ["Africa/Nairobi"]);
    // A timestamptz copied from a column keeps its instant: it is stored as bare UTC,
    // which is no wall clock of the session's zone.
    must_exec(&conn, "UPDATE tzs SET dt = dt");
    must_exec(&conn, "INSERT INTO tzs (id, dt) SELECT 9, dt FROM tzs WHERE id = 1");
    assert_eq!(
        col(&conn, "SELECT dt FROM tzs WHERE id IN (1, 9) ORDER BY id"),
        ["2011-09-01 13:20:30+03", "2011-09-01 13:20:30+03"]
    );
    assert_eq!(fails(&conn, "SET TIME ZONE 'Nowhere/Land'"), "22023");
    must_exec(&conn, "RESET timezone");
    assert_eq!(
        row(&conn, "SELECT dt FROM tzs WHERE id = 1"),
        ["2011-09-01 10:20:30+00"]
    );
    // Another session keeps its own.
    let other = connect(&server);
    assert_eq!(row(&other, "SHOW TimeZone"), ["UTC"]);
}

// now() was the statement's clock, so it moved on within a transaction, where
// Postgres holds it at the transaction's start.
#[test]
fn now_holds_for_the_transaction() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "BEGIN");
    let first = row(&conn, "SELECT now(), statement_timestamp()");
    must_exec(&conn, "SELECT pg_sleep(0.02)");
    let second = row(
        &conn,
        "SELECT now(), transaction_timestamp(), CURRENT_TIMESTAMP, statement_timestamp()",
    );
    assert_eq!(first[0], second[0]);
    assert_eq!(second[0], second[1]);
    assert_eq!(second[0], second[2]);
    assert_ne!(first[1], second[3], "statement_timestamp moves with each statement");
    must_exec(&conn, "COMMIT");
    must_exec(&conn, "SELECT pg_sleep(0.02)");
    assert_ne!(row(&conn, "SELECT now()")[0], first[0]);
    assert_eq!(row(&conn, "SELECT now() = statement_timestamp()"), ["t"]);
}
