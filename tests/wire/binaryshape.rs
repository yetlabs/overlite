use crate::common::*;

// A type's binary form is a contract: format code 1 says these bytes are numeric_send's.
// SQLite will store `abc` in a numeric column, where Postgres could not, and that value
// has no such bytes. Sending its text under format 1 lied, so a client read garbage or
// failed to decode. It is an error now, for that row, in binary only: the same read in
// text still answers, which is also how to find the row.

fn binary_read(conn: &Conn, sql: &str) -> WireError {
    conn.query_formats(sql, &[BINARY_FORMAT], &[])
        .expect_err("no binary form")
}

#[track_caller]
fn assert_no_binary_form(conn: &Conn, sql: &str, value: &str) {
    let err = binary_read(conn, sql);
    assert_sqlstate(&err, "22P03");
    assert!(pg_error(&err).message.contains(value), "{err}");
    let rows = conn.query_formats(sql, &[TEXT_FORMAT], &[]).expect("text still reads");
    assert_eq!(rows.iter().next().unwrap().cell(0).text(), value);
}

#[test]
fn value_without_binary_form() {
    // Every write refuses these now, as Postgres does: they come from a file written
    // before, where they stay, their columns mixed.
    let dir = temp_dir();
    let path = dir.path().join("test.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        r#"CREATE TABLE t (n DECIMALTEXT, a int[], u UUID, ts TIMESTAMP, tz TIMESTAMPTZ, d DATE, h HSTORE, m "numeric[]");
           INSERT INTO t (n, a, u, ts, tz, d, h, m) VALUES ('abc', '{{1,2},{3}}', 'not-a-uuid', 'soon', 'later', 'someday', 'no map', '{1,abc}');"#,
    )
    .unwrap();
    drop(db);
    let conn = connect_extended(&start_server_on(&path, 0));
    assert_no_binary_form(&conn, "SELECT n FROM t", "abc");
    assert_no_binary_form(&conn, "SELECT u FROM t", "not-a-uuid");
    assert_no_binary_form(&conn, "SELECT ts FROM t", "soon");
    assert_no_binary_form(&conn, "SELECT tz FROM t", "later");
    assert_no_binary_form(&conn, "SELECT d FROM t", "someday");
    assert_no_binary_form(&conn, "SELECT h FROM t", "no map");

    let err = binary_read(&conn, "SELECT a FROM t");
    assert_sqlstate(&err, "22P03");
    let err = binary_read(&conn, "SELECT m FROM t");
    assert_sqlstate(&err, "22P03");

    // The cycle ended cleanly, and the connection goes on.
    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

// The rows before it have gone out, as with any error while a query runs, and the
// error ends the result.
#[test]
fn error_after_the_good_rows() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, n numeric)");
    must_exec(&conn, "INSERT INTO t VALUES (1, '1.5'), (2, 'abc'), (3, '2.5')");
    assert_sqlstate(&binary_read(&conn, "SELECT n FROM t ORDER BY id"), "22P03");

    // In a transaction it fails the transaction.
    must_exec(&conn, "BEGIN");
    assert_sqlstate(&binary_read(&conn, "SELECT n FROM t ORDER BY id"), "22P03");
    let err = conn.exec("SELECT 1", &[]).unwrap_err();
    assert_sqlstate(&err, "25P02");
    must_exec(&conn, "ROLLBACK");
}

// Values that are real ones, whatever they look like, keep their binary form.
#[test]
fn stored_forms_with_a_binary_form() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE t (ts timestamp, tz timestamptz, d date, u uuid, h hstore)",
    );

    // Infinity is a timestamp to Postgres, and a binary parameter is stored as the word.
    conn.exec(
        "INSERT INTO t (ts, tz, d) VALUES ('infinity', '-infinity', 'infinity')",
        &[],
    )
    .unwrap();
    let rows = conn
        .query_formats("SELECT ts, tz, d FROM t", &[BINARY_FORMAT], &[])
        .unwrap();
    let row = rows.iter().next().unwrap();
    assert_eq!(row.cell(0).raw(), i64::MAX.to_be_bytes());
    assert_eq!(row.cell(1).raw(), i64::MIN.to_be_bytes());
    assert_eq!(row.cell(2).raw(), i32::MAX.to_be_bytes());
    must_exec(&conn, "DELETE FROM t");

    // Postgres takes a uuid in braces, and an untyped binary parameter stays bytes.
    must_exec(
        &conn,
        "INSERT INTO t (u) VALUES ('{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}')",
    );
    conn.exec("INSERT INTO t (u) SELECT x'a0eebc999c0b4ef8bb6d6bb9bd380a11'", &[])
        .unwrap();
    let want = hex_bytes("a0eebc999c0b4ef8bb6d6bb9bd380a11");
    let rows = conn.query_formats("SELECT u FROM t", &[BINARY_FORMAT], &[]).unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.cell(0).raw(), want);
    }
    must_exec(&conn, "DELETE FROM t");

    // An hstore bound as text, in hstore's own spelling, is stored as its JSON object.
    conn.exec("INSERT INTO t (h) VALUES ($1)", &[&"\"a\"=>\"1\""]).unwrap();
    let rows = conn.query_formats("SELECT h FROM t", &[BINARY_FORMAT], &[]).unwrap();
    let raw = rows.iter().next().unwrap().cell(0).raw().to_vec();
    assert_eq!(raw, [&[0, 0, 0, 1, 0, 0, 0, 1, b'a', 0, 0, 0, 1, b'1'][..]].concat());
}

fn hex_bytes(s: &str) -> Vec<u8> {
    hex::decode(s).unwrap()
}

