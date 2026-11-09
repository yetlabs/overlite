//! The database overlite points at IS the "public" schema. A schema is a row in
//! `_overlite_schemas` plus tables carrying a `"<schema>."` name prefix. This file
//! owns discovering schemas and generating the catalog views that span them.

use crate::catalog::STATIC_CATALOG_VIEWS;
use crate::catalog::comments::COMMENTS_TABLE_DDL;
use crate::catalog::enums::{
    COMPOSITE_TYPES_TABLE_DDL, ENUM_LABELS_TABLE_DDL, ENUM_TYPES_TABLE_DDL, refresh_enum_names,
};
use crate::catalog::grants::{
    COLUMN_GRANTS_TABLE_DDL, GRANTS_TABLE_DDL, MEMBERSHIPS_ADD_ADMIN_DDL, MEMBERSHIPS_TABLE_DDL, OWNERS_TABLE_DDL,
    forget_table_rows_sql, move_table_rows_sql,
};
use crate::catalog::rls::{POLICIES_TABLE_DDL, RLS_STORED_NAMES_MARK, RLS_STORED_NAMES_UPGRADE, RLS_TABLE_DDL};
use crate::catalog::roles::{ROLES_ADD_PASSWORD_DDL, ROLES_TABLE_DDL, seed_default_role_sql};
use crate::catalog::sequences::{
    COLUMN_DEFAULTS_TABLE_DDL, DEFAULTS_STORED_NAMES_MARK, DEFAULTS_STORED_NAMES_UPGRADE, SEQUENCES_TABLE_DDL,
};
use crate::catalog::triggers::refresh_trigger_defs;
use crate::catalog::views::{dynamic_catalog_views, with_table_oid};
use crate::db_state::state_for;
use crate::error::{Result, errf};
use crate::sql_funcs::{FUNCTIONS_ADD_COLS_DDL, FUNCTIONS_TABLE_DDL, refresh_functions_from_rows};
use crate::sqlite::{DbSession, do_exec, do_query};
use overlite_core::SqlState;
use overlite_core::text::{eq_fold, quote_literal, quoted, slice, trim_chars};
use overlite_core::{ResultSet, Value, lazy_regex};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Presents one Postgres schema to the catalog. db is always "main" and prefix
/// is the table-name prefix ("vendas.") a schema's tables carry; public has an
/// empty prefix.
#[derive(Clone, Debug, Default)]
pub(crate) struct SchemaRef {
    pub(crate) pg_name: String, // "public", "vendas"
    pub(crate) db: String,      // SQLite attached-db name: "main", "vendas"
    pub(crate) ns_oid: i64,     // pg_namespace.oid
    pub(crate) offset: i64,     // added to every oid so ids don't collide across schemas
    pub(crate) prefix: String,  // single-file table-name prefix: "" (public) or "vendas."
}

impl SchemaRef {
    /// The SQL table source the per-schema templates read as @MASTER@. The public
    /// schema reads a view of main.sqlite_master that excludes tables belonging
    /// to a registered schema, so those don't leak into public.
    pub(crate) fn master(&self) -> String {
        if self.prefix.is_empty() {
            // public: bare-named tables, i.e. anything not owned by a registered
            // schema. mm.name must be qualified: an unqualified `name` inside the
            // EXISTS would bind to _overlite_schemas.
            return r#"(SELECT mm.rowid AS rowid, mm.type AS type, mm.name AS name,
		         mm.tbl_name AS tbl_name, mm.sql AS sql FROM main.sqlite_master mm
		  WHERE instr(mm.name,'.')=0
		     OR NOT EXISTS (SELECT 1 FROM _overlite_schemas s
		                    WHERE s.name = substr(mm.name,1,instr(mm.name,'.')-1)))"#
                .to_string();
        }
        // a schema: exactly the tables carrying its "<name>." prefix.
        [
            r#"(SELECT rowid, type, name, tbl_name, sql FROM main.sqlite_master
	  WHERE name LIKE '"#,
            &escape_like(&self.prefix),
            r#"%' ESCAPE '\')"#,
        ]
        .concat()
    }
}

/// The role name shown as owner / returned by current_user etc. It follows the
/// official Postgres image's POSTGRES_USER (default "postgres").
pub(crate) static CATALOG_ROLE: LazyLock<String> = LazyLock::new(|| env_or("POSTGRES_USER", "postgres"));

fn env_or(key: &str, def: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => def.to_string(),
    }
}

/// The last element of a Unix path.
pub(crate) fn filepath_base(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let p = path.trim_end_matches('/');
    if p.is_empty() {
        return "/".to_string();
    }
    match p.rfind('/') {
        Some(i) => p[i + 1..].to_string(),
        None => p.to_string(),
    }
}

