use crate::common::*;

// Postgres-isms that used to fail to parse. Each is a syntax error away from
// working, which is a hard stop for a client that emits it.

#[test]
fn ilike() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE TABLE t (id int primary key, name text)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 'Printer Jam'), (2, 'vpn down')");

    let n: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE name ILIKE '%printer%'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "ILIKE should match regardless of case");

    let n: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE name ILIKE '%VPN%'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);

    let n: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE name NOT ILIKE '%printer%'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);

    // A literal containing the word is not an operator.
    let s: String = conn.query_row("SELECT 'ilike'", &[]).scan().unwrap();
    assert_eq!(s, "ilike");
}

#[test]
fn select_for_update() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE TABLE t (id int primary key, n int)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 10)");

    for sql in [
        "SELECT n FROM t WHERE id = 1 FOR UPDATE",
        "SELECT n FROM t WHERE id = 1 FOR NO KEY UPDATE",
        "SELECT n FROM t WHERE id = 1 FOR SHARE",
        "SELECT n FROM t WHERE id = 1 FOR KEY SHARE",
        "SELECT n FROM t WHERE id = 1 FOR UPDATE NOWAIT",
        "SELECT n FROM t WHERE id = 1 FOR UPDATE SKIP LOCKED",
        "SELECT n FROM t WHERE id = 1 FOR UPDATE OF t",
    ] {
        let n: i64 = conn.query_row(sql, &[]).scan().expect(sql);
        assert_eq!(n, 10);
    }

    // A counter read under FOR UPDATE, the usual reason to write one.
    let tx = conn.begin().unwrap();
    let n: i64 = tx
        .query_row("SELECT n FROM t WHERE id = 1 FOR UPDATE", &[])
        .scan()
        .unwrap();
    tx.exec("UPDATE t SET n = $1 WHERE id = 1", &[&(n + 1)]).unwrap();
    tx.commit().unwrap();

    let n: i64 = conn.query_row("SELECT n FROM t WHERE id = 1", &[]).scan().unwrap();
    assert_eq!(n, 11);
}

#[test]
fn computed_column_defaults() {
    let conn = connect(&start_server());

    must_exec(
        &conn,
        "CREATE TABLE t (
		id uuid DEFAULT gen_random_uuid(),
		created_at timestamptz DEFAULT now(),
		label text DEFAULT 'none',
		n int DEFAULT 0)",
    );
    must_exec(&conn, "INSERT INTO t (label) VALUES ('x')");

    let (id, created_at, label, n): (String, String, String, i64) = conn
        .query_row("SELECT id, created_at, label, n FROM t", &[])
        .scan()
        .unwrap();
    assert_eq!(id.len(), 36, "the uuid default did not run");
    assert!(!created_at.is_empty(), "the now() default did not run");
    assert_eq!(label, "x");
    assert_eq!(n, 0);

    // The same through ALTER TABLE.
    must_exec(&conn, "ALTER TABLE t ADD COLUMN touched_at timestamptz DEFAULT now()");
    must_exec(&conn, "INSERT INTO t (label) VALUES ('y')");
    let touched: String = conn
        .query_row("SELECT touched_at FROM t WHERE label = 'y'", &[])
        .scan()
        .unwrap();
    assert_ne!(touched, "");
}

// A computed default through ADD COLUMN: SQLite refuses it outright, so the
// table is rebuilt with the column in its CREATE TABLE. Existing rows keep
// their data and take the new column's default.
#[test]
fn add_column_computed_default_keeps_data() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE TABLE t (id int primary key, label text)");
    must_exec(&conn, "CREATE INDEX idx_label ON t (label)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 'first'), (2, 'second')");

    must_exec(&conn, "ALTER TABLE t ADD COLUMN created_at timestamptz DEFAULT now()");

    // The existing rows are still there, with their data.
    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 2);

    let label: String = conn.query_row("SELECT label FROM t WHERE id = 2", &[]).scan().unwrap();
    assert_eq!(label, "second");

    // New rows get the default.
    must_exec(&conn, "INSERT INTO t (id, label) VALUES (3, 'third')");
    let created_at: String = conn
        .query_row("SELECT created_at FROM t WHERE id = 3", &[])
        .scan()
        .unwrap();
    assert_ne!(created_at, "");

    // The index survived the rebuild.
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM pg_indexes WHERE tablename = 't' AND indexname = 'idx_label'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1, "the index was lost in the rebuild");

    // A constant default still takes the plain path.
    must_exec(&conn, "ALTER TABLE t ADD COLUMN n int DEFAULT 0");
    let n: i64 = conn.query_row("SELECT n FROM t WHERE id = 1", &[]).scan().unwrap();
    assert_eq!(n, 0);
}

