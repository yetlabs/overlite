use crate::common::*;

// This file pins the exact SQL psql (v16, via `psql -E`) sends for its
// backslash commands. Running them against overlite is what proves \dt and \d
// work, so we discover missing catalog objects / dialect gaps here in CI
// instead of by hand at a psql prompt.

// The query behind \dt.
const PSQL_LIST_TABLES: &str = r#"SELECT n.nspname as "Schema",
  c.relname as "Name",
  CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' WHEN 'i' THEN 'index' WHEN 'S' THEN 'sequence' WHEN 't' THEN 'TOAST table' WHEN 'f' THEN 'foreign table' WHEN 'p' THEN 'partitioned table' WHEN 'I' THEN 'partitioned index' END as "Type",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam
WHERE c.relkind IN ('r','p','t','')
      AND n.nspname <> 'pg_catalog'
      AND n.nspname !~ '^pg_toast'
      AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;"#;

// The first query \d runs to find the table's oid. %s is the relation name.
const PSQL_RESOLVE_RELATION: &str = r#"SELECT c.oid,
  n.nspname,
  c.relname
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relname OPERATOR(pg_catalog.~) '^(%s)$' COLLATE pg_catalog.default
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 2, 3;"#;

// The follow-up queries \d runs once it has the oid. %[1]d is the relation oid.
const PSQL_DESCRIBE_QUERIES: &[&str] = &[
    // relation details
    r#"SELECT c.relchecks, c.relkind, c.relhasindex, c.relhasrules, c.relhastriggers, c.relrowsecurity, c.relforcerowsecurity, false AS relhasoids, c.relispartition, '', c.reltablespace, CASE WHEN c.reloftype = 0 THEN '' ELSE c.reloftype::pg_catalog.regtype::pg_catalog.text END, c.relpersistence, c.relreplident, am.amname
FROM pg_catalog.pg_class c
 LEFT JOIN pg_catalog.pg_am am ON (c.relam = am.oid)
WHERE c.oid = '%[1]d';"#,
    // columns
    r#"SELECT a.attname,
  pg_catalog.format_type(a.atttypid, a.atttypmod),
  (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true)
   FROM pg_catalog.pg_attrdef d
   WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef),
  a.attnotnull,
  (SELECT c.collname FROM pg_catalog.pg_collation c, pg_catalog.pg_type t
   WHERE c.oid = a.attcollation AND t.oid = a.atttypid AND a.attcollation <> t.typcollation) AS attcollation,
  a.attidentity,
  a.attgenerated
FROM pg_catalog.pg_attribute a
WHERE a.attrelid = '%[1]d' AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum;"#,
    // indexes
    r#"SELECT c2.relname, i.indisprimary, i.indisunique, i.indisclustered, i.indisvalid,
  pg_catalog.pg_get_indexdef(i.indexrelid, 0, true),
  pg_catalog.pg_get_constraintdef(con.oid, true), contype, condeferrable, condeferred, c2.reltablespace
FROM pg_catalog.pg_class c, pg_catalog.pg_class c2, pg_catalog.pg_index i
  LEFT JOIN pg_catalog.pg_constraint con ON (conrelid = i.indrelid AND conindid = i.indexrelid AND contype IN ('p','u','x'))
WHERE c.oid = '%[1]d' AND c.oid = i.indrelid AND i.indexrelid = c2.oid
ORDER BY i.indisprimary DESC, c2.relname;"#,
    // check constraints
    r#"SELECT r.conname, pg_catalog.pg_get_constraintdef(r.oid, true)
FROM pg_catalog.pg_constraint r
WHERE r.conrelid = '%[1]d' AND r.contype = 'c'
ORDER BY 1;"#,
    // foreign-key constraints
    r#"SELECT conname, pg_catalog.pg_get_constraintdef(r.oid, true) as condef
FROM pg_catalog.pg_constraint r
WHERE r.conrelid = '%[1]d' AND r.contype = 'f'
ORDER BY conname;"#,
    // triggers
    r#"SELECT t.tgname, pg_catalog.pg_get_triggerdef(t.oid, true), t.tgenabled, t.tgisinternal
FROM pg_catalog.pg_trigger t
WHERE t.tgrelid = '%[1]d' AND NOT t.tgisinternal
ORDER BY 1;"#,
    // rules
    r#"SELECT r.rulename, trim(trailing ';' from pg_catalog.pg_get_ruledef(r.oid, true))
