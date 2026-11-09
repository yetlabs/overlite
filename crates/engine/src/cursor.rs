//! A query read a few rows at a time: the statement under a cursor, prepared once and
//! stepped as FETCH asks, so that a cursor holds no more rows than a FETCH takes.
//! Going back means stepping again from the start, which a statement allows as often
//! as it is reset: inside a transaction it reads what it read the first time.

use crate::column_types::{apply_strong_types, computed_types};
use crate::error::Result;
use crate::sqlite::{DbSession, Script, Stmt, cell, do_exec, make_columns};
use overlite_core::{Column, SqlState, Value};
use rusqlite::{Connection, ffi};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// The table pg_cursors reads, one per connection: the session writes a row for each
/// cursor it holds.
const CURSORS_TABLE_DDL: &str = "CREATE TEMP TABLE IF NOT EXISTS pg_cursors (name TEXT, statement TEXT, \
     is_holdable BOOLEAN, is_binary BOOLEAN, is_scrollable BOOLEAN, creation_time TIMESTAMPTZ)";

/// Makes the connection's pg_cursors.
pub(crate) fn create_cursors_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(CURSORS_TABLE_DDL)
}

/// A prepared statement part way through its rows.
struct Open {
    /// Prepared on the session's connection, finalized only by Drop.
    p: *mut ffi::sqlite3_stmt,
    decls: Vec<String>,
    /// The first row, stepped to when the statement opened to learn its types.
    pending: Option<Vec<Value>>,
    done: bool,
}

// SAFETY: the statement is used only through the DbSession that owns the connection it
// was prepared on, which takes it along wherever it goes.
unsafe impl Send for Open {}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: p is live and this is the one call that finalizes it.
        unsafe { ffi::sqlite3_finalize(self.p) };
    }
}

/// The open queries of a session, by the number open_query gave each.
#[derive(Default)]
pub(crate) struct OpenQueries {
    open: RefCell<HashMap<u64, Open>>,
    next: Cell<u64>,
}

impl std::fmt::Debug for OpenQueries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OpenQueries({})", self.open.borrow().len())
    }
}

impl OpenQueries {
    /// Finalizes every statement, before the connection closes.
    pub(crate) fn clear(&self) {
        self.open.borrow_mut().clear();
    }
}

impl DbSession {
    /// Prepares and binds a query to be read a few rows at a time, through the passes
    /// `execute` runs. Its columns are typed as `execute` types them, from the first
    /// row, which is stepped to now and kept.
    ///
    /// # Errors
    ///
    /// Whatever SQLite reports for the statement, or 42601 for more than one.
    pub fn open_query(&self, sql: &str, args: &[Value], sp: Option<&[String]>) -> Result<(u64, Vec<Column>)> {
        self.sync_schemas()?;
        // A statement held open keeps its read snapshot, which a write after it could
        // not move past: a cursor's transaction takes the write lock up front.
        self.begin_writing(false)?;
        let (st, q) = (&self.state, &self.conn);
        let (query, typed) = crate::resolve::split_typed(sql);
        let query = crate::sqlite::resolve_names(sp, st, q, query);
        let mut script = Script::new(q, &query);
        let Some(stmt) = script.next()? else {
            return Err(crate::error::errf!(SqlState::SYNTAX_ERROR, "a cursor needs a query"));
        };
        if script.next()?.is_some() {
            return Err(crate::error::errf!(
                SqlState::SYNTAX_ERROR,
                "cannot open a cursor on more than one statement"
            ));
        }
        stmt.bind(args)?;
        let (names, mut decls) = stmt.column_info();
        let hints = computed_types(&query, &decls);
        apply_strong_types(&mut decls, &hints);
        let row = stmt.step()?;
        let mut columns = make_columns(names, &decls, &hints, row.then_some(&stmt));
        crate::resolve::apply_result_types(&mut columns, typed.as_deref());
        let pending = row.then(|| (0..decls.len()).map(|i| cell(stmt.value(i), &decls[i])).collect());
        let open = Open {
            p: stmt.into_raw(),
            decls,
            pending,
            done: !row,
        };
        let id = self.cursors.next.get() + 1;
        self.cursors.next.set(id);
        self.cursors.open.borrow_mut().insert(id, open);
        Ok((id, columns))
    }

    /// Up to n more rows of an open query; fewer at its end.
    ///
    /// # Errors
    ///
    /// What SQLite reports stepping it.
    pub fn fetch_query(&self, id: u64, n: usize) -> Result<Vec<Vec<Value>>> {
        self.read_query(id, n, true).map(|(rows, _)| rows)
    }

    /// Steps past up to n rows of an open query without reading them; the number passed.
    ///
    /// # Errors
    ///
    /// What SQLite reports stepping it.
    pub fn skip_query(&self, id: u64, n: usize) -> Result<usize> {
        self.read_query(id, n, false).map(|(_, passed)| passed)
    }

    fn read_query(&self, id: u64, n: usize, keep: bool) -> Result<(Vec<Vec<Value>>, usize)> {
        let mut map = self.cursors.open.borrow_mut();
        let Some(open) = map.get_mut(&id) else {
            return Ok((Vec::new(), 0));
        };
        // SAFETY: open.p was prepared on this connection, and the borrow ends here.
        let stmt = unsafe { Stmt::borrow_raw(&self.conn, open.p) };
        let mut rows = Vec::new();
        let mut passed = 0;
        while passed < n {
            if let Some(row) = open.pending.take() {
                if keep {
                    rows.push(row);
                }
                passed += 1;
                continue;
            }
            if open.done {
                break;
            }
            if !stmt.step()? {
                open.done = true;
                break;
            }
            if keep {
                rows.push(
                    (0..open.decls.len())
                        .map(|i| cell(stmt.value(i), &open.decls[i]))
                        .collect(),
                );
            }
            passed += 1;
        }
        Ok((rows, passed))
    }

    /// Takes an open query back to before its first row.
    pub fn rewind_query(&self, id: u64) {
        if let Some(open) = self.cursors.open.borrow_mut().get_mut(&id) {
            // SAFETY: open.p is live; a reset keeps its bindings.
            unsafe { ffi::sqlite3_reset(open.p) };
            open.pending = None;
            open.done = false;
        }
    }

    /// Finalizes an open query.
    pub fn close_query(&self, id: u64) {
        self.cursors.open.borrow_mut().remove(&id);
    }
}

impl DbSession {
    /// Makes pg_cursors hold these rows: name, statement, is_holdable, is_binary,
    /// is_scrollable, creation_time.
    ///
    /// # Errors
    ///
    /// What SQLite reports writing them.
    pub fn list_cursors(&self, rows: &[Vec<Value>]) -> Result<()> {
        do_exec(&self.conn, "DELETE FROM temp.pg_cursors", &[], "")?;
        for row in rows {
            do_exec(
                &self.conn,
                "INSERT INTO temp.pg_cursors VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                row,
                "",
            )?;
        }
        Ok(())
    }
}
