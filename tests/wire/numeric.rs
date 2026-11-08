use crate::common::*;

// TestNumericExact: numeric columns store exact decimals (no float loss),
// compare/order numerically, and sum/avg exactly.
#[test]
fn numeric_exact() {
    let conn = connect(&start_server()); // simple protocol (text), like psql

    conn.exec("CREATE TABLE t (id int, amount numeric(20,2))", &[]).unwrap();
    conn.exec(
        "INSERT INTO t VALUES (1,'0.10'),(2,'0.20'),(3,'123456789012345.67'),(4,'9'),(5,'10')",
        &[],
    )
    .unwrap();

    // Exact round-trip — a value beyond float64 precision survives.
    let big: String = conn.query_row("SELECT amount FROM t WHERE id = 3", &[]).scan().unwrap();
    assert_eq!(big, "123456789012345.67");

    // Numeric ordering (not textual: 9 < 10, 0.10 < 9).
    let rows = conn.query("SELECT id FROM t ORDER BY amount", &[]).unwrap();
    let mut order: Vec<i64> = Vec::new();
    for row in &rows {
        let id: i64 = row.scan().unwrap();
        order.push(id);
    }
    assert_eq!(order, [1, 2, 4, 5, 3]);

    // Numeric comparison.
    let cnt: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE amount > '5'", &[])
        .scan()
        .unwrap();
    assert_eq!(cnt, 3); // 9, 10, and the big one

    // Exact sum (0.10 + 0.20 == 0.30, not 0.30000000000000004).
    let sum: String = conn
        .query_row("SELECT sum(amount) FROM t WHERE id IN (1,2)", &[])
        .scan()
        .unwrap();
    assert_eq!(sum, "0.30");

    // Exact avg.
    let avg: String = conn
        .query_row("SELECT avg(amount) FROM t WHERE id IN (4,5)", &[])
        .scan()
        .unwrap();
    assert_eq!(avg, "9.5000000000000000");
}

// The bytes of numeric_send: ndigits, weight, sign, dscale, then base-10000 digits
// aligned on the decimal point. Each expectation is what Postgres sends.
#[test]
fn numeric_binary_send() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, n numeric)");
    for (id, text, want) in [
        (1, "1.5", "0002 0000 0000 0001 0001 1388"),
        (2, "0", "0000 0000 0000 0000"),
        (3, "0.00", "0000 0000 0000 0002"),
        (4, "-0.10", "0001 ffff 4000 0002 03e8"),
        (5, "123456789012345.67", "0005 0003 0000 0002 007b 11d7 22c5 0929 1a2c"),
        (6, "10000", "0001 0001 0000 0000 0001"),
        (7, "0.0001", "0001 ffff 0000 0004 0001"),
        (8, "1.50", "0002 0000 0000 0002 0001 1388"),
        (9, "100000000", "0001 0002 0000 0000 0001"),
        (10, "0.00001", "0001 fffe 0000 0005 03e8"),
        (11, "1e3", "0001 0000 0000 0000 03e8"),
        (12, "1.5e-3", "0001 ffff 0000 0004 000f"),
        (13, "NaN", "0000 0000 c000 0000"),
        (14, "Infinity", "0000 0000 d000 0020"),
        (15, "-Infinity", "0000 0000 f000 0020"),
    ] {
        must_exec(&conn, &format!("INSERT INTO t VALUES ({id}, '{text}')"));
        let sql = format!("SELECT n FROM t WHERE id = {id}");
        let rows = conn.query_formats(&sql, &[BINARY_FORMAT], &[]).unwrap();
        let cell = &rows.rows[0].cells[0];
        assert_eq!((cell.oid, cell.format), (1700, BINARY_FORMAT));
        assert_eq!(hex::encode(cell.raw()), want.replace(' ', ""), "{text}");
    }
}

