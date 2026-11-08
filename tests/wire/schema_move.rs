use crate::common::*;

// alter_table_set_schema: moving a table between schemas is a rename in
// single-file mode; data and referencing FKs survive it.
#[test]
fn alter_table_set_schema() {
    let conn = connect(&start_server());

    conn.exec("CREATE SCHEMA archive", &[]).unwrap();
    conn.exec("CREATE TABLE public.doc (id int primary key, body text)", &[])
        .unwrap();
    conn.exec("INSERT INTO public.doc VALUES (1, 'hello')", &[]).unwrap();

    // Move public.doc -> archive.doc.
    conn.exec("ALTER TABLE public.doc SET SCHEMA archive", &[]).unwrap();

    let ns: String = conn
        .query_row(
            "
		SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
		WHERE c.relname = 'doc'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(ns, "archive");
    let body: String = conn
        .query_row("SELECT body FROM archive.doc WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert_eq!(body, "hello");

    // The old location is gone.
    conn.exec("SELECT * FROM public.doc", &[]).unwrap_err();
}

// alter_schema_rename: renaming a schema carries its tables (and their data).
#[test]
fn alter_schema_rename() {
    let conn = connect(&start_server());

    conn.exec("CREATE SCHEMA old_name", &[]).unwrap();
    conn.exec("CREATE TABLE old_name.thing (id int)", &[]).unwrap();
    conn.exec("INSERT INTO old_name.thing VALUES (42)", &[]).unwrap();

    conn.exec("ALTER SCHEMA old_name RENAME TO new_name", &[]).unwrap();

    let n: i64 = conn
        .query_row("SELECT count(*) FROM pg_namespace WHERE nspname = 'old_name'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
    let id: i64 = conn.query_row("SELECT id FROM new_name.thing", &[]).scan().unwrap();
    assert_eq!(id, 42);

    // Renaming to an existing schema fails.
    conn.exec("CREATE SCHEMA taken", &[]).unwrap();
    conn.exec("ALTER SCHEMA new_name RENAME TO taken", &[]).unwrap_err();
}

// schema_table_introspection: a table in a non-public schema introspects
// fully — its PK/FK constraint names carry no "schema." prefix, and an explicit
// index on it belongs to the schema and is visible.
#[test]
fn schema_table_introspection() {
    let conn = connect(&start_server());

    conn.exec("CREATE SCHEMA app", &[]).unwrap();
    conn.exec("CREATE TABLE app.parent (id int primary key)", &[]).unwrap();
    conn.exec(
        "CREATE TABLE app.child (id int primary key, pid int references app.parent(id))",
        &[],
    )
    .unwrap();
    conn.exec("CREATE INDEX idx_child_pid ON app.child(pid)", &[]).unwrap();

    // Constraint names have no schema prefix.
    let names = query_column(
        &conn,
        "
		SELECT con.conname FROM pg_constraint con
		JOIN pg_class c ON c.oid = con.conrelid
		JOIN pg_namespace n ON n.oid = c.relnamespace
		WHERE n.nspname = 'app' AND c.relname = 'child' ORDER BY con.conname",
        0,
    );
    for n in &names {
        assert_not_contains!(n, "app.", "constraint name should not carry the schema prefix: {n}");
    }

    // The explicit index belongs to schema app and is linked to app.child.
    let (idx_name, idx_schema): (String, String) = conn
        .query_row(
            "
		SELECT ci.relname, ni.nspname
		FROM pg_index i
		JOIN pg_class ct ON ct.oid = i.indrelid
		JOIN pg_class ci ON ci.oid = i.indexrelid
		JOIN pg_namespace ni ON ni.oid = ci.relnamespace
		WHERE ct.relname = 'child' AND ci.relname LIKE '%idx_child_pid%'",
            &[],
        )
        .scan()
        .unwrap();
    assert_contains!(idx_name, "idx_child_pid");
    assert_eq!(idx_schema, "app");
}
