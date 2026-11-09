use std::collections::HashMap;

use crate::common::*;

// TestNoOpUtilityStatements checks statements we accept but don't model run
// without error (so migrations/dumps proceed).
#[test]
fn no_op_utility_statements() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int)");
    for sql in [
        r#"CREATE EXTENSION IF NOT EXISTS "uuid-ossp""#,
        "CREATE EXTENSION IF NOT EXISTS pgcrypto",
        "DROP EXTENSION IF EXISTS pgcrypto",
        "COMMENT ON TABLE t IS 'a table'",
        "COMMENT ON COLUMN t.id IS 'the id'",
        "NOTIFY some_channel",
    ] {
        must_exec(&conn, sql);
    }
    // The connection still works afterwards.
    let n: i64 = conn.query_row("SELECT 1", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

#[test]
fn faithful_type_oids() {
    // Extended protocol so pgx requests binary results (exercises the binary
    // encoders for jsonb/uuid) and exposes the advertised OIDs.
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE typed (
		a boolean, b integer, c bigint, d text,
		e varchar(20), f char(4), g json, h jsonb, i uuid, j real)",
    );

    let rows = conn
        .query("SELECT a, b, c, d, e, f, g, h, i, j FROM typed", &[])
        .unwrap();
    let want: [u32; 10] = [16, 23, 20, 25, 1043, 1042, 114, 3802, 2950, 701];
    let fds = &rows.fields;
    assert_eq!(fds.len(), want.len());
    for (i, fd) in fds.iter().enumerate() {
        assert_eq!(fd.data_type_oid, want[i], "column {:?} OID", fd.name);
    }

    // Round-trip real data through the binary path.
    must_exec(
        &conn,
        r#"INSERT INTO typed VALUES
		(true, 1, 2, 'x', 'vc', 'ch', '{"a":1}', '{"b":2}', gen_random_uuid(), 3.5)"#,
    );
    #[allow(clippy::type_complexity)] // one row of the table, spelled out once
    let (a, b, _c, d, e, _f, _g, h, id, j): (bool, i64, i64, String, String, String, String, String, String, f64) =
        conn.query_row("SELECT a, b, c, d, e, f, g, h, i, j FROM typed", &[])
            .scan()
            .unwrap();
    assert!(a);
    assert_eq!(b, 1);
    assert_eq!(d, "x");
    assert_eq!(e, "vc");
    assert_json_eq(r#"{"b":2}"#, &h);
    assert_regexp(UUID_RE, &id);
    assert_eq!(j, 3.5);
}

#[test]
fn pg_trigger_populated() {
    let addr = start_server();
    let conn = connect(&addr);
    must_exec(&conn, "CREATE TABLE t (id int)");
    must_exec(&conn, "CREATE TABLE audit (msg text)");
    must_exec(
        &conn,
        "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN INSERT INTO audit VALUES ('x'); END",
    );

    // The trigger appears in pg_trigger, linked to its table.
    assert_eq!(
        query_column(
            &conn,
            "SELECT tgname FROM pg_catalog.pg_trigger tr
		 JOIN pg_catalog.pg_class c ON c.oid = tr.tgrelid
		 WHERE c.relname = 't' AND NOT tr.tgisinternal",
            0
        ),
        ["t_ai"]
    );

    // pg_get_triggerdef renders the definition (registry refreshes per
    // connection, so read it from a fresh connection).
    let fresh = connect(&addr);
    let def: String = fresh
        .query_row(
            "SELECT pg_get_triggerdef(oid) FROM pg_catalog.pg_trigger WHERE tgname = 't_ai'",
            &[],
        )
        .scan()
        .unwrap();
    assert_contains!(def, "CREATE TRIGGER t_ai");
}

#[test]
fn alter_table() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, name text)");
    must_exec(&conn, "ALTER TABLE t ADD COLUMN email text");
    must_exec(&conn, "ALTER TABLE t RENAME COLUMN name TO full_name");
    must_exec(&conn, "ALTER TABLE t DROP COLUMN email");
    must_exec(&conn, "ALTER TABLE t RENAME TO people");

    assert_eq!(
        query_column(
            &conn,
            "SELECT column_name FROM information_schema.columns
		 WHERE table_name = 'people' ORDER BY ordinal_position",
            0
        ),
        ["id", "full_name"]
    );
}

#[test]
fn isolation_level_statements() {
    let conn = connect(&start_server());
    // Accepted (no real effect): SQLite serializes writes anyway.
    must_exec(&conn, "SET default_transaction_isolation = 'serializable'");
    let tx = conn.begin().unwrap(); // pgx BEGIN
    tx.exec("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE", &[]).unwrap();
    tx.rollback().unwrap();
}

