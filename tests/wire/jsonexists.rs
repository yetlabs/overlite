use crate::common::*;

// TestJSONBExists: the jsonb key-existence operators ? / ?| / ?& work (the ?
// no longer collides with a SQLite bind parameter).
#[test]
fn jsonb_exists() {
    let conn = connect(&start_server());
    conn.exec("CREATE TABLE t (id int, data jsonb)", &[]).unwrap();
    conn.exec(r#"INSERT INTO t VALUES (1, '{"a":1,"b":2}'), (2, '{"c":3}')"#, &[])
        .unwrap();

    // ? : key exists.
    let id: i64 = conn.query_row("SELECT id FROM t WHERE data ? 'c'", &[]).scan().unwrap();
    assert_eq!(id, 2);
    let cnt: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE data ? 'a'", &[])
        .scan()
        .unwrap();
    assert_eq!(cnt, 1);

    // ?| : any of the keys.
    let cnt: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE data ?| array['a','z']", &[])
        .scan()
        .unwrap();
    assert_eq!(cnt, 1);

    // ?& : all of the keys.
    let cnt: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE data ?& array['a','b']", &[])
        .scan()
        .unwrap();
    assert_eq!(cnt, 1);
    let cnt: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE data ?& array['a','c']", &[])
        .scan()
        .unwrap();
    assert_eq!(cnt, 0);

    // In the target list too, a boolean as in Postgres.
    let exists: bool = conn
        .query_row("SELECT data ? 'b' FROM t WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert!(exists);
}
