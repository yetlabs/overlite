use crate::common::*;

// lateral: LATERAL over a set-returning function works (the keyword is
// stripped; SQLite treats table-valued functions as implicitly lateral).
#[test]
fn lateral() {
    let conn = connect(&start_server());
    conn.exec(r#"CREATE TABLE docs (id int, data jsonb)"#, &[]).unwrap();
    conn.exec(r#"INSERT INTO docs VALUES (1,'[10,20]'), (2,'[30]')"#, &[])
        .unwrap();

    let rows = conn
        .query(
            r#"SELECT d.id, x.value::int FROM docs d, LATERAL jsonb_array_elements(d.data) x ORDER BY d.id, x.value"#,
            &[],
        )
        .unwrap();
    let got = rows.collect_rows::<(i64, i64)>().unwrap();
    assert_eq!(got, [(1, 10), (1, 20), (2, 30)]);
}

// default_nextval: DEFAULT nextval('seq') in a column definition assigns
// sequence values on insert when the column is omitted.
#[test]
fn default_nextval() {
    let conn = connect(&start_server());
    conn.exec(r#"CREATE SEQUENCE s START 100"#, &[]).unwrap();
    conn.exec(r#"CREATE TABLE t (id int DEFAULT nextval('s'), name text)"#, &[])
        .unwrap();
    conn.exec(r#"INSERT INTO t (name) VALUES ('a')"#, &[]).unwrap();
    conn.exec(r#"INSERT INTO t (name) VALUES ('b')"#, &[]).unwrap();

    let rows = conn.query(r#"SELECT id FROM t ORDER BY id"#, &[]).unwrap();
    let ids = rows.collect_rows::<i64>().unwrap();
    assert_eq!(ids, [100, 101]);
}

// create_function: CREATE/DROP FUNCTION are accepted (bodies not executed).
#[test]
fn create_function() {
    let conn = connect(&start_server());
    conn.exec(
        r#"CREATE FUNCTION addy(a int, b int) RETURNS int LANGUAGE sql AS $$ SELECT a + b $$"#,
        &[],
    )
    .unwrap();
    conn.exec(
        r#"CREATE OR REPLACE FUNCTION noop() RETURNS void AS $$ BEGIN END $$ LANGUAGE plpgsql"#,
        &[],
    )
    .unwrap();
    conn.exec(r#"DROP FUNCTION addy(int, int)"#, &[]).unwrap();
}

// range: range constructors, accessors, and containment.
#[test]
fn range() {
    let conn = connect(&start_server());

    let r: String = conn.query_row(r#"SELECT int4range(1, 10)"#, &[]).scan().unwrap();
    assert_eq!(r, "[1,10)");

    let (lo, hi): (String, String) = conn
        .query_row(r#"SELECT lower(int4range(1,10)), upper(int4range(1,10))"#, &[])
        .scan()
        .unwrap();
    assert_eq!(lo, "1");
    assert_eq!(hi, "10");

    // @> containment, a boolean as in Postgres.
    let inside: bool = conn.query_row(r#"SELECT int4range(1,10) @> 5"#, &[]).scan().unwrap();
    let outside: bool = conn.query_row(r#"SELECT int4range(1,10) @> 10"#, &[]).scan().unwrap();
    assert!(inside);
    assert!(!outside); // upper bound exclusive

    // isempty is a boolean, as Postgres types it.
    let empty: bool = conn.query_row(r#"SELECT isempty(int4range(5,5))"#, &[]).scan().unwrap();
    assert!(empty);

    // A range column round-trips.
    conn.exec(r#"CREATE TABLE t (r int4range)"#, &[]).unwrap();
    conn.exec(r#"INSERT INTO t VALUES ('[3,7)')"#, &[]).unwrap();
    let r: String = conn.query_row(r#"SELECT r FROM t"#, &[]).scan().unwrap();
    assert_eq!(r, "[3,7)");
}