#[test]
fn boolean_type() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE flags (id int, active boolean)");
    must_exec(&conn, "INSERT INTO flags VALUES (1, true), (2, false)");

    // A declared boolean column scans as bool (advertised OID is bool, so the
    // stored 0/1 is sent as t/f).
    let active: bool = conn
        .query_row("SELECT active FROM flags WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert!(active);
    let active: bool = conn
        .query_row("SELECT active FROM flags WHERE id = 2", &[])
        .scan()
        .unwrap();
    assert!(!active);

    // Round-trip a bool parameter.
    must_exec(&conn, "CREATE TABLE p (b boolean)");
    conn.exec("INSERT INTO p (b) VALUES ($1)", &[&true]).unwrap();
    let active: bool = conn.query_row("SELECT b FROM p", &[]).scan().unwrap();
    assert!(active);
}

#[test]
fn json_containment() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE docs (id int, body jsonb)");
    must_exec(
        &conn,
        r#"INSERT INTO docs VALUES
		(1, '{"name":"ada","tags":["a","b"],"meta":{"x":1}}'),
		(2, '{"name":"bob","tags":["c"]}')"#,
    );

    // @> object containment.
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT id::text FROM docs WHERE body @> '{"name":"ada"}' ORDER BY id"#,
            0
        ),
        ["1"]
    );
    // @> nested + array element containment.
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT id::text FROM docs WHERE body @> '{"meta":{"x":1}}'"#,
            0
        ),
        ["1"]
    );
    assert_eq!(
        query_column(&conn, r#"SELECT id::text FROM docs WHERE body @> '{"tags":["b"]}'"#, 0),
        ["1"]
    );
    // No match.
    assert_eq!(
        query_column(&conn, r#"SELECT id::text FROM docs WHERE body @> '{"name":"zed"}'"#, 0),
        [] as [std::string::String; 0]
    );
    // <@ (contained by): the literal on the left is contained in the column.
    assert_eq!(
        query_column(&conn, r#"SELECT id::text FROM docs WHERE '{"name":"bob"}' <@ body"#, 0),
        ["2"]
    );
}

#[test]
fn interval_arithmetic() {
    let conn = connect(&start_server());

    let cases = [
        (
            "SELECT '2020-01-15 10:00:00' + interval '2 hours'",
            "2020-01-15 12:00:00",
        ),
        ("SELECT '2020-01-15' - interval '1 day'", "2020-01-14 00:00:00"),
        (
            "SELECT '2020-01-15' + interval '1 year 2 months'",
            "2021-03-15 00:00:00",
        ),
        ("SELECT '2020-01-01' + interval '2 weeks'", "2020-01-15 00:00:00"),
        (
            "SELECT '2020-01-15 10:00:00' - interval '30 minutes'",
            "2020-01-15 09:30:00",
        ),
    ];
    for (query, want) in cases {
        let got: String = conn
            .query_row(query, &[])
            .scan()
            .unwrap_or_else(|e| panic!("query {query:?}: {e}"));
        assert_eq!(got, want, "query {query:?}");
    }
}

#[test]
fn savepoints() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int)");

    let tx = conn.begin().unwrap();
    tx.exec("INSERT INTO t VALUES (1)", &[]).unwrap();
    tx.exec("SAVEPOINT sp1", &[]).unwrap();
    tx.exec("INSERT INTO t VALUES (2)", &[]).unwrap();
    // Roll back to the savepoint: row 2 is undone, row 1 stays.
    tx.exec("ROLLBACK TO SAVEPOINT sp1", &[]).unwrap();
    tx.exec("INSERT INTO t VALUES (3)", &[]).unwrap();
    tx.exec("RELEASE SAVEPOINT sp1", &[]).unwrap();
    tx.commit().unwrap();

    assert_eq!(query_column(&conn, "SELECT id::text FROM t ORDER BY id", 0), ["1", "3"]);
}

// TestSavepointRecoversAbortedTx checks ROLLBACK TO clears the aborted state so
// the transaction can continue (Postgres error-recovery semantics).
#[test]
fn savepoint_recovers_aborted_tx() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int PRIMARY KEY)");

    let tx = conn.begin().unwrap();
    tx.exec("SAVEPOINT sp", &[]).unwrap();
    // Cause an error, aborting the (sub)transaction.
    let _ = tx.exec("INSERT INTO t VALUES ('not a number' + 1)", &[]); // ignored: the failure is the point
    tx.exec("SELECT bogus_col", &[]).unwrap_err();
    // Recover by rolling back to the savepoint, then continue.
    tx.exec("ROLLBACK TO SAVEPOINT sp", &[]).unwrap();
    tx.exec("INSERT INTO t VALUES (42)", &[]).unwrap();
    tx.commit().unwrap();

    assert_eq!(query_column(&conn, "SELECT id::text FROM t", 0), ["42"]);
}