/// Derives the Postgres database name from the SQLite file path:
/// "/data/hello.db" -> "hello".
pub(crate) fn db_name_from_path(path: &str) -> String {
    if path.is_empty() || path == ":memory:" {
        return "main".to_string();
    }
    let base = filepath_base(path);
    base.strip_suffix(".db").unwrap_or(&base).to_string()
}

/// Rebuilt per connection because they embed the (possibly runtime-derived) role
/// and database name.
fn meta_catalog_views(db_name: &str) -> Vec<String> {
    let role = quote_literal(&CATALOG_ROLE);
    vec![
        // The configured role gets oid 10 (Postgres' bootstrap superuser oid), so
        // object owners (relowner=10 in pg_class) resolve to it for pg_dump.
        [
            r#"CREATE TEMP VIEW pg_roles AS SELECT
		 CASE WHEN rolname = "#,
            &role,
            r#" COLLATE NOCASE THEN 10 ELSE rowid + 16384 END AS oid,
		 rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolreplication,
		 -1 AS rolconnlimit, '********' AS rolpassword, NULL AS rolvaliduntil,
		 rolbypassrls, NULL AS rolconfig, 1260 AS tableoid
		 FROM _overlite_roles"#,
        ]
        .concat(),
        [
            r#"CREATE TEMP VIEW pg_sequences AS SELECT 'public' AS schemaname, seqname AS sequencename,
		 "#,
            &role,
            r#" AS sequenceowner, 'bigint' AS data_type,
		 start_value, min_value, max_value, increment AS increment_by, is_cycled AS cycle,
		 cache_size, CASE WHEN is_called THEN last_value ELSE NULL END AS last_value
		 FROM _overlite_sequences
		 UNION ALL SELECT schemaname, seqname, "#,
            &role,
            r#", 'bigint', 1, 1, 9223372036854775807, 1, 0, 1, NULL
		 FROM _overlite_serial_sequences"#,
        ]
        .concat(),
        [
            r#"CREATE TEMP VIEW pg_database AS SELECT 1 AS oid, "#,
            &quote_literal(db_name),
            r#" AS datname,
		 10 AS datdba, 6 AS encoding, 'c' AS datlocprovider, 'C' AS datcollate, 'C' AS datctype,
		 NULL AS daticulocale, NULL AS daticurules, NULL AS datcollversion,
		 0 AS datistemplate, 1 AS datallowconn, -1 AS datconnlimit, 0 AS dattablespace,
		 0 AS datfrozenxid, 0 AS datminmxid, NULL AS datacl"#,
        ]
        .concat(),
    ]
}

/// Tracks the schemas this database holds; the source of truth for pg_namespace
/// and for search_path. Public is implicit, never listed.
const SCHEMAS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_schemas (
  name TEXT PRIMARY KEY COLLATE NOCASE
)"#;

lazy_regex!(RE_SCHEMA_NAME, r"^[A-Za-z_][A-Za-z0-9_]*$");

static RESERVED_SCHEMAS: LazyLock<HashMap<&'static str, bool>> = LazyLock::new(|| {
    HashMap::from([
        ("main", true),
        ("temp", true),
        ("public", true),
        ("pg_catalog", true),
        ("information_schema", true),
    ])
});

fn is_reserved_schema(name: &str) -> bool {
    RESERVED_SCHEMAS
        .get(name.to_lowercase().as_str())
        .copied()
        .unwrap_or(false)
}

#[cfg(test)]
fn valid_schema_name(name: &str) -> bool {
    RE_SCHEMA_NAME.is_match(name) && !is_reserved_schema(name)
}

/// Rejects a name overlite cannot carry, saying which rule it breaks.
fn check_schema_name(name: &str) -> Result<()> {
    if is_reserved_schema(name) {
        return Err(errf!(
            SqlState::RESERVED_NAME,
            "schema name {name:?} is reserved",
            name = quoted(name)
        ));
    }
    if !RE_SCHEMA_NAME.is_match(name) {
        return Err(errf!(
            SqlState::FEATURE_NOT_SUPPORTED,
            "invalid schema name {name:?}: a schema name must start with a letter or underscore and contain only letters, digits and underscores, because it becomes part of the name of the file or table that stores it",
            name = quoted(name)
        ));
    }
    Ok(())
}