// The same class in a number: an integer or a float column read `abc` as 0 and said
// nothing. The first row sets the column's type, as it does for any client.
#[test]
fn numbers_without_binary_form() {
    // A boolean column refuses 'abc' now, as Postgres does: it comes from a file
    // written before, where it stays, its column mixed.
    let dir = temp_dir();
    let path = dir.path().join("test.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE t (id int, i bigint, f double precision, b boolean);
         INSERT INTO t VALUES (1, 1, 1.5, 1), (2, 'abc', 'abc', 'abc');",
    )
    .unwrap();
    drop(db);
    let conn = connect_extended(&start_server_on(&path, 0));
    for (sql, oid) in [
        ("SELECT i FROM t ORDER BY id", 20),
        ("SELECT f FROM t ORDER BY id", 701),
        ("SELECT b FROM t ORDER BY id", 16),
    ] {
        let err = binary_read(&conn, sql);
        assert_sqlstate(&err, "22P03");
        assert!(pg_error(&err).message.contains("abc"), "{err}");
        assert!(pg_error(&err).message.contains(&oid.to_string()), "{err}");
        let rows = conn.query_formats(sql, &[TEXT_FORMAT], &[]).expect("text still reads");
        assert_eq!(rows.len(), 2);
    }

    // A value in another spelling of the type keeps its form.
    must_exec(&conn, "DELETE FROM t WHERE id = 2");
    must_exec(&conn, "INSERT INTO t VALUES (2, ' 7 ', '2.5', 'yes')");
    let rows = conn
        .query_formats("SELECT i, f, b FROM t ORDER BY id", &[BINARY_FORMAT], &[])
        .unwrap();
    let row = rows.iter().nth(1).unwrap();
    assert_eq!(row.cell(0).raw(), 7i64.to_be_bytes());
    assert_eq!(row.cell(1).raw(), 2.5f64.to_be_bytes());
    assert_eq!(row.cell(2).raw(), [1]);
}

// A real that is not a whole number has no int8 bytes, where it used to lose its
// fraction.
#[test]
fn fraction_in_an_integer_column_has_no_binary_form() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, i bigint)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 1), (2, 1.5)");
    assert_sqlstate(&binary_read(&conn, "SELECT i FROM t ORDER BY id"), "22P03");
}

// Rows before the bad one go out, as rows before a failure do in Postgres: asked
// `SELECT 1/(3-g) FROM generate_series(1,5) g`, it sends two rows and 22012.
#[test]
fn rows_before_a_bad_row_go_out() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE t (id int, n numeric)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 1), (2, 2), (3, 'abc')");
    let mut fe = RawConn::dial(&server).unwrap();
    fe.startup("postgres", "test", "").unwrap();
    fe.send_parse("", "SELECT n FROM t ORDER BY id", &[]);
    fe.send_bind("", "", &[], &[], &[BINARY_FORMAT]);
    fe.send_execute("", 0);
    fe.send_sync();
    fe.flush().unwrap();
    let mut rows = 0;
    loop {
        match fe.receive().unwrap() {
            Backend::DataRow(_) => rows += 1,
            Backend::ErrorResponse(e) => {
                assert_eq!(e.code, "22P03");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(rows, 2);
}

// A write's RETURNING is held until the statement is done: asked `UPDATE r SET id = id +
// 10 RETURNING 1/(id-13)`, failing at its third row, Postgres sends no row, and nothing
// is changed. Here the rows went out and the write stayed.
#[test]
fn returning_failure_undoes_the_write() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE t (id int, n numeric)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 1), (2, 2), (3, 'abc')");
    let mut fe = RawConn::dial(&server).unwrap();
    fe.startup("postgres", "test", "").unwrap();
    fe.send_parse("", "UPDATE t SET id = id + 10 RETURNING n", &[]);
    fe.send_bind("", "", &[], &[], &[BINARY_FORMAT]);
    fe.send_describe(b'P', "");
    fe.send_execute("", 0);
    fe.send_sync();
    fe.flush().unwrap();
    let mut rows = 0;
    loop {
        match fe.receive().unwrap() {
            Backend::DataRow(_) => rows += 1,
            Backend::ErrorResponse(e) => assert_eq!(e.code, "22P03"),
            Backend::ReadyForQuery(status) => {
                assert_eq!(status, b'I');
                break;
            }
            _ => {}
        }
    }
    assert_eq!(rows, 0, "no row of a statement that failed");
    assert_eq!(
        query_column(&conn, "SELECT id::text FROM t ORDER BY id", 0),
        ["1", "2", "3"]
    );

    // In a transaction of the client's, it fails that one.
    fe.send_query("BEGIN");
    fe.flush().unwrap();
    fe.wait_ready().unwrap();
    fe.send_parse("", "UPDATE t SET id = id + 10 RETURNING n", &[]);
    fe.send_bind("", "", &[], &[], &[BINARY_FORMAT]);
    fe.send_describe(b'P', "");
    fe.send_execute("", 0);
    fe.send_sync();
    fe.flush().unwrap();
    assert_eq!(fe.wait_ready().unwrap(), Backend::ReadyForQuery(b'E'));
    fe.send_query("ROLLBACK");
    fe.flush().unwrap();
    fe.wait_ready().unwrap();
    assert_eq!(
        query_column(&conn, "SELECT id::text FROM t ORDER BY id", 0),
        ["1", "2", "3"]
    );

    // One that succeeds stays.
    let ext = connect_extended(&server);
    let rows = ext
        .query_formats(
            "UPDATE t SET id = id + 10 WHERE id = 1 RETURNING id",
            &[BINARY_FORMAT],
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        query_column(&conn, "SELECT id::text FROM t ORDER BY t.id", 0),
        ["2", "3", "11"]
    );
}
