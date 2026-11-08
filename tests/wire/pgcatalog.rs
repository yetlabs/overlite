use std::collections::HashMap;

use crate::common::*;

// The queries here mirror the shapes psql (\dt, \d) and GUI clients send:
// joins across pg_class / pg_namespace / pg_attribute / pg_type plus the
// helper functions format_type() and pg_table_is_visible().

#[test]
fn pg_catalog_list_tables() {
    let conn = connect(&start_server());

    must_exec(&conn, r#"CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"#);
    must_exec(&conn, r#"CREATE TABLE orders (id INTEGER PRIMARY KEY)"#);

    // The \dt shape: class joined to namespace, filtered to ordinary tables in
    // the public schema and visible in the search path.
    let rows = conn
        .query(
            r#"
		SELECT n.nspname, c.relname, c.relkind
		FROM pg_catalog.pg_class c
		JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
		WHERE c.relkind = 'r'
		  AND n.nspname = 'public'
		  AND pg_catalog.pg_table_is_visible(c.oid)
		ORDER BY c.relname"#,
            &[],
        )
        .unwrap();

    let got = rows.collect_rows::<(String, String, String)>().unwrap();
    assert_eq!(
        got,
        [
            ("public".to_string(), "orders".to_string(), "r".to_string()),
            ("public".to_string(), "users".to_string(), "r".to_string()),
        ]
    );
}

#[test]
fn pg_catalog_describe_columns() {
    let conn = connect(&start_server());

    must_exec(
        &conn,
        r#"CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, bio TEXT)"#,
    );

    // The \d <table> shape: attributes joined to class, with human type names
    // via format_type().
    let rows = conn
        .query(
            r#"
		SELECT a.attname,
		       format_type(a.atttypid, a.atttypmod) AS type,
		       a.attnotnull
		FROM pg_catalog.pg_attribute a
		JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
		JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
		WHERE c.relname = 'users'
		  AND n.nspname = 'public'
		  AND a.attnum > 0
		  AND NOT a.attisdropped
		ORDER BY a.attnum"#,
            &[],
        )
        .unwrap();

    let got = rows.collect_rows::<(String, String, bool)>().unwrap();
    assert_eq!(
        got,
        [
            ("id".to_string(), "integer".to_string(), true), // primary key surfaces as NOT NULL, as in Postgres
            ("name".to_string(), "text".to_string(), true),
            ("bio".to_string(), "text".to_string(), false),
        ]
    );
}

#[test]
fn pg_catalog_bare_names() {
    // Clients often reference catalog tables unqualified, relying on
    // pg_catalog being on the search_path.
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE t (a INTEGER)"#);

    let name: String = conn
        .query_row(
            r#"SELECT relname FROM pg_class WHERE relkind = 'r' AND relname NOT LIKE 'pg_%'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(name, "t");
}

#[test]
fn pg_catalog_type_join() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE t (a INTEGER, b TEXT, c REAL)"#);

    let rows = conn
        .query(
            r#"
		SELECT a.attname, t.typname
		FROM pg_catalog.pg_attribute a
		JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
		JOIN pg_catalog.pg_type t ON t.oid = a.atttypid
		WHERE c.relname = 't' AND a.attnum > 0
		ORDER BY a.attnum"#,
            &[],
        )
        .unwrap();

    let mut got: HashMap<String, String> = HashMap::new();
    for row in &rows {
        let (name, typ): (String, String) = row.scan().unwrap();
        got.insert(name, typ);
    }
    assert_eq!(
        got,
        HashMap::from([
            ("a".to_string(), "int4".to_string()),
            ("b".to_string(), "text".to_string()),
            ("c".to_string(), "float8".to_string()),
        ])
    );
}

#[test]
fn pg_catalog_partition_functions() {
    // psql's \d calls partition helpers; they must at least resolve.
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE t (a INTEGER)"#);
    for func in [
        "pg_catalog.pg_partition_root(c.oid)",
        "pg_catalog.pg_partition_ancestors(c.oid)",
        "pg_catalog.pg_get_partkeydef(c.oid)",
    ] {
        conn.exec(&format!("SELECT {func} FROM pg_catalog.pg_class c LIMIT 1"), &[])
            .unwrap_or_else(|e| panic!("function {func} must resolve: {e}"));
    }
}

#[test]
fn pg_catalog_constraint_and_index_columns() {
    // The column info that drives \d's Indexes / Foreign-key sections.
    let conn = connect(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE clientes (id INTEGER PRIMARY KEY, nome TEXT NOT NULL)"#,
    );
    must_exec(
        &conn,
        r#"CREATE TABLE pedidos (id INTEGER PRIMARY KEY, cliente_id INTEGER REFERENCES clientes(id))"#,
    );
    must_exec(&conn, r#"CREATE INDEX idx_cli ON pedidos(cliente_id)"#);

    let pk_cols: String = conn
        .query_row(
            r#"
		SELECT ov_cols FROM pg_catalog.pg_constraint con
		JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
		WHERE c.relname = 'clientes' AND con.contype = 'p'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(pk_cols, "id");

    let idx_cols: String = conn
        .query_row(
            r#"
		SELECT i.ov_cols FROM pg_catalog.pg_index i
		JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
		WHERE c.relname = 'idx_cli'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(idx_cols, "cliente_id");

    let (fk_cols, fk_ref): (String, String) = conn
        .query_row(
            r#"
		SELECT ov_cols, ov_ref FROM pg_catalog.pg_constraint con
		JOIN pg_catalog.pg_class c ON c.oid = con.conrelid
		WHERE c.relname = 'pedidos' AND con.contype = 'f'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(fk_cols, "cliente_id");
    assert_eq!(fk_ref, "clientes(id)");
}

#[test]
fn pg_catalog_regexp_operator() {
    // psql's \dt filters schemas with "!~ '^pg_toast'"; the ~ / !~ operators
    // must map onto SQLite's REGEXP.
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE real_table (a INTEGER)"#);

    let rows = conn
        .query(
            r#"
		SELECT nspname FROM pg_catalog.pg_namespace
		WHERE nspname !~ '^pg_' AND nspname ~ 'pub'"#,
            &[],
        )
        .unwrap();
    let got = rows.collect_rows::<String>().unwrap();
    assert_eq!(got, ["public"]);
}
