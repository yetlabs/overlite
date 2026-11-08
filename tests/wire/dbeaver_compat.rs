use crate::common::*;

// Representative of the introspection queries DBeaver's PostgreSQL connector
// (over pgjdbc) runs while connecting and expanding the object tree: roles,
// databases, schemas (with descriptions and privilege filtering), and types.
// They must execute so the connection and navigator tree build.

const DBEAVER_ROLES: &str = r#"SELECT r.oid, r.rolname, r.rolsuper, r.rolinherit, r.rolcreaterole,
  r.rolcreatedb, r.rolcanlogin, r.rolreplication, r.rolbypassrls, r.rolconnlimit
FROM pg_catalog.pg_roles r
ORDER BY r.rolname;"#;

const DBEAVER_DATABASES: &str = r#"SELECT db.oid, db.datname, db.datistemplate, db.datallowconn,
  pg_catalog.pg_get_userbyid(db.datdba) AS owner,
  pg_catalog.pg_encoding_to_char(db.encoding) AS encoding,
  pg_catalog.has_database_privilege(db.oid, 'CONNECT') AS accessible
FROM pg_catalog.pg_database db
WHERE db.datallowconn
ORDER BY db.datname;"#;

const DBEAVER_SCHEMAS: &str = r#"SELECT n.oid, n.nspname, n.nspowner,
  pg_catalog.pg_get_userbyid(n.nspowner) AS owner,
  d.description
FROM pg_catalog.pg_namespace n
  LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid = n.oid AND d.objsubid = 0
WHERE pg_catalog.has_schema_privilege(n.oid, 'USAGE')
  AND n.nspname NOT LIKE 'pg_temp_%'
ORDER BY n.nspname;"#;

const DBEAVER_TYPES: &str = r#"SELECT t.oid, t.typname, t.typtype, t.typcategory, t.typlen,
  t.typbasetype, t.typelem, t.typrelid, t.typnamespace
FROM pg_catalog.pg_type t
ORDER BY t.oid;"#;

const DBEAVER_TABLES: &str = r#"SELECT c.oid, c.relname, c.relkind, c.relnamespace, c.relpersistence,
  c.reltablespace, pg_catalog.pg_get_userbyid(c.relowner) AS owner,
  d.description
FROM pg_catalog.pg_class c
  LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = 0
WHERE c.relnamespace = 2200 AND c.relkind IN ('r','v','m','f','p')
  AND pg_catalog.has_table_privilege(c.oid, 'SELECT')
ORDER BY c.relname;"#;

#[test]
fn dbeaver_connect_queries() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE clientes (id INTEGER PRIMARY KEY, nome TEXT NOT NULL)"#,
    );

    // Each connection-time query must execute without error.
    for (name, query) in [
        ("roles", DBEAVER_ROLES),
        ("databases", DBEAVER_DATABASES),
        ("schemas", DBEAVER_SCHEMAS),
        ("types", DBEAVER_TYPES),
        ("tables", DBEAVER_TABLES),
    ] {
        println!("{name}");
        conn.query(query, &[])
            .unwrap_or_else(|e| panic!("DBeaver {name} query must execute: {e}"));
    }
}

#[test]
fn dbeaver_schema_qualified_data() {
    // DBeaver reads table data with the schema-qualified name public.<table>;
    // SQLite has no "public" schema, so the qualifier must be dropped.
    let conn = connect(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE clientes (id INTEGER PRIMARY KEY, nome TEXT NOT NULL)"#,
    );
    must_exec(&conn, r#"INSERT INTO clientes (nome) VALUES ('ana')"#);

    let (id, nome): (i64, String) = conn
        .query_row(r#"SELECT id, nome FROM public.clientes ORDER BY id LIMIT 1"#, &[])
        .scan()
        .unwrap();
    assert_eq!(id, 1);
    assert_eq!(nome, "ana");

    // Quoted form too.
    let nome: String = conn
        .query_row(r#"SELECT nome FROM "public"."clientes" WHERE id = 1"#, &[])
        .scan()
        .unwrap();
    assert_eq!(nome, "ana");
}

#[test]
fn dbeaver_roles_and_schemas_content() {
    let conn = connect(&start_server());

    let roles = query_column(&conn, DBEAVER_ROLES, 1);
    assert_contains!(roles, "postgres"); // default role follows POSTGRES_USER

    let schemas = query_column(&conn, DBEAVER_SCHEMAS, 1);
    assert_contains!(schemas, "public");

    let dbs = query_column(&conn, DBEAVER_DATABASES, 1);
    assert_contains!(dbs, "test"); // database name derives from the file (test.db)
}
