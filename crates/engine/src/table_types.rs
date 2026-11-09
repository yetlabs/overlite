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
    /// The names of the columns whose type makes the AST path's typed translation of a
    /// statement differ from the statement as written (`typed_differs`).
    typed_columns: HashSet<String>,
    /// Whether some column is a boolean that may hold text (`ColumnType::mixed_bool`).
    pub any_mixed_bool: bool,
    /// The tables with an integer column narrower than a bigint, which a star reads.
    narrow_tables: HashSet<String>,
    /// The lengths and first bytes of those names, which most words of a statement are
    /// told from before any hashing.
    lengths: u64,
    firsts: [u64; 2],
}

/// How a column's type bears on the fast path: whether a statement naming it reads
/// differently typed, and whether it is an integer the protocol sends at its width.
/// Only text, a bigint (or the rowid) and a float read as SQLite reads them; any other
/// type (a date or time, an array, a range, a numeric, a boolean, which a string
/// compared with it is read as, a uuid, json...) has the typed translation write
/// something of its own, which the fast path, with no type pass after it, would not.
fn typed_kind(c: &ColumnType) -> (bool, bool) {
    let d = c.decl.trim().trim_matches('"').to_ascii_lowercase();
    if d.contains('[') {
        return (true, false);
    }
    let base = d.split('(').next().unwrap_or("").trim();
    let narrow = !c.rowid
        && matches!(
            base,
            "smallint" | "int2" | "smallserial" | "serial2" | "integer" | "int" | "int4" | "serial" | "serial4"
        );
    let plain = c.rowid
        || matches!(
            base,
            "text"
                | "varchar"
                | "character varying"
                | "bigint"
                | "int8"
                | "bigserial"
                | "serial8"
                | "real"
                | "float4"
                | "double precision"
                | "float8"
                | "float"
        );
    (narrow || !plain, narrow)
}

impl TableTypes {
    fn new(generation: u64, tables: HashMap<String, Arc<[ColumnType]>>) -> TableTypes {
        let mut typed_columns = HashSet::new();
        let mut narrow_tables = HashSet::new();
        for (t, cols) in &tables {
            for c in cols.iter() {
                let (typed, narrow) = typed_kind(c);
                if typed {
                    typed_columns.insert(c.name.to_ascii_lowercase());
                }
                if narrow {
                    // The table by its name alone, as a statement may name it.
                    narrow_tables.insert(t.rsplit('.').next().unwrap_or(t).to_string());
                }
            }
        }
        let (mut lengths, mut firsts) = (0u64, [0u64; 2]);
        for w in typed_columns.iter().chain(&narrow_tables) {
            lengths |= 1 << w.len().min(63);
            if let Some(&c) = w.as_bytes().first() {
                firsts[usize::from(c >= 64) & 1] |= 1 << (c % 64);
            }
        }
        let any_mixed_bool = tables.values().any(|cols| cols.iter().any(|c| c.mixed_bool));
        TableTypes {
            generation,
            tables,
            typed_columns,
            any_mixed_bool,
            narrow_tables,
            lengths,
            firsts,
        }
    }

    /// Whether the typed translation of sql, a statement the fast path would send as
    /// it is, may say something else: sql names a column of a type the translation
    /// reads (`typed_kind`), or a star over a table with an integer narrower than a
    /// bigint, which it sends at its width. A word that is no column
    /// but has such a name answers true too, which costs a translation, not an answer.
    #[must_use]
    pub fn typed_differs(&self, sql: &str) -> bool {
        if self.typed_columns.is_empty() {
            return false;
        }
        let b = sql.as_bytes();
        let star = b.contains(&b'*');
        let mut word = [0u8; 64];
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c == b'\'' {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
                continue;
            }
            if c.is_ascii_alphabetic() || c == b'_' {
                let mut n = 0;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                    if n < word.len() {
                        word[n] = b[i].to_ascii_lowercase();
                    }
                    n += 1;
                    i += 1;
                }
                if n > word.len() || (i < b.len() && b[i] >= 0x80) {
                    return true;
                }
                let c = word[0];
                if self.lengths & (1 << n.min(63)) == 0 || self.firsts[usize::from(c >= 64) & 1] & (1 << (c % 64)) == 0
                {
                    continue;
                }
                let Ok(w) = std::str::from_utf8(&word[..n]) else {
                    return true;
                };
                if self.typed_columns.contains(w) || (star && self.narrow_tables.contains(w)) {
                    return true;
                }
                continue;
            }
            if c >= 0x80 || c == b'"' {
                return true;
            }
            i += 1;
        }
        false
    }
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
        TableTypes::new(
            NEXT_GENERATION.fetch_add(1, Ordering::AcqRel),
            tables
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
        )
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
        TableTypes::new(
            NEXT_GENERATION.fetch_add(1, Ordering::AcqRel),
            all.into_iter().map(|(t, c)| (t, Arc::from(c))).collect(),
        )
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

    #[test]
    fn typed_columns_take_a_statement_off_the_fast_path() {
        let t = TableTypes::of(&[
            (
                "kv",
                &[
                    ("id", "INTEGER"),
                    ("v", "TEXT"),
                    ("ts", "timestamptz"),
                    ("n", "DECIMALTEXT(12,2)"),
                ],
            ),
            ("w", &[("i", "INTEGER"), ("b", "BIGINT")]),
        ]);
        assert!(!t.typed_differs("SELECT v FROM kv WHERE v = 'ts'"));
        assert!(t.typed_differs("SELECT v FROM kv WHERE ts > '2024-01-02'"));
        assert!(t.typed_differs("SELECT b FROM w WHERE i = 1"));
        assert!(t.typed_differs("SELECT * FROM w"));
        assert!(!t.typed_differs("SELECT b FROM w WHERE b = 6"));
        // A numeric reads as one, not as its text.
        assert!(t.typed_differs("SELECT v FROM kv WHERE n = 1.5"));
        assert!(!TableTypes::of(&[("t", &[("a", "TEXT")])]).typed_differs("SELECT * FROM t"));
    }
}