/// Assigns stable oids/offsets to main (public) plus the given schema names
/// (sorted for determinism). All share "main" and carry a "<name>." table-name
/// prefix.
fn schema_refs(schemas: &mut [String]) -> Vec<SchemaRef> {
    let mut refs = vec![SchemaRef {
        pg_name: "public".to_string(),
        db: "main".to_string(),
        ns_oid: 2200,
        offset: 0,
        prefix: String::new(),
    }];
    schemas.sort();
    for (i, name) in schemas.iter().enumerate() {
        refs.push(SchemaRef {
            pg_name: name.clone(),
            db: "main".to_string(),
            ns_oid: 3000000 + i as i64 + 1,
            // Each schema gets a 200M-wide oid band. It must stay < 2^32 because
            // pg_dump holds oids in a uint32 and would truncate a larger value,
            // breaking its attrelid joins (empty CREATE TABLE). ~21 schemas fit.
            offset: (i as i64 + 1) * 200_000_000,
            prefix: format!("{name}."),
        });
    }
    refs
}

// --- per-connection setup -----------------------------------------------------

/// The connection hook: exec and query, where a cell is rendered with
/// `Value::display_string`.
pub(crate) fn setup_connection(conn: &Connection, path: &str) -> Result<()> {
    // A database with no table yet holds nothing in another form: it starts in the
    // storage format, as ensure_storage_format leaves a file it converted.
    crate::storage::mark_if_fresh(conn)?;
    let exec = |q: &str| -> Result<()> { do_exec(conn, q, &[], "").map(|_| ()) };
    let query = |q: &str| -> Result<Vec<String>> {
        let rs = do_query(conn, q, &[], "")?;
        Ok(rs.rows.iter().map(|r| r[0].to_string()).collect())
    };
    setup_connection_with(&exec, &query, path)
}