// TestOuterJoins covers RIGHT/FULL OUTER JOIN, which recent SQLite runs
// natively (pass-through).
#[test]
fn outer_joins() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE a (id int)");
    must_exec(&conn, "CREATE TABLE b (id int)");
    must_exec(&conn, "INSERT INTO a VALUES (1),(2),(3)");
    must_exec(&conn, "INSERT INTO b VALUES (2),(3),(4)");

    assert_eq!(
        query_column(
            &conn,
            "SELECT b.id::text FROM a RIGHT JOIN b ON a.id = b.id ORDER BY b.id",
            0
        ),
        ["2", "3", "4"]
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT COALESCE(a.id, b.id)::text FROM a FULL OUTER JOIN b ON a.id = b.id ORDER BY 1",
            0
        ),
        ["1", "2", "3", "4"]
    );
}

#[test]
fn generate_series() {
    let conn = connect(&start_server());

    assert_eq!(
        query_column(&conn, "SELECT generate_series::text FROM generate_series(1, 5)", 0),
        ["1", "2", "3", "4", "5"]
    );
    // Step, including descending, and empty ranges.
    assert_eq!(
        query_column(&conn, "SELECT generate_series::text FROM generate_series(0, 6, 2)", 0),
        ["0", "2", "4", "6"]
    );
    assert_eq!(
        query_column(&conn, "SELECT generate_series::text FROM generate_series(5, 1, -2)", 0),
        ["5", "3", "1"]
    );
    assert_eq!(
        query_column(&conn, "SELECT generate_series::text FROM generate_series(5, 1)", 0),
        [] as [std::string::String; 0]
    );
    // Usable with an alias and joined like a table.
    assert_eq!(
        query_column(&conn, "SELECT (g * 10)::text FROM generate_series(1, 3) AS g", 0),
        ["10", "20", "30"]
    );
}

#[test]
fn distinct_on() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE sales (region text, amount int, ts int)");
    must_exec(
        &conn,
        "INSERT INTO sales VALUES
		('west', 10, 1), ('west', 30, 2), ('east', 20, 1), ('east', 5, 3)",
    );

    // Latest row per region (highest ts): DISTINCT ON (region) ... ORDER BY ts DESC.
    let rows = conn
        .query(
            "SELECT DISTINCT ON (region) region, amount FROM sales ORDER BY region, ts DESC",
            &[],
        )
        .unwrap();
    let mut got: HashMap<String, i64> = HashMap::new();
    for row in &rows {
        let (r, a): (String, i64) = row.scan().unwrap();
        got.insert(r, a);
    }
    assert_eq!(got, HashMap::from([("west".to_string(), 30), ("east".to_string(), 5)]));
}

const UUID_RE: &str = r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";

#[test]
fn uuid_type_and_generators() {
    let conn = connect(&start_server());

    // gen_random_uuid()/uuid_generate_v4() produce distinct valid v4 UUIDs.
    let u1: String = conn.query_row("SELECT gen_random_uuid()", &[]).scan().unwrap();
    let u2: String = conn.query_row("SELECT uuid_generate_v4()", &[]).scan().unwrap();
    assert_regexp(UUID_RE, &u1);
    assert_regexp(UUID_RE, &u2);
    assert_ne!(u1, u2);

    // A uuid column stores the value as text and reports type uuid in the catalog.
    must_exec(&conn, "CREATE TABLE items (id uuid, name text)");
    must_exec(&conn, "INSERT INTO items (id, name) VALUES (gen_random_uuid(), 'a')");
    let got: String = conn.query_row("SELECT id FROM items", &[]).scan().unwrap();
    assert_regexp(UUID_RE, &got);

    assert_eq!(
        query_column(
            &conn,
            "SELECT format_type(atttypid, NULL) FROM pg_catalog.pg_attribute a
		 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
		 WHERE c.relname = 'items' AND a.attname = 'id'",
            0
        ),
        ["uuid"]
    );

    // A non-deterministic generator yields a fresh value per row.
    let rows = query_column(
        &conn,
        "SELECT gen_random_uuid() FROM (SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3)",
        0,
    );
    assert_eq!(rows.len(), 3);
    assert_ne!(rows[0], rows[1]);
    assert_ne!(rows[1], rows[2]);
}
