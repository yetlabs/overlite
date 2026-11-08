use crate::common::*;

// Most ORMs quote every identifier they emit, so a schema-qualified name
// reaches the server as `"sales"."orders"`. Only the bare spelling used to be
// recognised, which left those clients unable to use schemas at all: the
// quoted form fell through to SQLite, which read it as an attached database
// and answered `unknown database "sales"`.

#[test]
fn quoted_schema_qualifiers() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA sales");

    // Every quoting combination names the same table.
    for (i, create) in [
        r#"CREATE TABLE "sales"."t%d" (id int primary key, label text)"#,
        r#"CREATE TABLE sales."t%d" (id int primary key, label text)"#,
        r#"CREATE TABLE "sales".t%d (id int primary key, label text)"#,
        "CREATE TABLE sales.t%d (id int primary key, label text)",
    ]
    .iter()
    .enumerate()
    {
        conn.exec(&create.replace("%d", &i.to_string()), &[])
            .unwrap_or_else(|e| panic!("create form {i}: {e}"));
    }

    must_exec(&conn, r#"INSERT INTO "sales"."t0" VALUES (1, 'quoted')"#);

    let label: String = conn.query_row(r#"SELECT label FROM "sales"."t0""#, &[]).scan().unwrap();
    assert_eq!(label, "quoted");

    // The bare and quoted spellings reach the same table.
    let label: String = conn.query_row("SELECT label FROM sales.t0", &[]).scan().unwrap();
    assert_eq!(label, "quoted");

    must_exec(&conn, r#"UPDATE "sales"."t0" SET label = 'updated' WHERE id = 1"#);
    let label: String = conn.query_row("SELECT label FROM sales.t0", &[]).scan().unwrap();
    assert_eq!(label, "updated");

    must_exec(&conn, r#"DELETE FROM "sales"."t0" WHERE id = 1"#);
    let n: i64 = conn.query_row("SELECT count(*) FROM sales.t0", &[]).scan().unwrap();
    assert_eq!(n, 0);
}

// The catalog must see a table created through the quoted spelling as
// belonging to the schema, not to public.
#[test]
fn quoted_schema_qualifier_in_catalog() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA sales");
    must_exec(&conn, r#"CREATE TABLE "sales"."orders" (id int primary key)"#);

    let schema: String = conn
        .query_row(
            "SELECT table_schema FROM information_schema.tables WHERE table_name = 'orders'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(schema, "sales");
}

// A quoted name that is not a schema keeps its own meaning: an alias qualifier
// must not be mistaken for a schema qualifier.
#[test]
fn quoted_alias_is_not_a_schema() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA sales");
    must_exec(
        &conn,
        r#"CREATE TABLE "sales"."orders" (id int primary key, total int)"#,
    );
    must_exec(&conn, r#"INSERT INTO "sales"."orders" VALUES (1, 10)"#);

    // "o" is an alias here, not a schema.
    let total: i64 = conn
        .query_row(
            r#"SELECT "o"."total" FROM "sales"."orders" "o" WHERE "o"."id" = 1"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(total, 10);

    // A string literal that merely looks like a qualifier is left alone.
    let s: String = conn.query_row("SELECT 'sales.orders'", &[]).scan().unwrap();
    assert_eq!(s, "sales.orders");
}

// An index on a quoted qualified table belongs to that table's schema.
#[test]
fn index_on_quoted_schema_table() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA sales");
    must_exec(
        &conn,
        r#"CREATE TABLE "sales"."orders" (id int primary key, total int)"#,
    );
    must_exec(&conn, r#"CREATE INDEX "idx_total" ON "sales"."orders" ("total")"#);

    let n: i64 = conn
        .query_row(
            "
		SELECT count(*) FROM pg_index i
		JOIN pg_class c ON c.oid = i.indrelid
		JOIN pg_namespace ns ON ns.oid = c.relnamespace
		WHERE ns.nspname = 'sales' AND c.relname = 'orders'",
            &[],
        )
        .scan()
        .unwrap();
    assert!(n >= 1, "{n} is not >= 1");
}

// A foreign key written with quoted qualified names is enforced.
#[test]
fn quoted_schema_foreign_key() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA sales");
    must_exec(&conn, r#"CREATE TABLE "sales"."customers" (id int primary key)"#);
    must_exec(
        &conn,
        r#"CREATE TABLE "sales"."orders" (
		id int primary key,
		customer_id int REFERENCES "sales"."customers"("id"))"#,
    );
    must_exec(&conn, r#"INSERT INTO "sales"."customers" VALUES (1)"#);
    must_exec(&conn, r#"INSERT INTO "sales"."orders" VALUES (10, 1)"#);

    conn.exec(r#"INSERT INTO "sales"."orders" VALUES (11, 999)"#, &[])
        .expect_err("a dangling foreign key was accepted");
}
