//! The columns of every table and view of a database with their declared types, read
//! once and kept until a statement may have changed them: what the AST path's type
//! environment (crates/postgres/src/ast/types.rs) knows of the catalog.
//!
//! The text passes of `resolve` prepare a probe over a statement's FROM to learn a
//! column's type, on each execution. The AST path reads its scopes from the tree and
//! looks each table up here instead, once per translation, and its translation is
//! kept for as long as the snapshot it read ([`TableTypes::generation`]) stands.
//!
//! A snapshot is dropped by every CREATE, ALTER and DROP this server runs and by a
//! rollback; a file changed by another process is not seen until then, as for the
//! probes' cache (`resolve/time_zone.rs`).

use crate::db_state::DbState;
use overlite_core::sync::RwLockExt;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A column of a table or view as the catalog declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnType {
    /// The name as the table spells it.
    pub name: String,
    /// The declared type as SQLite keeps it (`DECIMALTEXT(10,2)`, `timestamptz`, `text[]`).
    pub decl: String,
    /// A boolean column that may hold text as well as 1 and 0 (a mixed column, or any
    /// boolean of a file not in the storage format, or of a view while some column of
    /// the file is mixed): the readings of resolve/booleans.rs for text apply to it.
    pub mixed_bool: bool,
    /// The table's rowid under its own name (`INTEGER PRIMARY KEY`): a serial and a
    /// bigserial key are both declared INTEGER there, so its width is not known.
    pub rowid: bool,
}

/// The columns of a database's tables and views, by stored name in lower case.
#[derive(Debug, Default)]
pub struct TableTypes {
    /// Unique across databases and snapshots: a translation made from this one holds
    /// for as long as the database's snapshot has this generation.
    pub generation: u64,
    tables: HashMap<String, Arc<[ColumnType]>>,
}

/// Generations are counted for the whole process, so that two databases never share one.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

impl TableTypes {
    /// The columns of the table or view whose stored name (`t`, `s.t`) is name, case
    /// ignored, in their order.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&[ColumnType]> {
        match self.tables.get(name) {
            Some(cols) => Some(cols),
            None => self.tables.get(&name.to_lowercase()).map(|c| &c[..]),
        }
    }

    /// A snapshot of these tables, for tests: each `(table, [(column, declared type)])`.
    #[must_use]
    pub fn of(tables: &[(&str, &[(&str, &str)])]) -> TableTypes {
        TableTypes {
            generation: NEXT_GENERATION.fetch_add(1, Ordering::AcqRel),
            tables: tables
                .iter()
                .map(|(t, cols)| {
                    let cols: Vec<ColumnType> = cols
                        .iter()
                        .map(|(n, d)| ColumnType {
                            name: (*n).to_string(),
                            decl: (*d).to_string(),
                            mixed_bool: false,
                            rowid: false,
                        })
                        .collect();
                    (t.to_lowercase(), Arc::from(cols))
                })
                .collect(),
        }
    }

    fn read(q: &Connection) -> TableTypes {
        let user = |p: &str| {
            format!(r"{p}name NOT LIKE 'sqlite\_%' ESCAPE '\' AND {p}name NOT LIKE '\_overlite\_%' ESCAPE '\'")
        };
        // (table, column, declared type, pk position, is a view)
        let mut rows: Vec<(String, String, String, i64, bool)> = Vec::new();
        let tables = format!(
            "SELECT m.name, p.name, p.type, p.pk FROM sqlite_master AS m, pragma_table_info(m.name) AS p
             WHERE m.type = 'table' AND {} ORDER BY m.name, p.cid",
            user("m.")
        );
        if let Ok(mut st) = q.prepare(&tables)
            && let Ok(it) = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, false)))
        {
            rows.extend(it.filter_map(std::result::Result::ok));
        }
        // A view is read on its own: one that no longer compiles fails
        // pragma_table_info, and has no columns to read anyway.
        let views: Vec<String> = q
            .prepare(&format!(
                "SELECT name FROM sqlite_master WHERE type = 'view' AND {}",
                user("")
            ))
            .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect())
            .unwrap_or_default();
        for view in views {
            if let Ok(mut st) = q.prepare("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid")
                && let Ok(it) = st.query_map([&view], |r| Ok((view.clone(), r.get(0)?, r.get(1)?, 0, true)))
            {
                rows.extend(it.filter_map(std::result::Result::ok));
            }
        }
        let formatted = crate::storage::current_version(q) >= crate::storage::FORMAT_VERSION;
        let mixed: HashSet<(String, String)> = crate::storage::mixed_columns(q).into_iter().collect();
        let mut keys: HashMap<String, usize> = HashMap::new();
        for (table, _, _, pk, _) in &rows {
            if *pk > 0 {
                *keys.entry(table.to_lowercase()).or_default() += 1;
            }
        }
        let mut all: HashMap<String, Vec<ColumnType>> = HashMap::new();
        for (table, col, decl, pk, view) in rows {
            let lower = table.to_lowercase();
            let is_bool = matches!(decl.trim().to_ascii_lowercase().as_str(), "boolean" | "bool");
            let listed = mixed.contains(&(lower.clone(), col.to_lowercase()));
            let mixed_bool = is_bool && (!formatted || listed || (view && !mixed.is_empty()));
            let rowid = pk == 1 && keys.get(&lower) == Some(&1) && decl.eq_ignore_ascii_case("integer");
            all.entry(lower).or_default().push(ColumnType {
                name: col,
                decl,
                mixed_bool,
                rowid,
            });
        }
        TableTypes {
            generation: NEXT_GENERATION.fetch_add(1, Ordering::AcqRel),
            tables: all.into_iter().map(|(t, c)| (t, Arc::from(c))).collect(),
        }
    }
}