/// Creates what is missing and (re)creates all catalog views. Called from the
/// connection hook and after CREATE/DROP SCHEMA.
fn setup_connection_with(
    exec: &dyn Fn(&str) -> Result<()>,
    query: &dyn Fn(&str) -> Result<Vec<String>>,
    main_path: &str,
) -> Result<()> {
    let st = state_for(main_path);
    // Only what is missing is written. Writing takes the database's write lock,
    // and a new connection that wrote its bootstrap every time waited on any
    // client's open write transaction, past the session timeout: refused as
    // "too many clients" with the server empty. So it reads first.
    let mut tables: HashSet<String> = HashSet::new();
    if let Ok(names) = query("SELECT name FROM sqlite_master WHERE type = 'table'") {
        for n in names {
            tables.insert(n.to_lowercase());
        }
    }
    let create = |ddl: &str| -> Result<()> {
        if let Some(m) = CREATED_TABLE_RE.captures(ddl)
            && tables.contains(&m[1].to_lowercase())
        {
            return Ok(());
        }
        exec(ddl)
    };
    let add_column = |ddl: &str| {
        if let Some(m) = ADDED_COLUMN_RE.captures(ddl) {
            let cols = query(&format!("SELECT name FROM pragma_table_info({})", quote_literal(&m[1])));
            if let Ok(cols) = cols {
                for c in &cols {
                    if eq_fold(c, &m[2]) {
                        return;
                    }
                }
            }
        }
        let _ = exec(ddl); // an older table without the column; errors harmlessly if present
    };
    // The schema registry is the list of schemas this database holds.
    create(SCHEMAS_TABLE_DDL)?;
    let mut schemas: Vec<String> = Vec::new();
    if let Ok(names) = query("SELECT name FROM _overlite_schemas ORDER BY name") {
        st.set_schemas(&names); // the schema-qualifier rewrite reads this
        schemas = names;
    }

    // The internal roles table (pg_roles reads from it) must exist before the
    // meta views are built.
    create(ROLES_TABLE_DDL)?;
    add_column(ROLES_ADD_PASSWORD_DDL);
    let has = query(&format!(
        "SELECT 1 FROM _overlite_roles WHERE rolname = {}",
        quote_literal(&CATALOG_ROLE)
    ));
    if has.map_or(true, |h| h.is_empty()) {
        exec(&seed_default_role_sql())?;
    }
    // The internal sequences table (nextval/currval/... read and write it).
    create(SEQUENCES_TABLE_DDL)?;
    create(COLUMN_DEFAULTS_TABLE_DDL)?;
    // The internal enum tables (pg_type/pg_enum read them; enum columns become
    // TEXT + CHECK).
    create(ENUM_TYPES_TABLE_DDL)?;
    create(ENUM_LABELS_TABLE_DDL)?;
    create(COMPOSITE_TYPES_TABLE_DDL)?;
    // COMMENT ON storage (pg_description reads it).
    create(COMMENTS_TABLE_DDL)?;
    create(crate::catalog::comments::EXTENSIONS_TABLE_DDL)?;
    create(crate::catalog::comments::COLLATIONS_TABLE_DDL)?;
    if let Ok(rows) =
        query("SELECT collname || char(9) || locale || char(9) || deterministic FROM _overlite_collations")
    {
        for row in rows {
            let mut f = row.split('\t');
            if let (Some(name), Some(locale), Some(det)) = (f.next(), f.next(), f.next())
                && !matches!(locale.to_ascii_lowercase().as_str(), "" | "c" | "posix")
            {
                crate::builtins::collation::register_collation(
                    name,
                    crate::builtins::collation::spec_of_locale(locale, det != "0"),
                );
            }
        }
    }
    // LANGUAGE sql function definitions (inlined at call sites).
    create(FUNCTIONS_TABLE_DDL)?;
    for ddl in &FUNCTIONS_ADD_COLS_DDL {
        add_column(ddl);
    }
    // The internal privilege tables (GRANT/REVOKE storage + table ownership),
    // consulted by the protocol before running a statement.
    create(GRANTS_TABLE_DDL)?;
    create(COLUMN_GRANTS_TABLE_DDL)?;
    create(OWNERS_TABLE_DDL)?;
    create(MEMBERSHIPS_TABLE_DDL)?;
    add_column(MEMBERSHIPS_ADD_ADMIN_DDL);
    // Row-level security flags and policies.
    create(RLS_TABLE_DDL)?;
    create(POLICIES_TABLE_DDL)?;
    // Before a statement can run: until then, a table may be protected only by a row
    // this server would not read as its own. Failing here beats serving it open.
    let upgrade_once = |mark: &str, upgrade: &str| -> Result<()> {
        let marked = format!("SELECT name FROM sqlite_master WHERE name = '{mark}'");
        if !query(&marked)?.is_empty() {
            return Ok(());
        }
        exec("BEGIN IMMEDIATE")?;
        let upgraded = if query(&marked)?.is_empty() {
            exec(upgrade).and_then(|()| exec(&format!("CREATE TABLE {mark} (x)")))
        } else {
            Ok(())
        };
        match upgraded {
            Ok(()) => exec("COMMIT"),
            Err(e) => {
                let _ = exec("ROLLBACK");
                Err(e)
            }
        }
    };
    upgrade_once(RLS_STORED_NAMES_MARK, RLS_STORED_NAMES_UPGRADE)?;
    upgrade_once(DEFAULTS_STORED_NAMES_MARK, DEFAULTS_STORED_NAMES_UPGRADE)?;
    // Expose each sequence as a one-row relation (last_value/is_called), as
    // Postgres does, so pg_dump can read its current value with
    // "SELECT last_value, is_called FROM <seq>".
    if let Ok(seqs) = query(
        "SELECT seqname FROM _overlite_sequences WHERE seqname NOT IN (SELECT name FROM sqlite_master WHERE type IN ('table','view'))",
    ) {
        for name in &seqs {
            let _ = exec(&format!(
                "CREATE TEMP VIEW IF NOT EXISTS \"{}\" AS SELECT last_value, 0 AS log_cnt, is_called FROM _overlite_sequences WHERE seqname = {}",
                name.replace('"', "\"\""),
                quote_literal(name)
            ));
        }
    }
    // Refresh the global enum oid->name registry that format_type() reads.
    if let Ok(names) = query(&format!(
        "SELECT (rowid + {}) || ':' || typname FROM _overlite_enum_types",
        st.enum_base()
    )) {
        refresh_enum_names(&names);
    }
    // Refresh the LANGUAGE sql function cache (columns joined by char(30)).
    if let Ok(rows) = query(
        "SELECT rowid || char(30) || name || char(30) || arity || char(30) || params || char(30) || body || char(30) || lang || char(30) || args || char(30) || rettype || char(30) || src FROM _overlite_functions",
    ) {
        refresh_functions_from_rows(&st, &rows);
    }
    // Refresh the trigger oid->definition registry that pg_get_triggerdef() reads
    // (public schema; oid matches the pg_trigger view).
    if let Ok(defs) = query(
        "SELECT (rowid + 70000000) || char(31) || sql FROM sqlite_master WHERE type = 'trigger' AND sql IS NOT NULL AND name NOT GLOB '_overlite_*'",
    ) {
        refresh_trigger_defs(&defs);
    }

    let refs = schema_refs(&mut schemas);
    for stmt in STATIC_CATALOG_VIEWS {
        exec(&with_table_oid(&st.fill_enum_base(stmt)))?;
    }
    // Meta + schema-spanning views are rebuilt (DROP + CREATE) so they pick up
    // the current role/database name and schema set.
    let mut rebuilt = meta_catalog_views(st.db_name());
    rebuilt.extend(dynamic_catalog_views(&refs, st.db_name(), st.enum_base()));
    for stmt in &rebuilt {
        exec(&drop_view_of(stmt))?;
        exec(stmt)?;
    }
    // Each serial column's sequence as a relation too, after the views that list them, from what SQLite keeps of an AUTOINCREMENT, which is
    // nothing until a first row: sqlite_sequence is made then.
    let has_sqlite_sequence =
        query("SELECT name FROM sqlite_master WHERE name = 'sqlite_sequence'").is_ok_and(|rows| !rows.is_empty());
    if let Ok(rows) = query(
        "SELECT seqname || char(30) || tablename FROM _overlite_serial_sequences WHERE schemaname = 'public' AND seqname NOT IN (SELECT name FROM sqlite_master WHERE type IN ('table','view'))",
    ) {
        for row in &rows {
            let Some((name, table)) = row.split_once('\u{1e}') else {
                continue;
            };
            let body = if has_sqlite_sequence {
                format!(
                    "SELECT COALESCE((SELECT seq FROM main.sqlite_sequence WHERE name = {t}), 1) AS last_value, 0 AS log_cnt, EXISTS (SELECT 1 FROM main.sqlite_sequence WHERE name = {t}) AS is_called",
                    t = quote_literal(table)
                )
            } else {
                "SELECT 1 AS last_value, 0 AS log_cnt, 0 AS is_called".to_string()
            };
            let _ = exec(&format!(
                "CREATE TEMP VIEW IF NOT EXISTS \"{}\" AS {body}",
                name.replace('"', "\"\"")
            ));
        }
    }
    Ok(())
}

