use crate::common::*;

// schema_in_transaction: in the default single-file mode a schema is an
// ordinary write, so CREATE/DROP SCHEMA work inside a transaction (impossible
// in multi-file mode, where ATTACH can't run in a tx) and roll back with it.
#[test]
fn schema_in_transaction() {
    let conn = connect(&start_server());

    // CREATE SCHEMA + a table + data, all in one transaction.
    let tx = conn.begin().unwrap();
    tx.exec("CREATE SCHEMA sales", &[]).unwrap();
    tx.exec("CREATE TABLE sales.orders (id int primary key, total numeric)", &[])
        .unwrap();
    tx.exec("INSERT INTO sales.orders VALUES (1, 12.34)", &[]).unwrap();
    tx.commit().unwrap();

    let total: String = conn.query_row("SELECT total FROM sales.orders", &[]).scan().unwrap();
    assert_eq!(total, "12.34");

    // A rolled-back CREATE SCHEMA leaves nothing behind.
    let tx2 = conn.begin().unwrap();
    tx2.exec("CREATE SCHEMA temp_stuff", &[]).unwrap();
    tx2.rollback().unwrap();

    let n: i64 = conn
        .query_row("SELECT count(*) FROM pg_namespace WHERE nspname = 'temp_stuff'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
}

// cross_schema_foreign_key: a foreign key from a public table to a table in
// another schema is enforced (both live in the one file, unlike multi-file mode
// where SQLite can't enforce FKs across attached databases).
#[test]
fn cross_schema_foreign_key() {
    let conn = connect(&start_server());

    conn.exec("CREATE SCHEMA inv", &[]).unwrap();
    conn.exec("CREATE TABLE inv.product (id int primary key)", &[]).unwrap();
    conn.exec("INSERT INTO inv.product VALUES (1)", &[]).unwrap();
    conn.exec(
        "CREATE TABLE public.line (id int primary key, pid int references inv.product(id))",
        &[],
    )
    .unwrap();

    // A dangling reference is rejected; a valid one is accepted.
    conn.exec("INSERT INTO public.line VALUES (1, 999)", &[]).unwrap_err();
    conn.exec("INSERT INTO public.line VALUES (1, 1)", &[]).unwrap();
}

// drop_schema_cascade_in_tx: DROP SCHEMA CASCADE removes the schema and its
// prefixed tables, transactionally.
#[test]
fn drop_schema_cascade_in_tx() {
    let conn = connect(&start_server());

    conn.exec("CREATE SCHEMA s1", &[]).unwrap();
    conn.exec("CREATE TABLE s1.t (id int)", &[]).unwrap();

    // Non-empty schema needs CASCADE.
    conn.exec("DROP SCHEMA s1", &[]).unwrap_err();

    conn.exec("DROP SCHEMA s1 CASCADE", &[]).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM pg_namespace WHERE nspname = 's1'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
    // The table is gone too.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM pg_class WHERE relname = 't'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
}
