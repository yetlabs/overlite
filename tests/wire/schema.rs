use crate::common::*;

use crate::psql_compat::PSQL_LIST_SCHEMAS;

// CREATE SCHEMA over the wire, then use and introspect a schema-qualified table
// (the schema is a separate attached SQLite file behind the scenes).
#[test]
fn schema_over_wire() {
    let conn = connect(&start_server());

    conn.exec("CREATE SCHEMA vendas", &[]).unwrap();

    conn.exec("CREATE TABLE vendas.pedidos (id INTEGER PRIMARY KEY, total REAL)", &[])
        .unwrap();
    conn.exec("INSERT INTO vendas.pedidos (total) VALUES (5.5)", &[])
        .unwrap();

    let total: f64 = conn.query_row("SELECT total FROM vendas.pedidos", &[]).scan().unwrap();
    assert_eq!(total, 5.5);

    // A public table stays independent.
    conn.exec("CREATE TABLE clientes (id INTEGER PRIMARY KEY)", &[])
        .unwrap();

    // \dn lists both schemas.
    let schemas = query_column(&conn, PSQL_LIST_SCHEMAS, 0);
    assert_contains!(schemas, "public");
    assert_contains!(schemas, "vendas");

    // The table is visible in its schema via pg_class/pg_namespace.
    let rel: String = conn
        .query_row(
            "
		SELECT c.relname FROM pg_catalog.pg_class c
		JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
		WHERE n.nspname = 'vendas' AND c.relkind = 'r'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(rel, "pedidos");

    // CREATE SCHEMA IF NOT EXISTS is idempotent; DROP SCHEMA CASCADE works.
    conn.exec("CREATE SCHEMA IF NOT EXISTS vendas", &[]).unwrap();
    conn.exec("DROP SCHEMA vendas CASCADE", &[]).unwrap();

    let schemas = query_column(&conn, PSQL_LIST_SCHEMAS, 0);
    assert_not_contains!(schemas, "vendas");
}

// Schema statements by the extended protocol.
#[test]
fn schema_ddl_extended() {
    let addr = start_server();
    let mut raw = raw_conn(&addr, "overlite", "");
    let schemas = || {
        query_column(
            &connect(&addr),
            "SELECT nspname FROM pg_namespace WHERE nspname LIKE 's_' ORDER BY 1",
            0,
        )
    };

    assert_eq!(extended_exec(&mut raw, "CREATE SCHEMA s1").unwrap(), "CREATE SCHEMA");
    assert_eq!(schemas(), ["s1"]);
    extended_exec(&mut raw, "CREATE SCHEMA IF NOT EXISTS s1").unwrap();
    let err = extended_exec(&mut raw, "CREATE SCHEMA s1").unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");

    extended_exec(&mut raw, "CREATE TABLE s1.t (id int)").unwrap();
    extended_exec(&mut raw, "INSERT INTO s1.t VALUES (1)").unwrap();
    assert_eq!(
        extended_exec(&mut raw, "ALTER SCHEMA s1 RENAME TO s2").unwrap(),
        "ALTER SCHEMA"
    );
    assert_eq!(schemas(), ["s2"]);
    let n: i64 = connect(&addr)
        .query_row("SELECT count(*) FROM s2.t", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "the rename took the table along");

    extended_exec(&mut raw, "DROP SCHEMA s2").expect_err("the schema is not empty");
    assert_eq!(
        extended_exec(&mut raw, "DROP SCHEMA s2 CASCADE").unwrap(),
        "DROP SCHEMA"
    );
    assert_eq!(schemas(), Vec::<String>::new());
    extended_exec(&mut raw, "DROP SCHEMA IF EXISTS s2").unwrap();
    extended_exec(&mut raw, "DROP SCHEMA s2").expect_err("the schema is gone");
}

// In a transaction, as an error is: it aborts, and what ran before is undone.
#[test]
fn schema_ddl_extended_in_a_transaction() {
    let addr = start_server();
    let mut raw = raw_conn(&addr, "overlite", "");
    extended_exec(&mut raw, "BEGIN").unwrap();
    extended_exec(&mut raw, "CREATE SCHEMA s1").unwrap();
    extended_exec(&mut raw, "CREATE SCHEMA s1").expect_err("already there");
    let err = extended_exec(&mut raw, "CREATE SCHEMA s2").unwrap_err();
    assert_sqlstate(&err, "25P02");
    extended_exec(&mut raw, "ROLLBACK").unwrap();
    let n: i64 = connect(&addr)
        .query_row("SELECT count(*) FROM pg_namespace WHERE nspname = 's1'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
}

// A schema is everyone's the moment it exists: a session already open sees it in the
// catalog, and can use it, without reconnecting.
#[test]
fn open_session_sees_a_new_schema() {
    let server = start_server();
    let open = connect(&server);
    let other = connect(&server);
    let listed = |c: &Conn, name: &str| {
        let q = format!("SELECT count(*) FROM pg_namespace WHERE nspname = '{name}'");
        c.query_row(&q, &[]).scan::<i64>().unwrap()
    };
    assert_eq!(listed(&open, "fresh"), 0);

    must_exec(&other, "CREATE SCHEMA fresh");
    assert_eq!(listed(&open, "fresh"), 1);
    must_exec(&other, "CREATE TABLE fresh.t (a int)");
    must_exec(&open, "INSERT INTO fresh.t VALUES (1)");
    let n: i64 = open
        .query_row(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = 'fresh' AND c.relname = 't'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1);

    must_exec(&other, "ALTER SCHEMA fresh RENAME TO renamed");
    assert_eq!(listed(&open, "fresh"), 0);
    assert_eq!(listed(&open, "renamed"), 1);

    must_exec(&other, "DROP SCHEMA renamed CASCADE");
    assert_eq!(listed(&open, "renamed"), 0);
}

// A schema in a transaction is seen when the transaction commits, and never if it
// rolls back.
#[test]
fn schema_in_a_transaction_is_seen_at_commit() {
    let server = start_server();
    let open = connect(&server);
    let other = connect(&server);
    let listed = |c: &Conn, name: &str| {
        let q = format!("SELECT count(*) FROM pg_namespace WHERE nspname = '{name}'");
        c.query_row(&q, &[]).scan::<i64>().unwrap()
    };
    must_exec(&other, "BEGIN");
    must_exec(&other, "CREATE SCHEMA pending");
    assert_eq!(listed(&open, "pending"), 0);
    assert_eq!(listed(&other, "pending"), 1);
    must_exec(&other, "COMMIT");
    assert_eq!(listed(&open, "pending"), 1);

    must_exec(&other, "BEGIN");
    must_exec(&other, "CREATE SCHEMA undone");
    must_exec(&other, "ROLLBACK");
    assert_eq!(listed(&other, "undone"), 0);
    assert_eq!(listed(&open, "undone"), 0);
}

// Schema DDL takes part in a transaction like any statement: it fails it, is refused in
// one that failed, and each failure has Postgres' code, on both routes.
#[test]
fn schema_ddl_in_transactions() {
    let server = start_server();
    for c in [&connect(&server), &connect_extended(&server)] {
        must_exec(c, "BEGIN");
        must_exec(c, "CREATE SCHEMA s1");
        assert_sqlstate(&c.exec("CREATE SCHEMA s1", &[]).unwrap_err(), "42P06");
        assert_sqlstate(&c.exec("SELECT 1", &[]).unwrap_err(), "25P02");
        must_exec(c, "ROLLBACK");

        must_exec(c, "BEGIN");
        assert_sqlstate(&c.exec("DROP SCHEMA nope", &[]).unwrap_err(), "3F000");
        assert_sqlstate(&c.exec("CREATE SCHEMA s3", &[]).unwrap_err(), "25P02");
        assert_sqlstate(&c.exec("DROP SCHEMA nope2", &[]).unwrap_err(), "25P02");
        assert_sqlstate(&c.exec("ALTER SCHEMA nope RENAME TO x", &[]).unwrap_err(), "25P02");
        must_exec(c, "ROLLBACK");
        let n: i64 = c
            .query_row("SELECT count(*) FROM pg_namespace WHERE nspname IN ('s1', 's3')", &[])
            .scan()
            .unwrap();
        assert_eq!(n, 0);
    }
}

#[test]
fn schema_ddl_codes() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE SCHEMA s2; CREATE TABLE s2.t (a int)");
    for c in [&conn, &connect_extended(&server)] {
        for (sql, code) in [
            ("DROP SCHEMA s2", "2BP01"),
            ("DROP SCHEMA s2 RESTRICT", "2BP01"),
            ("CREATE SCHEMA s2", "42P06"),
            ("DROP SCHEMA nope", "3F000"),
            ("ALTER SCHEMA nope RENAME TO y", "3F000"),
            ("ALTER SCHEMA nope OWNER TO postgres", "3F000"),
            ("CREATE SCHEMA pg_x", "42939"),
            ("CREATE SCHEMA", "42601"),
            ("DROP SCHEMA", "42601"),
            ("CREATE SCHEMA public", "42P06"),
        ] {
            let got = c.exec(sql, &[]);
            assert_eq!(
                got.as_ref().map_err(WireError::code).err(),
                Some(code),
                "{sql}: {got:?}"
            );
        }
    }
    must_exec(&conn, "CREATE SCHEMA IF NOT EXISTS s2");
    must_exec(&conn, "DROP SCHEMA IF EXISTS nope");
    must_exec(&conn, "CREATE SCHEMA a; CREATE SCHEMA b");
    must_exec(&conn, "DROP SCHEMA a, b");
    must_exec(&conn, "CREATE SCHEMA AUTHORIZATION owner_name");
    must_exec(&conn, "DROP SCHEMA owner_name");
}