lazy_regex!(
    CREATED_TABLE_RE,
    r#"(?i)^\s*CREATE\s+TABLE\s+IF\s+NOT\s+EXISTS\s+"?([A-Za-z0-9_]+)"?"#
);
lazy_regex!(
    ADDED_COLUMN_RE,
    r#"(?i)^\s*ALTER\s+TABLE\s+"?([A-Za-z0-9_]+)"?\s+ADD\s+COLUMN\s+"?([A-Za-z0-9_]+)"?"#
);

/// A DROP VIEW statement for the view a CREATE statement defines, so dynamic
/// views can be rebuilt when the schema set changes.
fn drop_view_of(create_stmt: &str) -> String {
    format!("DROP VIEW IF EXISTS {}", view_name(create_stmt))
}

lazy_regex!(
    RE_VIEW_NAME,
    r#"(?i)CREATE\s+TEMP\s+VIEW\s+(?:IF\s+NOT\s+EXISTS\s+)?("[^"]+"|\w+)"#
);

fn view_name(create_stmt: &str) -> String {
    match RE_VIEW_NAME.captures(create_stmt) {
        Some(m) => m[1].to_string(),
        None => String::new(),
    }
}

// --- schema management --------------------------------------------------------
//
// Schema DDL runs on a single connection (the engine's own, or a client
// session's), rebuilding that connection's catalog. Other connections rebuild theirs
// on their next statement, once the change is committed: see DbSession::publish_schemas.

/// A cell read as a string: NULL is empty.
fn scan_strings(rs: &ResultSet) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(rs.rows.len());
    for row in &rs.rows {
        if row[0].is_null() {
            return Err(errf!(
                SqlState::INTERNAL_ERROR,
                "sql: Scan error on column index 0, name {:?}: converting NULL to string is unsupported",
                quoted(&rs.columns[0].name)
            ));
        }
        out.push(row[0].to_text_lossy());
    }
    Ok(out)
}

fn create_schema(ce: &Connection, main_path: &str, name: &str, if_not_exists: bool) -> Result<()> {
    if eq_fold(name, "public") {
        if if_not_exists {
            return Ok(());
        }
        return Err(errf!(
            SqlState::DUPLICATE_SCHEMA,
            "schema {name:?} already exists",
            name = quoted(name)
        ));
    }
    check_schema_name(name)?;
    create_schema_single(ce, main_path, name, if_not_exists)
}

/// Records a schema in _overlite_schemas. It's an ordinary write, so it works
/// inside a transaction and in :memory:.
fn create_schema_single(ce: &Connection, main_path: &str, name: &str, if_not_exists: bool) -> Result<()> {
    do_exec(ce, SCHEMAS_TABLE_DDL, &[], "")?;
    if schema_registered(ce, name) {
        if if_not_exists {
            return Ok(());
        }
        return Err(errf!(
            SqlState::DUPLICATE_SCHEMA,
            "schema {name:?} already exists",
            name = quoted(name)
        ));
    }
    do_exec(
        ce,
        "INSERT INTO _overlite_schemas (name) VALUES (?)",
        &[Value::from(name)],
        "",
    )?;
    rebuild_catalog(ce, main_path)
}

fn drop_schema(ce: &Connection, main_path: &str, name: &str, if_exists: bool, cascade: bool) -> Result<()> {
    if eq_fold(name, "public") {
        return Err(errf!(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop schema {name:?}",
            name = quoted(name)
        ));
    }
    drop_schema_single(ce, main_path, name, if_exists, cascade)
}

