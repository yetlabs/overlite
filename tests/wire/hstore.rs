use crate::common::*;

// TestHstore: an hstore column stores a key/value map (as JSON), round-trips as
// hstore text, and supports -> (value) and ? (key existence).
#[test]
fn hstore() {
    let conn = connect(&start_server());
    conn.exec("CREATE TABLE t (id int, h hstore)", &[]).unwrap();
    conn.exec(
        "INSERT INTO t VALUES (1, 'a=>1, b=>2'::hstore), (2, 'x=>10'::hstore)",
        &[],
    )
    .unwrap();

    // Output in hstore text form (keys sorted for stability).
    let s: String = conn.query_row("SELECT h FROM t WHERE id = 1", &[]).scan().unwrap();
    assert_eq!(s, r#""a"=>"1", "b"=>"2""#);

    // -> value access, as text.
    let v: String = conn
        .query_row("SELECT h -> 'a' FROM t WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert_eq!(v, "1");

    // ? key existence.
    let yes: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE h ? 'b'", &[])
        .scan()
        .unwrap();
    let no: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE h ? 'z'", &[])
        .scan()
        .unwrap();
    assert_eq!(yes, 1);
    assert_eq!(no, 0);

    // akeys / avals (sorted).
    let keys: String = conn
        .query_row("SELECT akeys(h) FROM t WHERE id = 1", &[])
        .scan()
        .unwrap();
    let vals: String = conn
        .query_row("SELECT avals(h) FROM t WHERE id = 1", &[])
        .scan()
        .unwrap();
    // text[] arrays, as Postgres sends them.
    assert_eq!(keys, "{a,b}");
    assert_eq!(vals, "{1,2}");
}

// hstore_send: the pair count, then each key and value with its length, -1 for NULL.
#[test]
fn hstore_binary_send() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (h hstore)");
    must_exec(&conn, "INSERT INTO t VALUES ('a=>1, b=>NULL'::hstore)");
    let rows = conn.query_formats("SELECT h FROM t", &[BINARY_FORMAT], &[]).unwrap();
    let cell = &rows.rows[0].cells[0];
    assert_eq!(cell.format, BINARY_FORMAT);
    assert_eq!(
        hex::encode(cell.raw()),
        concat!(
            "00000002", "00000001", "61", "00000001", "31", "00000001", "62", "ffffffff"
        )
    );
}
