use crate::common::*;

// current_schema() must follow the session's search_path. It used to answer a
// constant "public", which misreports the schema a client works in: gorm's
// Migrator filters information_schema by CURRENT_SCHEMA(), so it asked about
// public and concluded every table in the real schema was missing.

#[test]
fn current_schema_follows_search_path() {
    let conn = connect(&start_server());

    let schema: String = conn.query_row("SELECT current_schema()", &[]).scan().unwrap();
    assert_eq!(schema, "public", "the default path is public");

    must_exec(&conn, "CREATE SCHEMA app");
    must_exec(&conn, "SET search_path TO app");

    let schema: String = conn.query_row("SELECT current_schema()", &[]).scan().unwrap();
    assert_eq!(schema, "app");

    // The parenthesis-less spelling Postgres allows works too.
    let schema: String = conn.query_row("SELECT current_schema", &[]).scan().unwrap();
    assert_eq!(schema, "app");

    // A path naming a schema that does not exist falls back to public.
    must_exec(&conn, "SET search_path TO nope");
    let schema: String = conn.query_row("SELECT current_schema()", &[]).scan().unwrap();
    assert_eq!(schema, "public");

    must_exec(&conn, "RESET search_path");
    let schema: String = conn.query_row("SELECT current_schema()", &[]).scan().unwrap();
    assert_eq!(schema, "public");
}

// The catalog filter clients actually write: information_schema narrowed by
// CURRENT_SCHEMA(), which is how gorm's HasTable asks.
#[test]
fn information_schema_by_current_schema() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA app");
    must_exec(&conn, "CREATE TABLE app.widget (id int primary key)");
    must_exec(&conn, "SET search_path TO app");

    let n: i64 = conn
        .query_row(
            "
		SELECT count(*) FROM information_schema.tables
		WHERE table_schema = CURRENT_SCHEMA() AND table_name = 'widget'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1, "the table was not found in the session's own schema");

    // A table in another schema is not reported. (It has to be created
    // explicitly in public: with the path set, a bare CREATE goes to app.)
    must_exec(&conn, "CREATE TABLE public.public_only (id int primary key)");
    let n: i64 = conn
        .query_row(
            "
		SELECT count(*) FROM information_schema.tables
		WHERE table_schema = CURRENT_SCHEMA() AND table_name = 'public_only'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn current_schemas() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA app");
    must_exec(&conn, "SET search_path TO app");

    let schemas: String = conn.query_row("SELECT current_schemas(false)", &[]).scan().unwrap();
    assert_eq!(schemas, "{app,public}");

    let schemas: String = conn.query_row("SELECT current_schemas(true)", &[]).scan().unwrap();
    assert_eq!(schemas, "{pg_catalog,app,public}");
}

// A string that merely looks like a call is left alone.
#[test]
fn current_schema_in_string_literal() {
    let conn = connect(&start_server());

    let s: String = conn.query_row("SELECT 'current_schema()'", &[]).scan().unwrap();
    assert_eq!(s, "current_schema()");
}

// An explicit public qualifier names the public table even when search_path
// points elsewhere. The qualifier used to be stripped before resolution ran,
// which turned it into a bare name that the path then moved into its own
// schema — a CREATE aimed at public silently landed in another schema.
#[test]
fn explicit_public_qualifier_beats_search_path() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE SCHEMA app");
    must_exec(&conn, "SET search_path TO app");
    must_exec(&conn, "CREATE TABLE public.only_public (id int primary key)");
    must_exec(&conn, "INSERT INTO public.only_public VALUES (1)");

    let schema: String = conn
        .query_row(
            "SELECT table_schema FROM information_schema.tables WHERE table_name = 'only_public'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(schema, "public", "the table landed outside public");

    // Reading it back through the qualifier works too.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM public.only_public", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);

    // And the unqualified name still resolves through the path, not to public.
    must_exec(&conn, "CREATE TABLE only_public (id int primary key)");
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'only_public'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 2, "the path's schema should now have its own copy");
}