/// Removes a schema and (with CASCADE) its prefixed tables. Fully transactional.
fn drop_schema_single(ce: &Connection, main_path: &str, name: &str, if_exists: bool, cascade: bool) -> Result<()> {
    if !schema_registered(ce, name) {
        if if_exists {
            return Ok(());
        }
        return Err(errf!(
            SqlState::INVALID_SCHEMA_NAME,
            "schema {name:?} does not exist",
            name = quoted(name)
        ));
    }
    let tables = schema_tables(ce, name)?;
    if !tables.is_empty() && !cascade {
        return Err(errf!(
            SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop schema {name} because other objects depend on it"
        ));
    }
    for t in &tables {
        do_exec(ce, &format!("DROP TABLE IF EXISTS {t:?}", t = quoted(t)), &[], "")?;
        do_exec(ce, &forget_table_rows_sql(t), &[], "")?;
    }
    do_exec(
        ce,
        "DELETE FROM _overlite_schemas WHERE name = ? COLLATE NOCASE",
        &[Value::from(name)],
        "",
    )?;
    rebuild_catalog(ce, main_path)
}

/// Whether a schema is in the registry.
fn schema_registered(ce: &Connection, name: &str) -> bool {
    match do_query(
        ce,
        "SELECT name FROM _overlite_schemas WHERE name = ? COLLATE NOCASE",
        &[Value::from(name)],
        "",
    ) {
        Ok(rs) => rs.rows.first().is_some_and(|r| !r[0].is_null()),
        Err(_) => false,
    }
}

/// The SQLite table names ("vendas.pedidos") belonging to a schema.
fn schema_tables(ce: &Connection, name: &str) -> Result<Vec<String>> {
    let rs = do_query(
        ce,
        "SELECT name FROM main.sqlite_master WHERE type='table' AND name LIKE ? ESCAPE '\\'",
        &[Value::Text(format!("{}.%", escape_like(name)))],
        "",
    )?;
    scan_strings(&rs)
}

/// Escapes LIKE metacharacters in a literal (used with ESCAPE '\').
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || c == '%' || c == '_' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Re-runs the catalog setup on the given connection.
pub(crate) fn rebuild_catalog(ce: &Connection, main_path: &str) -> Result<()> {
    let exec = |q: &str| -> Result<()> { do_exec(ce, q, &[], "").map(|_| ()) };
    let q = |query: &str| -> Result<Vec<String>> {
        let rs = do_query(ce, query, &[], "")?;
        scan_strings(&rs)
    };
    setup_connection_with(&exec, &q, main_path)
}

impl DbSession {
    /// A schema is a row in _overlite_schemas plus name-prefixed tables, so
    /// CREATE/DROP SCHEMA are ordinary writes.
    pub fn schema_ddl_transactional(&self) -> bool {
        true
    }

    /// Creates a schema.
    ///
    /// # Errors
    ///
    /// When the name is reserved or taken, or SQLite fails.
    pub fn create_schema(&self, name: &str, if_not_exists: bool) -> Result<()> {
        self.begin_writing(false)?;
        create_schema(&self.conn, &self.main_path, name, if_not_exists)
    }

    /// Drops a schema, and with `cascade` the tables in it.
    ///
    /// # Errors
    ///
    /// When it is missing or not empty, or SQLite fails.
    pub fn drop_schema(&self, name: &str, if_exists: bool, cascade: bool) -> Result<()> {
        self.begin_writing(false)?;
        drop_schema(&self.conn, &self.main_path, name, if_exists, cascade)
    }

    /// Renames a schema.
    ///
    /// # Errors
    ///
    /// When it is `public` or missing, the new name is taken, or SQLite fails.
    pub fn rename_schema(&self, o: &str, n: &str) -> Result<()> {
        self.begin_writing(false)?;
        rename_schema(&self.conn, &self.main_path, o, n)
    }

    /// Moves a table to another schema.
    ///
    /// # Errors
    ///
    /// When the schema is missing, or SQLite fails.
    pub fn set_table_schema(&self, table_ref: &str, sch: &str) -> Result<()> {
        self.begin_writing(false)?;
        set_table_schema(&self.conn, &self.main_path, table_ref, sch)
    }

    pub fn resolve_table(&self, table_ref: &str) -> (String, String, String) {
        resolve_table(table_ref)
    }
}

#[cfg(test)]
impl crate::sqlite::Sqlite {
    pub(crate) fn create_schema(&self, name: &str, if_not_exists: bool) -> Result<()> {
        self.with_own_session(|ss| ss.create_schema(name, if_not_exists))
    }

    pub(crate) fn drop_schema(&self, name: &str, if_exists: bool, cascade: bool) -> Result<()> {
        self.with_own_session(|ss| ss.drop_schema(name, if_exists, cascade))
    }
}

