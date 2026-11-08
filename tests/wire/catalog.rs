use crate::common::*;

#[test]
fn catalog_version() {
    let conn = connect(&start_server());

    let v: String = conn.query_row("SELECT version()", &[]).scan().unwrap();
    assert_contains!(v, "overlite");
    assert_contains!(v, "PostgreSQL");
}

#[test]
fn catalog_current_schema() {
    let conn = connect(&start_server());

    let schema: String = conn.query_row("SELECT current_schema()", &[]).scan().unwrap();
    assert_eq!(schema, "public");
}

#[test]
fn catalog_information_schema_tables() {
    let conn = connect(&start_server());

    must_exec(&conn, r#"CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)"#);
    must_exec(&conn, r#"CREATE TABLE orders (id INTEGER PRIMARY KEY, total REAL)"#);

    let rows = conn
        .query(
            r#"SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name"#,
            &[],
        )
        .unwrap();

    let names = rows.collect_rows::<String>().unwrap();
    assert_eq!(names, ["orders", "users"]);
}

#[test]
fn catalog_information_schema_columns() {
    let conn = connect(&start_server());

    must_exec(
        &conn,
        r#"CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)"#,
    );

    let rows = conn
        .query(
            r#"SELECT column_name FROM information_schema.columns WHERE table_name = 'users'"#,
            &[],
        )
        .unwrap();

    let mut cols = rows.collect_rows::<String>().unwrap();
    cols.sort();
    assert_eq!(cols, ["age", "id", "name"]);
}