impl DbState {
    /// The snapshot, read on this connection when there is none. One read while a
    /// statement of another connection changed the schema is used for this statement
    /// and not kept, so that no snapshot outlives the change it missed.
    pub(crate) fn table_types(&self, q: &Connection) -> Arc<TableTypes> {
        let version = self.table_types_version.load(Ordering::Acquire);
        if let Some(t) = self.table_types.read_locked().clone() {
            return t;
        }
        let t = Arc::new(TableTypes::read(q));
        let mut kept = self.table_types.write_locked();
        if self.table_types_version.load(Ordering::Acquire) == version {
            *kept = Some(t.clone());
        }
        t
    }

    /// Drops the snapshot, after a statement that may change the schema.
    pub(crate) fn forget_table_types(&self) {
        let mut kept = self.table_types.write_locked();
        self.table_types_version.fetch_add(1, Ordering::AcqRel);
        *kept = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tables_and_views() {
        let q = Connection::open_in_memory().unwrap();
        q.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a DECIMALTEXT(6,2), b BOOLEAN, ts timestamptz);
             CREATE TABLE u (a int, b int, PRIMARY KEY (a, b));
             CREATE VIEW v AS SELECT a, ts FROM t",
        )
        .unwrap();
        let t = TableTypes::read(&q);
        let cols = t.table("T").unwrap();
        assert_eq!(
            cols.iter()
                .map(|c| (c.name.as_str(), c.decl.as_str()))
                .collect::<Vec<_>>(),
            [
                ("id", "INTEGER"),
                ("a", "DECIMALTEXT(6,2)"),
                ("b", "BOOLEAN"),
                ("ts", "timestamptz")
            ]
        );
        assert!(cols[0].rowid && !cols[1].rowid);
        // Not in the storage format: a boolean may hold text.
        assert!(cols[2].mixed_bool);
        assert!(t.table("u").unwrap().iter().all(|c| !c.rowid));
        assert_eq!(t.table("v").unwrap()[1].decl, "timestamptz");
        assert!(t.table("w").is_none());
    }
}