/// Renames a schema and every "old.<t>" table to "new.<t>". A transactional set
/// of renames.
fn rename_schema(ce: &Connection, main_path: &str, old_name: &str, new_name: &str) -> Result<()> {
    if eq_fold(old_name, "public") || eq_fold(new_name, "public") {
        return Err(errf!(
            SqlState::RESERVED_NAME,
            "cannot rename schema {:?}",
            quoted("public")
        ));
    }
    check_schema_name(new_name)?;
    if !schema_registered(ce, old_name) {
        return Err(errf!(
            SqlState::INVALID_SCHEMA_NAME,
            "schema {old_name:?} does not exist",
            old_name = quoted(old_name)
        ));
    }
    if schema_registered(ce, new_name) {
        return Err(errf!(
            SqlState::DUPLICATE_SCHEMA,
            "schema {new_name:?} already exists",
            new_name = quoted(new_name)
        ));
    }
    let tables = schema_tables(ce, old_name)?;
    for t in &tables {
        let nt = format!("{new_name}{}", slice(t, old_name.len(), t.len())); // swap the "<old>" prefix for "<new>"
        do_exec(
            ce,
            &format!("ALTER TABLE {t:?} RENAME TO {nt:?}", t = quoted(t), nt = quoted(&nt)),
            &[],
            "",
        )?;
        do_exec(ce, &move_table_rows_sql(t, &nt), &[], "")?;
    }
    do_exec(
        ce,
        "UPDATE _overlite_schemas SET name = ? WHERE name = ? COLLATE NOCASE",
        &[Value::from(new_name), Value::from(old_name)],
        "",
    )?;
    rebuild_catalog(ce, main_path)
}

/// Moves a table to another schema by renaming it. SQLite's RENAME updates
/// foreign keys/views/triggers that reference it.
fn set_table_schema(ce: &Connection, main_path: &str, table_ref: &str, new_schema: &str) -> Result<()> {
    let (src_schema, name) = split_schema_ref(table_ref);
    if !eq_fold(new_schema, "public") && !schema_registered(ce, new_schema) {
        return Err(errf!(
            SqlState::INVALID_SCHEMA_NAME,
            "schema {new_schema:?} does not exist",
            new_schema = quoted(new_schema)
        ));
    }
    let src = stored_table_name(&src_schema, &name);
    let dst = stored_table_name(new_schema, &name);
    if src == dst {
        return Ok(());
    }
    do_exec(
        ce,
        &format!(
            "ALTER TABLE {src:?} RENAME TO {dst:?}",
            src = quoted(&src),
            dst = quoted(&dst)
        ),
        &[],
        "",
    )?;
    do_exec(ce, &move_table_rows_sql(&src, &dst), &[], "")?;
    rebuild_catalog(ce, main_path)
}

/// Splits "schema.table" into its parts; a bare name is public.
fn split_schema_ref(table_ref: &str) -> (String, String) {
    let r = trim_chars(table_ref.trim(), "\"");
    if let Some(i) = r.find('.') {
        return (r[..i].to_string(), r[i + 1..].to_string());
    }
    ("public".to_string(), r.to_string())
}

/// The SQLite table name for a (schema, table): bare in public,
/// `"<schema>.<table>"` otherwise.
pub fn stored_table_name(schema: &str, table: &str) -> String {
    if schema.is_empty() || eq_fold(schema, "public") {
        return table.to_string();
    }
    format!("{schema}.{table}")
}