FROM pg_catalog.pg_rewrite r
WHERE r.ev_class = '%[1]d' AND r.rulename != '_RETURN'
ORDER BY 1;"#,
    // row-level security policies (exact psql 16 query; uses ARRAY(subquery)
    // and "= ANY(...)", which the dialect layer must rewrite)
    r#"SELECT pol.polname, pol.polpermissive,
  CASE WHEN pol.polroles = '{0}' THEN NULL ELSE pg_catalog.array_to_string(array(select rolname from pg_catalog.pg_roles where oid = any (pol.polroles) order by 1),',') END,
  pg_catalog.pg_get_expr(pol.polqual, pol.polrelid),
  pg_catalog.pg_get_expr(pol.polwithcheck, pol.polrelid),
  CASE pol.polcmd
    WHEN 'r' THEN 'SELECT'
    WHEN 'a' THEN 'INSERT'
    WHEN 'w' THEN 'UPDATE'
    WHEN 'd' THEN 'DELETE'
    END AS cmd
FROM pg_catalog.pg_policy pol
WHERE pol.polrelid = '%[1]d' ORDER BY 1;"#,
    // child/partition relations
    r#"SELECT c.oid::pg_catalog.regclass
FROM pg_catalog.pg_class c, pg_catalog.pg_inherits i
WHERE c.oid = i.inhparent AND i.inhrelid = '%[1]d'
ORDER BY inhseqno;"#,
    // publications
    r#"SELECT pubname
FROM pg_catalog.pg_publication p
JOIN pg_catalog.pg_publication_rel pr ON p.oid = pr.prpubid
WHERE pr.prrelid = '%[1]d'
ORDER BY 1;"#,
    // referenced-by foreign keys (exact psql 16 query; regclass on a literal
    // stays an oid for the IN filter, on a column resolves to a name)
    r#"SELECT conname, conrelid::pg_catalog.regclass AS ontable,
       pg_catalog.pg_get_constraintdef(oid, true) AS condef
  FROM pg_catalog.pg_constraint c
 WHERE confrelid IN (SELECT pg_catalog.pg_partition_ancestors('%[1]d')
                     UNION ALL VALUES ('%[1]d'::pg_catalog.regclass))
       AND contype = 'f' AND conparentid = 0
ORDER BY conname;"#,
    // extended statistics (exact psql 16 query)
    r#"SELECT oid, stxrelid::pg_catalog.regclass, stxnamespace::pg_catalog.regnamespace::pg_catalog.text AS nsp, stxname,
pg_catalog.pg_get_statisticsobjdef_columns(oid) AS columns,
  'd' = any(stxkind) AS ndist_enabled,
  'f' = any(stxkind) AS deps_enabled,
  'm' = any(stxkind) AS mcv_enabled,
stxstattarget
FROM pg_catalog.pg_statistic_ext
WHERE stxrelid = '%[1]d'
ORDER BY nsp, stxname;"#,
];

#[test]
fn psql_list_tables() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nome TEXT NOT NULL)",
    );

    let rows = conn.query(PSQL_LIST_TABLES, &[]).expect("psql \\dt query must execute");

    let found = rows
        .iter()
        .any(|row| row.cell(1).text_value().as_deref() == Some("clientes"));
    assert!(found, "clientes must show up in \\dt");
}

#[test]
fn psql_describe_table() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nome TEXT NOT NULL)",
    );

    // Resolve the oid, as \d does first.
    let (oid, _, _): (i64, String, String) = conn
        .query_row(&PSQL_RESOLVE_RELATION.replace("%s", "clientes"), &[])
        .scan()
        .expect("psql \\d relation-resolve query must execute and find the table");

    // Every follow-up query \d issues must execute without error.
    for (i, q) in PSQL_DESCRIBE_QUERIES.iter().enumerate() {
        run_psql_query(&conn, i, &q.replace("%[1]d", &oid.to_string()));
    }
}

// --- other list commands (\l \dn \df \dv \di) ---------------------------------

const PSQL_LIST_DATABASES: &str = r#"SELECT d.datname as "Name",
  pg_catalog.pg_get_userbyid(d.datdba) as "Owner",
  pg_catalog.pg_encoding_to_char(d.encoding) as "Encoding"
FROM pg_catalog.pg_database d
ORDER BY 1;"#;

pub const PSQL_LIST_SCHEMAS: &str = r#"SELECT n.nspname AS "Name",
  pg_catalog.pg_get_userbyid(n.nspowner) AS "Owner"
FROM pg_catalog.pg_namespace n
WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
ORDER BY 1;"#;

const PSQL_LIST_FUNCTIONS: &str = r#"SELECT n.nspname as "Schema",
  p.proname as "Name",
  pg_catalog.pg_get_function_result(p.oid) as "Result data type",
  pg_catalog.pg_get_function_arguments(p.oid) as "Argument data types"
FROM pg_catalog.pg_proc p
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
WHERE pg_catalog.pg_function_is_visible(p.oid)
      AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema'
ORDER BY 1, 2;"#;

