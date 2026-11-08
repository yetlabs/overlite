use std::collections::HashMap;

use crate::common::*;

#[test]
fn information_schema_constraints() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE dept (id int PRIMARY KEY, name text UNIQUE)"#);
    must_exec(
        &conn,
        r#"CREATE TABLE emp (
		id int PRIMARY KEY, dept_id int REFERENCES dept(id), email text NOT NULL)"#,
    );

    // table_constraints lists PK, FK and UNIQUE with Postgres' names: an
    // unnamed foreign key is <table>_<column>_fkey, as Postgres names it.
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT table_name || ':' || constraint_type || ':' || constraint_name
		FROM information_schema.table_constraints WHERE table_schema = 'public'
		ORDER BY table_name, constraint_type"#,
            0,
        ),
        [
            "dept:PRIMARY KEY:dept_pkey",
            "dept:UNIQUE:dept_name_key",
            "emp:FOREIGN KEY:emp_dept_id_fkey",
            "emp:PRIMARY KEY:emp_pkey",
        ]
    );

    // key_column_usage maps constraints to their columns.
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT constraint_name || ':' || column_name
			FROM information_schema.key_column_usage
			WHERE constraint_name IN ('dept_pkey','emp_pkey','emp_dept_id_fkey')
			ORDER BY constraint_name"#,
            0,
        ),
        ["dept_pkey:id", "emp_dept_id_fkey:dept_id", "emp_pkey:id"]
    );

    // referential_constraints links the FK to the referenced key.
    let (uniq, del): (String, String) = conn
        .query_row(
            r#"SELECT unique_constraint_name, delete_rule
		FROM information_schema.referential_constraints WHERE constraint_name = 'emp_dept_id_fkey'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(uniq, "dept_pkey");
    assert_eq!(del, "NO ACTION");
}

#[test]
fn information_schema_columns() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE t (
		id int PRIMARY KEY, name text NOT NULL, note text DEFAULT 'hi', doc jsonb, active boolean)"#,
    );

    #[derive(Default, Clone)]
    struct Col {
        typ: String,
        nullable: String,
        def: String,
    }
    let mut got: HashMap<String, Col> = HashMap::new();
    let rows = conn
        .query(
            r#"SELECT column_name, data_type, is_nullable,
		coalesce(column_default, '') FROM information_schema.columns WHERE table_name = 't'"#,
            &[],
        )
        .unwrap();
    for row in &rows {
        let (name, typ, nullable, def): (String, String, String, String) = row.scan().unwrap();
        got.insert(name, Col { typ, nullable, def });
    }
    // A missing column reads as its zero value.
    let got = |name: &str| got.get(name).cloned().unwrap_or_default();

    assert_eq!(got("id").typ, "integer");
    assert_eq!(got("id").nullable, "NO"); // primary key ⇒ not null
    assert_eq!(got("name").nullable, "NO");
    assert_eq!(got("note").nullable, "YES");
    assert_eq!(got("note").def, "'hi'");
    assert_eq!(got("doc").typ, "jsonb");
    assert_eq!(got("active").typ, "boolean");
}