/// A schema keeps its tables under a prefixed name (`sales.orders`) in the one
/// file, so callers above the engine get the stored name, where to read it back
/// from, and how to write it in a statement.
fn resolve_table(table_ref: &str) -> (String, String, String) {
    let (schema, table) = split_schema_ref(table_ref);
    // Quoted whatever it looks like: a word SQLite reserves looks like any other.
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    if schema.is_empty() || eq_fold(&schema, "public") {
        let q = quote(&table);
        return (table, "main.sqlite_master".to_string(), q);
    }
    let stored = format!("{schema}.{table}");
    let q = quote(&stored);
    (stored, "main.sqlite_master".to_string(), q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::Sqlite;
    use crate::sqlite::tests::{TempDir, exec, must_exec, text_or_empty};

    #[test]
    fn schema_lifecycle() {
        let dir = TempDir::new();
        let main = dir.join("system.db");

        let eng = Sqlite::open(&main).unwrap();

        eng.create_schema("vendas", false).unwrap();

        // A table created in the schema is queryable through it.
        must_exec(&eng, "CREATE TABLE vendas.pedidos (id INTEGER PRIMARY KEY, total REAL)");
        must_exec(&eng, "INSERT INTO vendas.pedidos (total) VALUES (9.9)");
        let sel = must_exec(&eng, "SELECT total FROM vendas.pedidos");
        assert_eq!(sel.rows.len(), 1);

        // The catalog reflects the schema and its table under the right namespace.
        let ns = must_exec(&eng, "SELECT nspname FROM pg_namespace WHERE nspname = 'vendas'");
        assert_eq!(ns.rows.len(), 1);

        let rel = must_exec(
            &eng,
            "
		SELECT c.relname FROM pg_class c
		JOIN pg_namespace n ON n.oid = c.relnamespace
		WHERE n.nspname = 'vendas' AND c.relkind = 'r'",
        );
        assert_eq!(rel.rows.len(), 1);
        assert_eq!(text_or_empty(&rel.rows[0][0]), "pedidos");

        // public and vendas tables don't collide in the catalog.
        must_exec(&eng, "CREATE TABLE clientes (id INTEGER PRIMARY KEY)");
        let all = must_exec(&eng, "SELECT count(*) FROM pg_class WHERE relkind = 'r'");
        assert_eq!(all.rows[0][0], Value::Int(2)); // clientes (public) + pedidos (vendas)

        // DROP SCHEMA without cascade refuses a non-empty schema.
        assert!(eng.drop_schema("vendas", false, false).is_err());
        eng.drop_schema("vendas", false, true).unwrap();

        // And it is gone from the catalog.
        let gone = must_exec(&eng, "SELECT count(*) FROM pg_namespace WHERE nspname = 'vendas'");
        assert_eq!(gone.rows[0][0], Value::Int(0));
    }

    #[test]
    fn schema_persists_across_reopen() {
        let dir = TempDir::new();
        let main = dir.join("system.db");

        let eng = Sqlite::open(&main).unwrap();
        eng.create_schema("auth", false).unwrap();
        must_exec(&eng, "CREATE TABLE auth.users (id INTEGER PRIMARY KEY, email TEXT)");
        drop(eng);

        // Reopening rediscovers and reattaches the schema file.
        let eng2 = Sqlite::open(&main).unwrap();

        exec(&eng2, "SELECT email FROM auth.users", &[]).unwrap();

        let ns = must_exec(&eng2, "SELECT nspname FROM pg_namespace WHERE nspname = 'auth'");
        assert_eq!(ns.rows.len(), 1);
    }

    #[test]
    fn schema_discovery_multiple() {
        let dir = TempDir::new();
        let main = dir.join("hello.db");

        // First run: create several schemas, each with a table.
        let eng = Sqlite::open(&main).unwrap();
        for s in ["bla", "ble", "vendas"] {
            eng.create_schema(s, false).unwrap();
            must_exec(&eng, &format!("CREATE TABLE {s}.t (id INTEGER PRIMARY KEY, v TEXT)"));
            must_exec(&eng, &format!("INSERT INTO {s}.t (v) VALUES ('{s}-row')"));
        }
        drop(eng);

        // Second run: a fresh engine must find every schema again.
        let eng2 = Sqlite::open(&main).unwrap();

        // Every schema shows up in the catalog.
        let ns = must_exec(
            &eng2,
            "
		SELECT nspname FROM pg_namespace
		WHERE nspname NOT IN ('pg_catalog','information_schema')
		ORDER BY nspname",
        );
        let got: Vec<String> = ns.rows.iter().map(|row| text_or_empty(&row[0])).collect();
        assert_eq!(got, ["bla", "ble", "public", "vendas"]);

        // Each schema's data is queryable and the tables don't collide.
        for s in ["bla", "ble", "vendas"] {
            let sel = exec(&eng2, &format!("SELECT v FROM {s}.t"), &[]).unwrap_or_else(|e| panic!("query {s}.t: {e}"));
            assert_eq!(sel.rows.len(), 1);
            assert_eq!(text_or_empty(&sel.rows[0][0]), format!("{s}-row"));
        }

        // pg_class exposes all three tables under their own namespaces.
        let rels = must_exec(
            &eng2,
            "
		SELECT n.nspname, c.relname FROM pg_class c
		JOIN pg_namespace n ON n.oid = c.relnamespace
		WHERE c.relkind = 'r' AND n.nspname <> 'public'
		ORDER BY n.nspname",
        );
        assert_eq!(rels.rows.len(), 3);
    }

    #[test]
    fn schema_name_validation() {
        assert!(valid_schema_name("vendas"));
        assert!(valid_schema_name("auth_2"));
        assert!(!valid_schema_name("public")); // reserved
        assert!(!valid_schema_name("main")); // reserved
        assert!(!valid_schema_name("with space"));
        assert!(!valid_schema_name("1abc"));
        assert!(!valid_schema_name("a.b"));
    }
}