const PSQL_LIST_VIEWS: &str = r#"SELECT n.nspname as "Schema", c.relname as "Name",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('v','')
      AND n.nspname <> 'pg_catalog' AND n.nspname !~ '^pg_toast' AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;"#;

const PSQL_LIST_INDEXES: &str = r#"SELECT n.nspname as "Schema", c.relname as "Name",
  pg_catalog.pg_get_userbyid(c.relowner) as "Owner", c2.relname as "Table"
FROM pg_catalog.pg_class c
     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     LEFT JOIN pg_catalog.pg_index i ON i.indexrelid = c.oid
     LEFT JOIN pg_catalog.pg_class c2 ON i.indrelid = c2.oid
WHERE c.relkind IN ('i','I','')
      AND n.nspname <> 'pg_catalog' AND n.nspname !~ '^pg_toast' AND n.nspname <> 'information_schema'
  AND pg_catalog.pg_table_is_visible(c.oid)
ORDER BY 1,2;"#;

#[test]
fn psql_list_databases() {
    let conn = connect(&start_server());
    assert_contains!(query_column(&conn, PSQL_LIST_DATABASES, 0), "test"); // db name from test.db
}

#[test]
fn psql_list_schemas() {
    let conn = connect(&start_server());
    assert_eq!(query_column(&conn, PSQL_LIST_SCHEMAS, 0), ["public"]);
}

#[test]
fn psql_list_functions() {
    // Must execute even though we expose no user functions (empty result).
    let conn = connect(&start_server());
    query_column(&conn, PSQL_LIST_FUNCTIONS, 1);
}

#[test]
fn psql_list_views() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE base (a INTEGER)");
    must_exec(&conn, "CREATE VIEW ativos AS SELECT a FROM base WHERE a > 0");
    assert_contains!(query_column(&conn, PSQL_LIST_VIEWS, 1), "ativos");
}

#[test]
fn psql_list_indexes() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nome TEXT)");
    must_exec(&conn, "CREATE INDEX idx_nome ON clientes(nome)");
    assert_contains!(query_column(&conn, PSQL_LIST_INDEXES, 1), "idx_nome");
}

#[test]
fn psql_describe_shows_index_and_foreign_key() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nome TEXT)");
    must_exec(
        &conn,
        "CREATE TABLE pedidos (id INTEGER PRIMARY KEY, cliente_id INTEGER REFERENCES clientes(id))",
    );
    must_exec(&conn, "CREATE INDEX idx_cli ON pedidos(cliente_id)");

    let (oid, _, _): (i64, String, String) = conn
        .query_row(&PSQL_RESOLVE_RELATION.replace("%s", "pedidos"), &[])
        .scan()
        .unwrap();
    let describe = |i: usize| PSQL_DESCRIBE_QUERIES[i].replace("%[1]d", &oid.to_string());

    // Index query (\d "Indexes:" section) must list our index.
    let indexes = query_column(&conn, &describe(2), 0);
    assert_contains!(indexes, "idx_cli");

    // Foreign-key query (\d "Foreign-key constraints:") must return a row.
    let rows = conn.query(&describe(4), &[]).unwrap();
    assert_eq!(rows.len(), 1, "pedidos has one foreign key");
}

#[test]
fn psql_referenced_by_renders_name() {
    // The "Referenced by" section: the referencing table must render by NAME
    // (regclass on a column) while the IN filter (regclass on a literal oid)
    // still matches — the regclass heuristic must satisfy both.
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE clientes (id INTEGER PRIMARY KEY, nome TEXT)");
    must_exec(
        &conn,
        "CREATE TABLE pedidos (id INTEGER PRIMARY KEY, cliente_id INTEGER REFERENCES clientes(id))",
    );

    let (oid, _, _): (i64, String, String) = conn
        .query_row(&PSQL_RESOLVE_RELATION.replace("%s", "clientes"), &[])
        .scan()
        .unwrap();

    let q = r#"
		SELECT conname, conrelid::pg_catalog.regclass AS ontable,
		       pg_catalog.pg_get_constraintdef(oid, true) AS condef
		FROM pg_catalog.pg_constraint c
		WHERE confrelid IN (SELECT pg_catalog.pg_partition_ancestors('%[1]d')
		                    UNION ALL VALUES ('%[1]d'::pg_catalog.regclass))
		  AND contype = 'f' AND conparentid = 0"#;
    let (_conname, ontable, condef): (String, String, String) = conn
        .query_row(&q.replace("%[1]d", &oid.to_string()), &[])
        .scan()
        .unwrap();

    assert_eq!(ontable, "pedidos"); // name, not the oid
    assert_contains!(condef, "FOREIGN KEY (cliente_id) REFERENCES clientes(id)");
}
