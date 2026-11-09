//! Per-database state.

use crate::catalog::enums::{ENUM_OID_BAND_WIDTH, ENUM_OID_BASE};
use crate::catalog::rls_cache::RlsCache;
use crate::catalog::schema::db_name_from_path;
use overlite_core::sync::{MutexExt, RwLockExt};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

#[derive(Debug)]
pub(crate) struct DbState {
    /// What current_database() answers.
    name: String,
    /// Keeps one database's generated oids from colliding with another's.
    oid_band: i64,
    schemas: RwLock<Arc<Vec<String>>>,
    /// Bumped when the set of schemas changes, so that a connection already open
    /// rebuilds the catalog views it built from the old set.
    schema_generation: AtomicU64,
    pub(crate) rls: RlsCache,
    /// The columns of the tables and views, as the AST path types them
    /// (table_types.rs); None until first asked.
    pub(crate) table_types: RwLock<Option<Arc<crate::table_types::TableTypes>>>,
    /// Bumped by each forget_table_types, so a snapshot read across one is not kept.
    pub(crate) table_types_version: AtomicU64,
    /// The names, lower-cased, of the LANGUAGE sql functions a call may reach
    /// (sql_funcs): each connection that changes them, or opens, reads them again.
    pub(crate) func_names: RwLock<Arc<HashSet<String>>>,
}

static DB_STATES: LazyLock<Mutex<HashMap<String, Arc<DbState>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_OID_BAND: AtomicI64 = AtomicI64::new(0);

/// The state for a database file, created on first use.
pub(crate) fn state_for(path: &str) -> Arc<DbState> {
    let mut states = DB_STATES.locked();
    if let Some(st) = states.get(path) {
        return st.clone();
    }
    let band = NEXT_OID_BAND.fetch_add(1, Ordering::SeqCst) + 1;
    let st = Arc::new(DbState {
        name: db_name_from_path(path),
        oid_band: band * ENUM_OID_BAND_WIDTH,
        schemas: RwLock::new(Arc::new(Vec::new())),
        schema_generation: AtomicU64::new(0),
        rls: RlsCache::default(),
        table_types: RwLock::new(None),
        table_types_version: AtomicU64::new(0),
        func_names: RwLock::new(Arc::new(HashSet::new())),
    });
    states.insert(path.to_string(), st.clone());
    st
}

/// Drops a database's state, so a file deleted and recreated does not inherit it.
pub(crate) fn forget_state(path: &str) {
    DB_STATES.locked().remove(path);
}

impl DbState {
    pub(crate) fn set_schemas(&self, names: &[String]) {
        *self.schemas.write_locked() = Arc::new(names.to_vec());
    }

    /// Which set of schemas the database is at.
    pub(crate) fn schema_generation(&self) -> u64 {
        self.schema_generation.load(Ordering::Acquire)
    }

    /// Says the set of schemas changed, and which generation that makes.
    pub(crate) fn next_schema_generation(&self) -> u64 {
        self.schema_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn schema_list(&self) -> Arc<Vec<String>> {
        self.schemas.read_locked().clone()
    }

    /// Where this database's enum type oids start.
    pub(crate) fn enum_base(&self) -> i64 {
        ENUM_OID_BASE + self.oid_band
    }

    /// Substitutes this database's enum oid base into a catalog view.
    pub(crate) fn fill_enum_base(&self, stmt: &str) -> String {
        stmt.replace("@ENUMBASE@", &self.enum_base().to_string())
    }

    /// The name current_database() answers.
    pub(crate) fn db_name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::sqlite::Sqlite;
    use crate::sqlite::tests::{TempDir, exec, must_exec, text_or_empty};

    // A process serves one engine per database file. Anything the catalog or the
    // statement rewrites know about "the database" therefore has to be per engine:
    // while it was process-global, the second engine to open overwrote the first's
    // name and schema list, and every connection got whichever won.
    #[test]
    fn two_databases_in_one_process() {
        let dir = TempDir::new();

        let shop = Sqlite::open(&dir.join("shop.db")).unwrap();
        let blog = Sqlite::open(&dir.join("blog.db")).unwrap();

        // Each answers with its own name, whichever opened last.
        let name = |e: &Sqlite| text_or_empty(&must_exec(e, "SELECT current_database()").rows[0][0]);
        assert_eq!(name(&shop), "shop");
        assert_eq!(name(&blog), "blog");

        // Schemas are per database: one's list must not leak into the other's
        // name resolution.
        shop.create_schema("vendas", false).unwrap();
        must_exec(&shop, "CREATE TABLE vendas.pedidos (id INTEGER PRIMARY KEY)");

        let schemas = |e: &Sqlite| -> Vec<String> {
            let rs = must_exec(
                e,
                "SELECT nspname FROM pg_namespace
			WHERE nspname NOT IN ('pg_catalog','information_schema') ORDER BY nspname",
            );
            rs.rows.iter().map(|r| text_or_empty(&r[0])).collect()
        };
        assert_eq!(schemas(&shop), ["public", "vendas"]);
        assert_eq!(schemas(&blog), ["public"], "the other database's schema leaked");

        // blog has no "vendas", so the name must not resolve there.
        assert!(
            exec(&blog, "SELECT * FROM vendas.pedidos", &[]).is_err(),
            "a table from another database was reachable"
        );

        // And the tables stay apart.
        must_exec(&shop, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        must_exec(&shop, "INSERT INTO t (v) VALUES ('shop-row')");
        must_exec(&blog, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)");
        must_exec(&blog, "INSERT INTO t (v) VALUES ('blog-row')");

        for (eng, want) in [(&shop, "shop-row"), (&blog, "blog-row")] {
            let rs = must_exec(eng, "SELECT v FROM t");
            assert_eq!(rs.rows.len(), 1);
            assert_eq!(text_or_empty(&rs.rows[0][0]), want);
        }

        // The catalog names each database as its own. At this level the view
        // carries its quoted dotted name; the protocol is what lets a client write
        // information_schema.tables.
        let catalog = |e: &Sqlite| {
            let rs = must_exec(
                e,
                r#"SELECT table_catalog FROM "information_schema.tables" WHERE table_name = 't'"#,
            );
            text_or_empty(&rs.rows[0][0])
        };
        assert_eq!(catalog(&shop), "shop");
        assert_eq!(catalog(&blog), "blog");
    }
}