#[test]
fn add_column_computed_default_extended_protocol() {
    let conn = connect_extended(&start_server());

    must_exec(&conn, "CREATE TABLE t (id int primary key)");
    must_exec(&conn, "INSERT INTO t VALUES (1)");
    must_exec(&conn, "ALTER TABLE t ADD COLUMN created_at timestamptz DEFAULT now()");

    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

// The same through a schema-qualified table, which is what an ORM emits once
// the application uses schemas at all.
#[test]
fn add_column_computed_default_on_qualified_table() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA sales");
    must_exec(
        &conn,
        r#"CREATE TABLE "sales"."orders" (id int primary key, label text)"#,
    );
    must_exec(&conn, r#"INSERT INTO "sales"."orders" VALUES (1, 'first')"#);

    must_exec(
        &conn,
        r#"ALTER TABLE "sales"."orders" ADD COLUMN created_at timestamptz DEFAULT now()"#,
    );

    let label: String = conn
        .query_row(r#"SELECT label FROM "sales"."orders" WHERE id = 1"#, &[])
        .scan()
        .unwrap();
    assert_eq!(label, "first", "the rebuild lost the existing row");

    must_exec(
        &conn,
        r#"INSERT INTO "sales"."orders" (id, label) VALUES (2, 'second')"#,
    );
    let created_at: String = conn
        .query_row(r#"SELECT created_at FROM "sales"."orders" WHERE id = 2"#, &[])
        .scan()
        .unwrap();
    assert_ne!(created_at, "");

    // The table stayed in its schema.
    let schema: String = conn
        .query_row(
            "SELECT table_schema FROM information_schema.tables WHERE table_name = 'orders'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(schema, "sales");
}

// Rebuilding a table that another one references: SQLite resolves those
// foreign keys while the table is briefly gone, so the rebuild has to defer
// enforcement to the end of its transaction.
#[test]
fn add_column_computed_default_on_referenced_table() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE TABLE customers (id int primary key, name text)");
    must_exec(
        &conn,
        "CREATE TABLE orders (id int primary key, customer_id int REFERENCES customers(id))",
    );
    must_exec(&conn, "INSERT INTO customers VALUES (1, 'ACME')");
    must_exec(&conn, "INSERT INTO orders VALUES (10, 1)");

    must_exec(
        &conn,
        "ALTER TABLE customers ADD COLUMN created_at timestamptz DEFAULT now()",
    );

    let name: String = conn
        .query_row("SELECT name FROM customers WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert_eq!(name, "ACME");

    // The foreign key still holds afterwards.
    conn.exec("INSERT INTO orders VALUES (11, 999)", &[])
        .expect_err("the foreign key stopped being enforced after the rebuild");
}

// ALTER TABLE on a schema-qualified table, in the spelling an ORM emits.
#[test]
fn alter_table_on_qualified_table() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA sales");
    must_exec(
        &conn,
        r#"CREATE TABLE "sales"."orders" (id int primary key, total int)"#,
    );
    must_exec(&conn, r#"INSERT INTO "sales"."orders" VALUES (1, 10)"#);

    must_exec(&conn, r#"ALTER TABLE "sales"."orders" ALTER COLUMN total TYPE bigint"#);
    must_exec(&conn, r#"ALTER TABLE "sales"."orders" ADD COLUMN label text"#);

    let total: i64 = conn
        .query_row(r#"SELECT total FROM "sales"."orders" WHERE id = 1"#, &[])
        .scan()
        .unwrap();
    assert_eq!(total, 10);

    let schema: String = conn
        .query_row(
            "SELECT table_schema FROM information_schema.tables WHERE table_name = 'orders'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(schema, "sales");
}

// Text that reads like what a rewrite pass looks for comes back as the client wrote
// it, by the whole route. Every pass is held to this in rewrite/mod.rs; these are the
// shapes that used to change.
#[test]
fn literals_reach_the_client_whole() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, a text)");
    for text in [
        "obj_description(c.oid, 'pg_class')",
        "x = ANY (ARRAY['a', 'b'])",
        "pg_get_indexdef(i.oid, 0, true)",
        "array_to_string(array(SELECT a FROM t), ',')",
        "pg_options_to_table(x)",
        "pg_partition_tree(x)",
        "a WITH ORDINALITY",
        "see '2024-01-02 03:04:05+02' there",
        "it's",
        "two ''",
    ] {
        let lit = text.replace('\'', "''");
        must_exec(&conn, "DELETE FROM t");
        must_exec(
            &conn,
            &format!("INSERT INTO t (id, a) VALUES (now() IS NOT NULL, '{lit}')"),
        );
        let got: String = conn.query_row("SELECT a FROM t", &[]).scan().unwrap();
        assert_eq!(got, text);

        // A cast on the literal takes all of it.
        let got: String = conn
            .query_row(&format!("SELECT '{lit}'::text, 1::int"), &[])
            .scan::<(String, i64)>()
            .unwrap()
            .0;
        assert_eq!(got, text);
    }
}