#[test]
fn information_schema_misc() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE t (id int)"#);
    must_exec(&conn, r#"CREATE VIEW v AS SELECT id FROM t"#);
    must_exec(&conn, r#"CREATE SEQUENCE s START WITH 5"#);

    assert_subset(
        &query_column(&conn, r#"SELECT schema_name FROM information_schema.schemata"#, 0),
        &["public", "pg_catalog", "information_schema"],
    );
    assert_contains!(
        query_column(
            &conn,
            r#"SELECT table_name FROM information_schema.views WHERE table_schema = 'public'"#,
            0,
        ),
        "v"
    );
    assert_contains!(
        query_column(&conn, r#"SELECT sequence_name FROM information_schema.sequences"#, 0),
        "s"
    );
    // routines lists the provided functions; check_constraints is queryable (empty).
    assert_contains!(
        query_column(&conn, r#"SELECT routine_name FROM information_schema.routines"#, 0),
        "version"
    );
    let empty = query_column(
        &conn,
        r#"SELECT constraint_name FROM information_schema.check_constraints"#,
        0,
    );
    assert!(empty.is_empty(), "{empty:?} should be empty");
}

// An ORM quotes every name: Gitea's doctor reads `"information_schema"."sequences"`,
// which was 42P01 while the bare name worked. pg_catalog likewise.
#[test]
fn quoted_catalog_schema_names() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE t (id int)"#);
    must_exec(&conn, r#"CREATE SEQUENCE s"#);
    for sql in [
        r#"SELECT "sequence_name" FROM "information_schema"."sequences" WHERE sequence_name LIKE 's%' AND sequence_catalog = current_database()"#,
        r#"SELECT sequence_name FROM information_schema."sequences" WHERE sequence_name = 's'"#,
        r#"SELECT sequence_name FROM "information_schema".sequences WHERE sequence_name = 's'"#,
    ] {
        assert_eq!(query_column(&conn, sql, 0), vec!["s".to_string()], "{sql}");
    }
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT "table_name" FROM "information_schema"."tables" WHERE "table_schema" = 'public' AND "table_name" = 't'"#,
            0
        ),
        vec!["t".to_string()]
    );
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT relname FROM "pg_catalog"."pg_class" WHERE relname = 't'"#,
            0
        ),
        vec!["t".to_string()]
    );
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT nspname FROM "pg_catalog".pg_namespace WHERE nspname = 'public'"#,
            0
        ),
        vec!["public".to_string()]
    );
    // A quoted name keeps its case: there is no "INFORMATION_SCHEMA" schema.
    assert!(
        conn.query(r#"SELECT 1 FROM "INFORMATION_SCHEMA"."tables""#, &[])
            .is_err()
    );
}

// Postgres makes `<table>_<column>_seq` for a serial column, and the catalogs list it:
// Gitea's doctor counts sequences, and tools find a table's by name. A serial primary
// key here is SQLite's AUTOINCREMENT, which none of them listed.
#[test]
fn serial_columns_have_sequences_in_the_catalogs() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE project (id bigserial PRIMARY KEY, repo_id bigint)");
    must_exec(&conn, "CREATE TABLE plain (id bigint PRIMARY KEY)");
    must_exec(&conn, "CREATE SEQUENCE own_seq");
    must_exec(&conn, "INSERT INTO project (repo_id) VALUES (1), (2)");
    let one = |sql: &str| query_column(&conn, sql, 0);
    assert_eq!(
        one("SELECT sequence_name FROM information_schema.sequences ORDER BY 1"),
        vec!["own_seq".to_string(), "project_id_seq".to_string()]
    );
    assert_eq!(
        one(
            "SELECT sequence_schema || '|' || data_type FROM information_schema.sequences WHERE sequence_name = 'project_id_seq'"
        ),
        vec!["public|bigint".to_string()]
    );
    assert_eq!(
        one("SELECT relname FROM pg_class WHERE relkind = 'S' ORDER BY 1"),
        vec!["own_seq".to_string(), "project_id_seq".to_string()]
    );
    assert_eq!(
        one("SELECT schemaname || '.' || sequencename FROM pg_sequences ORDER BY 1"),
        vec!["public.own_seq".to_string(), "public.project_id_seq".to_string()]
    );
    assert_eq!(
        one("SELECT c.relname FROM pg_sequence s JOIN pg_class c ON c.oid = s.seqrelid ORDER BY 1"),
        vec!["own_seq".to_string(), "project_id_seq".to_string()]
    );
    // Read as a relation, as pg_dump reads a sequence's value.
    let fresh = connect(&server);
    assert_eq!(
        query_column(&fresh, "SELECT last_value::text FROM project_id_seq", 0),
        vec!["2".to_string()]
    );
}