// As a client reads it in pgx's default mode, which asks numeric in binary.
#[test]
fn numeric_binary_scan() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (n numeric(20,2), i numeric)");
    must_exec(&conn, "INSERT INTO t VALUES ('123456789012345.67', 3), (NULL, NULL)");

    let (n, i): (String, i64) = conn
        .query_row("SELECT n, i FROM t WHERE n IS NOT NULL", &[])
        .scan()
        .unwrap();
    assert_eq!(n, "123456789012345.67");
    assert_eq!(i, 3);
    let f: f64 = conn
        .query_row("SELECT n FROM t WHERE n IS NOT NULL", &[])
        .scan()
        .unwrap();
    assert_eq!(f, 123456789012345.67);
    let none: Option<String> = conn.query_row("SELECT n FROM t WHERE n IS NULL", &[]).scan().unwrap();
    assert_eq!(none, None);

    // A value computed by SQLite and not stored as text.
    let s: String = conn.query_row("SELECT CAST(1.5 AS numeric)", &[]).scan().unwrap();
    assert_eq!(s, "1.5");
}

// numeric[] and decimal[]: the array stays an array, with or without precision, and
// its elements keep their digits.
#[test]
fn numeric_array_columns() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE t (id int, a numeric[], b decimal[], c numeric(10,2)[], d NUMERIC [], e numeric, \"numeric\" text DEFAULT 'numeric(3)')",
    );
    must_exec(
        &conn,
        "INSERT INTO t (id, a, b, c, d, e) VALUES (1, '{1.50,NULL}'::numeric[], ARRAY[2.5], '{3.25}'::numeric[], '{4}'::numeric[], 9)",
    );
    let rows = conn.query("SELECT a, b, c, d, e, \"numeric\" FROM t", &[]).unwrap();
    let oids: Vec<u32> = rows.fields.iter().map(|f| f.data_type_oid).collect();
    assert_eq!(oids, [1231, 1231, 1231, 1231, 1700, 25]);

    let (a, b, c, d): (Vec<Option<String>>, Vec<String>, Vec<String>, Vec<i64>) =
        conn.query_row("SELECT a, b, c, d FROM t", &[]).scan().unwrap();
    assert_eq!(a, [Some("1.50".to_string()), None]);
    assert_eq!(b, ["2.5"]);
    assert_eq!(c, ["3.25"]);
    assert_eq!(d, [4]);
    let s: String = conn.query_row("SELECT \"numeric\" FROM t", &[]).scan().unwrap();
    assert_eq!(s, "numeric(3)", "a literal and a quoted name are not the type");

    // Also on a column altered to it, and in simple-protocol text. Postgres
    // refuses to cast the existing 9 to an array, so the column is emptied first.
    must_exec(&conn, "UPDATE t SET e = NULL");
    must_exec(&conn, "ALTER TABLE t ALTER COLUMN e TYPE numeric[]");
    must_exec(&conn, "UPDATE t SET e = '{7.5}'::numeric[]");
    let v: Vec<String> = conn.query_row("SELECT e FROM t", &[]).scan().unwrap();
    assert_eq!(v, ["7.5"]);
    let rs = conn.pg_exec("SELECT a FROM t").unwrap();
    assert_eq!(rs[0].fields[0].data_type_oid, 1231);
    assert_eq!(rs[0].text(0, 0), "{1.50,NULL}");
}

fn typmod_setup() -> Conn {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE t (id int, n numeric(10,2), m numeric(3), a numeric(5,2)[], v varchar(3), c char(3), va varchar(2)[], f numeric)",
    );
    conn
}

fn stored_text(conn: &Conn, q: &str) -> String {
    let rs = conn.pg_exec(q).unwrap();
    rs[0].text(0, 0)
}

