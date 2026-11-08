use crate::common::*;

// pg_tables, pg_indexes and pg_views are the readable summaries psql and most
// migration tools reach for before the pg_class joins. to_regclass is the cheap
// existence check. All four were missing.

#[test]
fn pg_tables_and_indexes() {
    let conn = connect(&start_server());

    must_exec(&conn, r#"CREATE TABLE orders (id int primary key, total int)"#);
    must_exec(&conn, r#"CREATE INDEX idx_orders_total ON orders (total)"#);
    must_exec(&conn, r#"CREATE SCHEMA sales"#);
    must_exec(&conn, r#"CREATE TABLE sales.invoices (id int primary key)"#);

    let (schema, owner, has_indexes): (String, String, bool) = conn
        .query_row(
            r#"SELECT schemaname, tableowner, hasindexes FROM pg_tables WHERE tablename = 'orders'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(schema, "public");
    assert_ne!(owner, "");
    assert!(has_indexes);

    // A table in another schema is reported under that schema.
    let schema: String = conn
        .query_row(r#"SELECT schemaname FROM pg_tables WHERE tablename = 'invoices'"#, &[])
        .scan()
        .unwrap();
    assert_eq!(schema, "sales");

    // Internal tables stay hidden.
    let n: i64 = conn
        .query_row(
            r#"SELECT count(*) FROM pg_tables WHERE tablename LIKE '_overlite%'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 0);

    let (schema, indexdef): (String, String) = conn
        .query_row(
            r#"SELECT schemaname, indexdef FROM pg_indexes WHERE indexname = 'idx_orders_total'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(schema, "public");
    assert_contains!(indexdef, "idx_orders_total");
}

#[test]
fn pg_views() {
    let conn = connect(&start_server());

    must_exec(&conn, r#"CREATE TABLE t (id int primary key)"#);
    must_exec(&conn, r#"CREATE VIEW v AS SELECT id FROM t"#);

    let (schema, definition): (String, String) = conn
        .query_row(
            r#"SELECT schemaname, definition FROM pg_views WHERE viewname = 'v'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(schema, "public");
    assert_contains!(definition, "SELECT");
}

#[test]
fn to_regclass() {
    let conn = connect(&start_server());

    must_exec(&conn, r#"CREATE TABLE orders (id int primary key)"#);
    must_exec(&conn, r#"CREATE SCHEMA sales"#);
    must_exec(&conn, r#"CREATE TABLE sales.invoices (id int primary key)"#);

    // The oid itself is opaque; what clients do with it is compare it to NULL.
    let exists = |name: &str| {
        let oid: Cell = conn.query_row(r#"SELECT to_regclass($1)"#, &[&name]).scan().unwrap();
        !oid.is_null()
    };

    assert!(exists("orders"));
    assert!(!exists("nope"), "a missing relation should resolve to NULL");
    assert!(exists("sales.invoices"), "a qualified name should resolve");
    assert!(!exists("sales.nope"));
    // A relation in another schema is not found under the wrong one.
    assert!(!exists("public.invoices"));
}

// gorm's AutoMigrate reads this column when it inspects an existing table; it
// was missing, so the migration failed on the second run.
#[test]
fn information_schema_identity_columns() {
    let conn = connect(&start_server());

    must_exec(&conn, r#"CREATE TABLE t (id serial primary key, name text)"#);

    let rows = conn
        .query(
            r#"
		SELECT column_name, identity_increment, identity_generation, generation_expression
		FROM information_schema.columns WHERE table_name = 't'"#,
            &[],
        )
        .unwrap();

    assert_eq!(rows.len(), 2);
}

// to_regclass with a bound parameter, which is what the extended protocol
// sends. Resolving only literals would answer NULL for every such call — a
// wrong answer rather than an error, so nothing would notice.
#[test]
fn to_regclass_parameterized() {
    let conn = connect_extended(&start_server());

    must_exec(&conn, r#"CREATE TABLE orders (id int primary key)"#);
    must_exec(&conn, r#"CREATE SCHEMA sales"#);
    must_exec(&conn, r#"CREATE TABLE sales.invoices (id int primary key)"#);

    let exists = |name: &str| {
        let oid: Cell = conn.query_row(r#"SELECT to_regclass($1)"#, &[&name]).scan().unwrap();
        !oid.is_null()
    };

    assert!(exists("orders"));
    assert!(!exists("nope"));
    assert!(exists("sales.invoices"));
    assert!(!exists("sales.nope"));
    assert!(!exists("public.invoices"));
}

// information_schema names the database the client sees. Reporting SQLite's
// internal "main" made every catalog query filtered by catalog come back
// empty — which is how gorm's migrator asks what columns a table already has,
// so it concluded there were none and tried to add them all again.
#[test]
fn catalog_names_the_client_database() {
    let conn = connect(&start_server());

    must_exec(&conn, r#"CREATE TABLE t (id int primary key, label text)"#);

    let db_name: String = conn.query_row(r#"SELECT current_database()"#, &[]).scan().unwrap();

    let catalog: String = conn
        .query_row(
            r#"SELECT table_catalog FROM information_schema.tables WHERE table_name = 't'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(catalog, db_name, "information_schema.tables names another database");

    let catalog: String = conn
        .query_row(
            r#"SELECT table_catalog FROM information_schema.columns WHERE table_name = 't' LIMIT 1"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(catalog, db_name, "information_schema.columns names another database");

    // The filter a migrator writes finds the columns.
    let n: i64 = conn
        .query_row(
            r#"
		SELECT count(*) FROM information_schema.columns
		WHERE table_catalog = current_database()
		  AND table_schema = CURRENT_SCHEMA()
		  AND table_name = 't'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 2);
}