// numeric(p,s) rounds what is written to it, half away from zero, and refuses what does
// not fit, as Postgres does. It stored any value at all.
#[test]
fn numeric_modifier_rounds_on_write() {
    let conn = typmod_setup();
    for (id, value, want) in [
        (1, "1.234", "1.23"),
        (2, "1.235", "1.24"),
        (3, "-1.235", "-1.24"),
        (4, "'2.345'", "2.35"),
        (5, "99999999.994", "99999999.99"),
        (6, "5", "5.00"),
    ] {
        must_exec(&conn, &format!("INSERT INTO t (id, n) VALUES ({id}, {value})"));
        assert_eq!(
            stored_text(&conn, &format!("SELECT n FROM t WHERE id = {id}")),
            want,
            "{value}"
        );
    }
    must_exec(&conn, "INSERT INTO t (id, m) VALUES (9, 999.4)");
    assert_eq!(stored_text(&conn, "SELECT m FROM t WHERE id = 9"), "999");
    // An unmodified numeric takes what it is given, NULL and NaN are no overflow.
    must_exec(&conn, "INSERT INTO t (id, f, n) VALUES (10, 1.23456789, NULL)");
    assert_eq!(stored_text(&conn, "SELECT f FROM t WHERE id = 10"), "1.23456789");
    must_exec(&conn, "INSERT INTO t (id, n) VALUES (11, 'NaN')");

    for sql in [
        "INSERT INTO t (id, n) VALUES (20, 123456789.12)",
        "INSERT INTO t (id, n) VALUES (20, 99999999.995)",
        "INSERT INTO t (id, m) VALUES (20, 999.5)",
        "INSERT INTO t (id, n) VALUES (20, 'Infinity')",
        "UPDATE t SET n = 123456789 WHERE id = 1",
        "INSERT INTO t (id, n) SELECT 20, 123456789",
    ] {
        assert_sqlstate(&conn.exec(sql, &[]).unwrap_err(), "22003");
    }
    assert_sqlstate(
        &conn.exec("INSERT INTO t (id, n) VALUES (20, 'abc')", &[]).unwrap_err(),
        "22P02",
    );
}

#[test]
fn numeric_modifier_on_every_way_of_writing() {
    let conn = typmod_setup();
    let ext = connect_extended(&start_server());
    must_exec(
        &ext,
        "CREATE TABLE u (id int PRIMARY KEY, n numeric(4,1), v varchar(3))",
    );

    must_exec(&conn, "INSERT INTO t (id, n) VALUES (1, 1.111)");
    must_exec(&conn, "UPDATE t SET n = 1.239 WHERE id = 1");
    assert_eq!(stored_text(&conn, "SELECT n FROM t WHERE id = 1"), "1.24");
    let rs = conn
        .pg_exec("UPDATE t SET n = n * 1.111 WHERE id = 1 RETURNING n")
        .unwrap();
    assert_eq!(rs[0].text(0, 0), "1.38");
    let rs = conn
        .pg_exec("INSERT INTO t (id, n) VALUES (50, 5.555) RETURNING n")
        .unwrap();
    assert_eq!(rs[0].text(0, 0), "5.56");
    must_exec(&conn, "INSERT INTO t (id, n) SELECT 51, 1.999");
    assert_eq!(stored_text(&conn, "SELECT n FROM t WHERE id = 51"), "2.00");
    must_exec(
        &conn,
        "INSERT INTO t (id, n, v) VALUES (54, 1.111, 'a'), (55, 2.222, 'b')",
    );
    assert_eq!(stored_text(&conn, "SELECT n FROM t WHERE id = 55"), "2.22");

    // A parameter is a value like any other.
    ext.exec("INSERT INTO u (id, n) VALUES ($1, $2)", &[&1, &"2.22"])
        .unwrap();
    let got: String = ext.query_row("SELECT n::text FROM u", &[]).scan().unwrap();
    assert_eq!(got, "2.2");
    assert_sqlstate(
        &ext.exec("INSERT INTO u (id, n) VALUES ($1, $2)", &[&2, &"99999"])
            .unwrap_err(),
        "22003",
    );
    assert_sqlstate(&ext.exec("UPDATE u SET v = $1", &[&"abcd"]).unwrap_err(), "22001");

    must_exec(
        &ext,
        "INSERT INTO u (id, n) VALUES (1, 2.22) ON CONFLICT (id) DO UPDATE SET n = excluded.n",
    );
    must_exec(
        &ext,
        "INSERT INTO u (id, n) VALUES (1, 3.33) ON CONFLICT (id) DO UPDATE SET n = 4.44",
    );
    let got: String = ext.query_row("SELECT n::text FROM u", &[]).scan().unwrap();
    assert_eq!(got, "4.4");
    assert_sqlstate(
        &ext.exec(
            "INSERT INTO u (id, n) VALUES (1, 99999) ON CONFLICT (id) DO UPDATE SET n = excluded.n",
            &[],
        )
        .unwrap_err(),
        "22003",
    );

    // COPY writes too.
    conn.copy_from(&b"60\t1.999\n"[..], "COPY t (id, n) FROM STDIN")
        .unwrap();
    assert_eq!(stored_text(&conn, "SELECT n FROM t WHERE id = 60"), "2.00");
    assert_sqlstate(
        &conn
            .copy_from(&b"61\t123456789\n"[..], "COPY t (id, n) FROM STDIN")
            .unwrap_err(),
        "22003",
    );
}

#[test]
fn array_modifiers() {
    let conn = typmod_setup();
    must_exec(&conn, "INSERT INTO t (id, a) VALUES (20, '{1.234,2.345}')");
    assert_eq!(stored_text(&conn, "SELECT a FROM t WHERE id = 20"), "{1.23,2.35}");
    must_exec(&conn, "INSERT INTO t (id, a) VALUES (21, ARRAY[1.235])");
    assert_eq!(stored_text(&conn, "SELECT a FROM t WHERE id = 21"), "{1.24}");
    must_exec(&conn, "INSERT INTO t (id, a) VALUES (23, '{NULL,1.111}')");
    assert_eq!(stored_text(&conn, "SELECT a FROM t WHERE id = 23"), "{NULL,1.11}");
    assert_sqlstate(
        &conn
            .exec("INSERT INTO t (id, a) VALUES (22, '{1000.1}')", &[])
            .unwrap_err(),
        "22003",
    );
    assert_sqlstate(
        &conn
            .exec("INSERT INTO t (id, va) VALUES (40, '{abc}')", &[])
            .unwrap_err(),
        "22001",
    );
    must_exec(&conn, "INSERT INTO t (id, va) VALUES (41, '{ab}')");
}

// A string past its length is 22001, unless what goes is spaces.
#[test]
fn varchar_and_char_modifiers() {
    let conn = typmod_setup();
    for sql in [
        "INSERT INTO t (id, v) VALUES (30, 'abcd')",
        "INSERT INTO t (id, v) VALUES (33, 'ação')",
        "INSERT INTO t (id, c) VALUES (35, 'abcd')",
        "INSERT INTO t (id, v) SELECT 53, 'abcdef'",
        "UPDATE t SET v = 'abcde'",
    ] {
        must_exec(&conn, "INSERT INTO t (id, v) VALUES (0, 'x')");
        assert_sqlstate(&conn.exec(sql, &[]).unwrap_err(), "22001");
    }
    must_exec(&conn, "INSERT INTO t (id, v, c) VALUES (31, 'abc ', 'abc ')");
    assert_eq!(stored_text(&conn, "SELECT v FROM t WHERE id = 31"), "abc");
    must_exec(&conn, "INSERT INTO t (id, v) VALUES (34, 'açã')");
    assert_eq!(stored_text(&conn, "SELECT 'abcdef'::varchar(3)"), "abc");
}

// A stored row is never touched: this is the migration story. Only what is written is
// held to the modifier.
#[test]
fn stored_rows_stay_as_they_are() {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    {
        let server = start_server_on(&db, 0);
        let conn = connect(&server);
        must_exec(&conn, "CREATE TABLE t (id int, n numeric(10,2), v varchar(3))");
    }
    // As an older server wrote it: past the modifier, with SQLite alone, since every
    // write of this one fits it.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("INSERT INTO t VALUES (1, '1.23456', 'abcdef')")
        .unwrap();
    let server = start_server_on(&db, 0);
    let conn = connect(&server);
    assert_eq!(stored_text(&conn, "SELECT n FROM t"), "1.23456");
    assert_eq!(stored_text(&conn, "SELECT v FROM t"), "abcdef");
    must_exec(&conn, "UPDATE t SET id = 2");
    assert_eq!(stored_text(&conn, "SELECT n FROM t"), "1.23456");
}
