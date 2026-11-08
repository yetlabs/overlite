//! The SQLite-backed engine, down to what a driver would do: `$N` binding, column
//! type reporting, text-to-time conversion and error text.

use crate::blank::{blank_literals, code_only, is_word};
use crate::catalog::enums::{ENUM_TYPES_TABLE, refresh_enum_names_from};
use crate::catalog::register_functions;
use crate::catalog::schema::{rebuild_catalog, setup_connection};
use crate::column_types::{TypeHint, apply_strong_types, computed_types};
use crate::db_state::{DbState, state_for};
use crate::error::{Error, ErrorKind, Result, errf};
use crate::resolve::booleans::resolve_booleans;
use crate::resolve::current_database::resolve_current_database;
use crate::resolve::current_schema::resolve_current_schema;
use crate::resolve::public_qualifier::strip_public_qualifier;
use crate::resolve::regclass::resolve_to_regclass;
use crate::resolve::schema_qualify::qualify_schema_names;
use crate::resolve::search_path::resolve_search_path;
use crate::resolve::time_zone::{resolve_time_zones, resolve_zone_values};
use crate::resolve::tsvector::resolve_tsvectors;
use crate::sql_funcs::rewrite::rewrite_sql_functions;
use crate::sql_funcs::try_function_ddl;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime};
use overlite_core::sync::MutexExt;
use overlite_core::text::{compile_or_panic, quoted, text_from_bytes, text_to_bytes, trim_start_chars};
use overlite_core::{Column, ResultSet, SqlState, Timestamp, Value};
pub use rusqlite::InterruptHandle;
use rusqlite::config::DbConfig;
use rusqlite::ffi;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use std::borrow::Cow;
use std::cell::Cell;
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Caps concurrent client sessions, mirroring PostgreSQL's max_connections.
const MAX_CONNECTIONS: usize = 100;

/// How long a new client waits for a free connection before being refused.
const CONN_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
struct ConnLimit {
    open: Mutex<usize>,
    freed: Condvar,
}

impl ConnLimit {
    fn acquire(self: &Arc<Self>) -> Option<ConnPermit> {
        let guard = self.open.locked();
        let (mut open, res) = self
            .freed
            .wait_timeout_while(guard, CONN_WAIT_TIMEOUT, |n| *n >= MAX_CONNECTIONS)
            .unwrap_or_else(PoisonError::into_inner);
        if res.timed_out() && *open >= MAX_CONNECTIONS {
            return None;
        }
        *open += 1;
        Some(ConnPermit { limit: self.clone() })
    }
}

#[derive(Debug)]
#[must_use = "dropping it frees the connection slot"]
struct ConnPermit {
    limit: Arc<ConnLimit>,
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        *self.limit.open.locked() -= 1;
        self.limit.freed.notify_one();
    }
}

/// An engine backed by a single SQLite database file (or ":memory:").
#[derive(Debug)]
pub struct Sqlite {
    /// Keeps a shared-cache in-memory database alive; only tests read it.
    #[cfg_attr(not(test), allow(dead_code))]
    conn: Mutex<DbSession>,
    main_path: String,
    state: Arc<DbState>,
    limit: Arc<ConnLimit>,
}

/// Opens one connection: busy_timeout, foreign_keys,
/// WAL, then the connection hook (schemas, catalog).
fn open_connection(path: &str) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_FULL_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let uri = if path == ":memory:" {
        "file::memory:?cache=shared".to_string()
    } else {
        // In a URI these three mean something, and a database's file may hold `%`.
        let path = path.replace('%', "%25").replace('?', "%3f").replace('#', "%23");
        format!("file:{path}")
    };
    let conn = Connection::open_with_flags(&uri, flags).map_err(sqlite_error)?;
    conn.execute_batch("pragma busy_timeout(30000)").map_err(sqlite_error)?;
    conn.execute_batch("pragma foreign_keys(on)").map_err(sqlite_error)?;
    if path != ":memory:" {
        // journal_mode returns a row; execute_batch steps through it.
        conn.execute_batch("pragma journal_mode(WAL)").map_err(sqlite_error)?;
    }
    register_functions(&conn)?;
    crate::builtins::collation::register(&conn).map_err(sqlite_error)?;
    setup_connection(&conn, path)?;
    // A double-quoted name is a name, as in Postgres, where SQLite would fall back to
    // a string for one that is not a column. A stored view or trigger may lean on that
    // fallback, so only what a client runs is held to it.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)
        .map_err(sqlite_error)?;
    Ok(conn)
}

impl Sqlite {
    /// Opens (creating if needed) a SQLite database at path.
    ///
    /// # Errors
    ///
    /// When SQLite cannot open it or the emulated catalog cannot be set up.
    pub fn open(path: &str) -> Result<Arc<Sqlite>> {
        // Before any connection reads it: the readings of a column depend on its form.
        crate::storage::ensure_storage_format(path).map_err(|e| crate::storage::conversion_error(path, &e))?;
        let state = state_for(path);
        let limit = Arc::new(ConnLimit::default());
        let permit = limit.acquire().ok_or_else(|| {
            errf!(
                SqlState::IO_ERROR,
                "open sqlite {path:?}: no connection",
                path = quoted(path)
            )
        })?;
        let conn = open_connection(path)
            .map_err(|e| errf!(SqlState::IO_ERROR, "open sqlite {path:?}: {e}", path = quoted(path)))?;
        let seen_schemas = Cell::new(state.schema_generation());
        let own = DbSession {
            conn,
            main_path: path.to_string(),
            state: state.clone(),
            seen_schemas,
            cursors: crate::cursor::OpenQueries::default(),
            pending_begin: Cell::new(false),
            immediate_fks: Cell::new(false),
            _permit: permit,
        };
        Ok(Arc::new(Sqlite {
            conn: Mutex::new(own),
            main_path: path.to_string(),
            state,
            limit,
        }))
    }

    /// Pins a dedicated connection for one client. Past MAX_CONNECTIONS it
    /// waits briefly, then refuses.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::TooManyClients`] past the limit, or SQLite's failure to open.
    pub fn session(&self) -> Result<DbSession> {
        let permit = self.limit.acquire().ok_or_else(|| {
            Error::new(
                ErrorKind::TooManyClients,
                format!("sorry, too many clients already (max {MAX_CONNECTIONS})"),
            )
        })?;
        let seen_schemas = Cell::new(self.state.schema_generation());
        let conn = open_connection(&self.main_path)?;
        crate::cursor::create_cursors_table(&conn).map_err(sqlite_error)?;
        Ok(DbSession {
            conn,
            main_path: self.main_path.clone(),
            state: self.state.clone(),
            seen_schemas,
            cursors: crate::cursor::OpenQueries::default(),
            pending_begin: Cell::new(false),
            immediate_fks: Cell::new(false),
            _permit: permit,
        })
    }

    /// Runs f with the engine's own session.
    #[cfg(test)]
    pub(crate) fn with_own_session<T>(&self, f: impl FnOnce(&DbSession) -> T) -> T {
        f(&self.conn.locked())
    }

    pub(crate) fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// One client's dedicated connection. Transactions, row-level security lookups
/// and schema management all live here: a transaction is a state of the
/// connection, so its statements run on the same handle.
#[derive(Debug)]
pub struct DbSession {
    pub(crate) conn: Connection,
    pub(crate) main_path: String,
    pub(crate) state: Arc<DbState>,
    /// The schema generation this connection's catalog views were built at.
    seen_schemas: Cell<u64>,
    /// The statements of its cursors, finalized before the connection closes.
    pub(crate) cursors: crate::cursor::OpenQueries,
    /// A transaction the client began that SQLite has not yet: it begins at the first
    /// statement that writes, taking the write lock then (begin_writing).
    pending_begin: Cell<bool>,
    /// SET CONSTRAINTS ALL IMMEDIATE holds in the open transaction: deferred.rs.
    pub(crate) immediate_fks: Cell<bool>,
    _permit: ConnPermit,
}

impl Drop for DbSession {
    fn drop(&mut self) {
        self.cursors.clear();
    }
}

impl DbSession {
    /// Rebuilds the catalog views if the set of schemas moved since they were built.
    pub(crate) fn sync_schemas(&self) -> Result<()> {
        let now = self.state.schema_generation();
        if self.seen_schemas.get() != now {
            self.seen_schemas.set(now);
            rebuild_catalog(&self.conn, &self.main_path)?;
        }
        Ok(())
    }

    /// Tells every other connection the set of schemas changed. To be called when the
    /// change is committed; this connection has its views already. After a rollback,
    /// `own` is false: its views hold what was undone.
    pub fn publish_schemas(&self, own: bool) {
        let generation = self.state.next_schema_generation();
        if own {
            self.seen_schemas.set(generation);
        }
    }

    /// Runs a statement with no search_path.
    ///
    /// # Errors
    ///
    /// Whatever SQLite reports for the statement.
    pub fn execute(&self, sql: &str, args: &[Value]) -> Result<ResultSet> {
        self.execute_sp(sql, args, None)
    }

    /// Runs a statement with `sp` as search_path.
    ///
    /// # Errors
    ///
    /// Whatever SQLite reports for the statement.
    pub fn execute_sp(&self, sql: &str, args: &[Value], sp: Option<&[String]>) -> Result<ResultSet> {
        self.sync_schemas()?;
        if let Some(rs) = self.set_constraints(sql) {
            return rs;
        }
        self.begin_for(sql)?;
        let rs = execute(sp, &self.state, &self.conn, sql, args);
        if rs.is_ok() {
            self.check_immediate()?;
        }
        // format_type() renders an enum from a registry loaded when the
        // connection opens; a type created later has to reach it.
        if rs.is_ok() && sql.contains(ENUM_TYPES_TABLE) {
            refresh_enum_names_from(&self.state, &self.conn);
        }
        rs
    }

    /// Runs a statement in the open transaction, without the enum refresh.
    ///
    /// # Errors
    ///
    /// Whatever SQLite reports for the statement.
    pub fn tx_execute(&self, sql: &str, args: &[Value], sp: Option<&[String]>) -> Result<ResultSet> {
        self.sync_schemas()?;
        if let Some(rs) = self.set_constraints(sql) {
            return rs;
        }
        self.begin_for(sql)?;
        let rs = execute(sp, &self.state, &self.conn, sql, args)?;
        self.check_immediate()?;
        Ok(rs)
    }

    /// A statement's output columns, without running it for its effects. `None` for a
    /// statement that produces no rows.
    ///
    /// # Errors
    ///
    /// Whatever SQLite reports for the statement.
    pub fn describe(&self, sql: &str, args: &[Value], sp: Option<&[String]>) -> Result<Option<Vec<Column>>> {
        self.sync_schemas()?;
        describe(sp, &self.state, &self.conn, sql, args)
    }

    /// Begins a transaction. SQLite's begins at the first statement that writes, which
    /// takes the write lock then, waiting for it as a Postgres writer waits for a row
    /// lock; the statements before it read what is committed, each when it runs, as
    /// Postgres' READ COMMITTED does. Begun at BEGIN, a transaction that only reads
    /// would hold every other writer off, and one begun deferred that read and then
    /// wrote would fail at once (SQLITE_BUSY_SNAPSHOT) when another had written since.
    ///
    /// # Errors
    ///
    /// When a transaction is already open.
    pub fn begin(&self) -> Result<()> {
        self.immediate_fks.set(false);
        if self.pending_begin.get() || !self.conn.is_autocommit() {
            return Err(Error::with_state(
                SqlState::ACTIVE_SQL_TRANSACTION,
                "there is already a transaction in progress",
            ));
        }
        self.pending_begin.set(true);
        Ok(())
    }

    /// Begins SQLite's transaction of a begun one that has not yet, taking the write
    /// lock: within busy_timeout, or at once when nowait. Ok(false) when nowait and the
    /// lock is held.
    ///
    /// # Errors
    ///
    /// 55P03 when the lock cannot be had within busy_timeout.
    pub fn begin_writing(&self, nowait: bool) -> Result<bool> {
        if !self.pending_begin.get() {
            return Ok(true);
        }
        if nowait {
            let _ = self.conn.busy_timeout(std::time::Duration::ZERO);
        }
        let r = self.conn.execute_batch("begin immediate");
        if nowait {
            let _ = self
                .conn
                .busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS));
        }
        match r {
            Ok(()) => {
                self.pending_begin.set(false);
                Ok(true)
            }
            Err(rusqlite::Error::SqliteFailure(f, _)) if f.code == rusqlite::ErrorCode::DatabaseBusy => {
                if nowait {
                    return Ok(false);
                }
                Err(Error::with_state(
                    SqlState::LOCK_NOT_AVAILABLE,
                    "canceling statement due to lock timeout",
                ))
            }
            Err(e) => Err(sqlite_error(e)),
        }
    }

    /// Whether the client is in a transaction, written to or not yet.
    pub(crate) fn in_transaction(&self) -> bool {
        self.pending_begin.get() || !self.conn.is_autocommit()
    }

    /// Begins SQLite's transaction before a statement that is not a read.
    fn begin_for(&self, sql: &str) -> Result<()> {
        if self.pending_begin.get() && !is_read_only(sql) {
            self.begin_writing(false)?;
        }
        Ok(())
    }

    /// Commits, rolling back if that fails.
    ///
    /// # Errors
    ///
    /// SQLite's failure to commit.
    pub fn commit(&self) -> Result<()> {
        self.immediate_fks.set(false);
        if self.pending_begin.replace(false) {
            return Ok(()); // nothing was written
        }
        // A deferred foreign key still broken fails the COMMIT, in Postgres' words.
        if let Some(e) = self.pending_violation()? {
            let _ = self.conn.execute_batch("rollback");
            return Err(e);
        }
        let r = self.conn.execute_batch("commit");
        if let Err(e) = r {
            if !self.conn.is_autocommit() {
                let _ = self.conn.execute_batch("rollback");
            }
            return Err(sqlite_error(e));
        }
        Ok(())
    }

    /// Rolls back.
    ///
    /// # Errors
    ///
    /// When no transaction is open.
    pub fn rollback(&self) -> Result<()> {
        self.immediate_fks.set(false);
        if self.pending_begin.replace(false) {
            return Ok(());
        }
        self.conn.execute_batch("rollback").map_err(sqlite_error)
    }

    pub fn interrupt_handle(&self) -> InterruptHandle {
        self.conn.get_interrupt_handle()
    }
}

/// How long a writer waits for the write lock.
const BUSY_TIMEOUT_MS: u64 = 30_000;

/// Whether a statement only reads: a SELECT, VALUES, a WITH with no write in it, or
/// EXPLAIN, by its words outside literals. Anything else may write.
pub(crate) fn is_read_only(sql: &str) -> bool {
    let code = code_only(sql);
    let code = code.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    let first: String = code
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase();
    match first.as_str() {
        "select" | "values" | "explain" => true,
        "with" => !code
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|w| ["insert", "update", "delete"].iter().any(|k| w.eq_ignore_ascii_case(k))),
        _ => false,
    }
}

thread_local! {
    /// The SQLSTATE of the function that just failed. SQLite carries only a function's
    /// text out of a statement: sqlite3_result_error overwrites the result code, and a
    /// function runs on the thread stepping it, so this is what gets the state across.
    static FUNCTION_STATE: Cell<Option<SqlState>> = const { Cell::new(None) };
}

/// Records why a SQLite function is about to report failure.
pub(crate) fn set_function_state(state: SqlState) {
    FUNCTION_STATE.set(Some(state));
}

/// Renders an error as "<errstr>: <errmsg> (<rc>)", where rc is the extended
/// result code. Callers and sql_state read this form.
pub(crate) fn sqlite_error(e: rusqlite::Error) -> Error {
    use rusqlite::Error as E;
    let (rc, msg) = match &e {
        E::SqliteFailure(f, m) => (f.extended_code, m.clone()),
        E::SqlInputError { error, msg, .. } => (error.extended_code, Some(msg.clone())),
        _ => return Error::with_state(SqlState::INTERNAL_ERROR, e.to_string()),
    };
    // A function of ours failed: its message is for the client as it stands.
    if let Some(state) = FUNCTION_STATE.take() {
        return Error::with_state(state, msg.unwrap_or_default());
    }
    // A CHECK refused the row: in Postgres' words, which name the constraint quoted.
    if rc == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        && let Some(name) = msg.as_deref().and_then(|m| m.strip_prefix("CHECK constraint failed: "))
    {
        return Error::with_state(
            SqlState::CHECK_VIOLATION,
            format!("new row violates check constraint \"{name}\""),
        );
    }
    // An EXCLUDE constraint's trigger refused the row (ddl/exclude.rs).
    if rc == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER
        && let Some(m) = msg
            .as_deref()
            .filter(|m| m.starts_with("conflicting key value violates exclusion constraint"))
    {
        return Error::with_state(SqlState::EXCLUSION_VIOLATION, m);
    }
    // SAFETY: sqlite3_errstr takes any code and returns a static NUL-terminated string.
    let s = unsafe { std::ffi::CStr::from_ptr(rusqlite::ffi::sqlite3_errstr(rc as c_int)) }
        .to_string_lossy()
        .into_owned();
    let busy = if rc == rusqlite::ffi::SQLITE_BUSY {
        " (SQLITE_BUSY)"
    } else {
        ""
    };
    match msg {
        Some(m) if m != s => Error::sqlite(rc, &m, format!("{s}: {m} ({rc}){busy}")),
        _ => Error::sqlite(rc, &s, format!("{s} ({rc}){busy}")),
    }
}

fn execute(sp: Option<&[String]>, st: &DbState, q: &Connection, query: &str, args: &[Value]) -> Result<ResultSet> {
    if let Some(rs) = try_function_ddl(q, query)? {
        return Ok(rs);
    }
    let query = qualify_schema_names(st, query);
    let query = resolve_search_path(sp, st, q, &query);
    let query = strip_public_qualifier(&query);
    let query = resolve_current_schema(sp, st, &query);
    let query = resolve_current_database(st, &query);
    let query = resolve_to_regclass(&query);
    let query = rewrite_sql_functions(&query);
    let query = resolve_time_zones(q, &query);
    let query = resolve_zone_values(q, &query, args);
    let query = resolve_tsvectors(q, &query);
    let query = crate::resolve::temporal::resolve_temporal(q, &query);
    let query = resolve_booleans(st, q, &query);
    let cmd = leading_command(&query);
    if is_query(&query) {
        return do_query(q, &query, args, &cmd).map_err(|e| missing_from_entry(e, &query));
    }
    let rs = do_exec(q, &query, args, &cmd).map_err(|e| missing_from_entry(e, &query));
    if ["CREATE", "ALTER", "DROP"].iter().any(|c| cmd.starts_with(c)) {
        st.forget_bool_columns();
        crate::resolve::time_zone::DDL_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
    rs
}

/// `no such column: a.b` is 42P01 when nothing in the statement is called a: Postgres
/// tells a missing column from a missing FROM entry, and SQLite does not. Otherwise
/// it is 42703 in Postgres' words, `column a.b does not exist`.
fn missing_from_entry(e: Error, query: &str) -> Error {
    let ErrorKind::Sqlite { state, .. } = e.kind() else {
        return e;
    };
    if state != SqlState::UNDEFINED_COLUMN {
        return e;
    }
    let Some((_, rest)) = e.message().split_once("no such column: ") else {
        return e;
    };
    // SQLite may add " - should this be a string literal..." and the code.
    let missing = rest.split([' ', '\t', '\n']).next().unwrap_or(rest);
    let Some((table, column)) = missing.split_once('.') else {
        return Error::with_state(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{}\" does not exist", missing.trim_matches('"')),
        );
    };
    let table = table.trim_matches('"');
    let column = column.trim_matches('"');
    // Quoted names stay: `"t"."nope"` names t as much as `t.nope` does.
    let code = blank_literals(query).to_ascii_lowercase();
    let lower = table.to_ascii_lowercase();
    let b = code.as_bytes();
    let named = code.match_indices(&lower).any(|(at, _)| {
        let quoted = at > 0 && b[at - 1] == b'"';
        let before = at == 0 || !is_word(b[at - 1]);
        let mut end = at + lower.len();
        if quoted {
            if b.get(end) != Some(&b'"') {
                return false;
            }
            end += 1;
        }
        let next = b.get(end).copied().unwrap_or(b' ');
        before && !is_word(next) && next != b'.'
    });
    if named {
        return Error::with_state(
            SqlState::UNDEFINED_COLUMN,
            format!("column {table}.{column} does not exist"),
        );
    }
    Error::with_state(
        SqlState::UNDEFINED_TABLE,
        format!("missing FROM-clause entry for table \"{lower}\""),
    )
}

/// A prepared statement, driven through the C API because SQL, column names
/// and declared types may hold bytes that are not UTF-8: rusqlite takes the
/// first as &str and panics on the others.
///
/// `p` is a statement `sqlite3_prepare_v3` returned on `db`, finalized only by
/// `Drop`, and `_conn` keeps `db` open and off other threads for as long.
pub(crate) struct Stmt<'c> {
    p: *mut ffi::sqlite3_stmt,
    db: *mut ffi::sqlite3,
    _conn: &'c Connection,
}

impl Drop for Stmt<'_> {
    fn drop(&mut self) {
        // SAFETY: p is live (see Stmt) and this is the one call that finalizes it.
        unsafe { ffi::sqlite3_finalize(self.p) };
    }
}

/// Copies a string SQLite returned. `p` is null or NUL-terminated, as all of SQLite's
/// are, and is read before the next call on the connection can invalidate it.
fn c_text(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: see above.
    let c = unsafe { std::ffi::CStr::from_ptr(p) };
    Some(text_from_bytes(c.to_bytes()).into_owned())
}

/// The connection's current error, for a result code that isn't success.
fn db_error(db: *mut ffi::sqlite3, rc: c_int) -> Error {
    // SAFETY: db is an open connection, as every Stmt and Script holds one.
    let msg = c_text(unsafe { ffi::sqlite3_errmsg(db) });
    sqlite_error(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), msg))
}

/// Iterates the statements of a script, like rusqlite's Batch.
pub(crate) struct Script<'c, 's> {
    conn: &'c Connection,
    sql: Cow<'s, [u8]>,
    tail: usize,
}

impl<'c, 's> Script<'c, 's> {
    pub(crate) fn new(conn: &'c Connection, sql: &'s str) -> Self {
        Script {
            conn,
            sql: text_to_bytes(sql),
            tail: 0,
        }
    }

    pub(crate) fn next(&mut self) -> Result<Option<Stmt<'c>>> {
        // SAFETY: the handle only goes to the C API, and conn outlives what is prepared.
        let db = unsafe { self.conn.handle() };
        while self.tail < self.sql.len() {
            let rest = &self.sql[self.tail..];
            let Ok(len) = c_int::try_from(rest.len()) else {
                return Err(db_error(db, ffi::SQLITE_TOOBIG));
            };
            let c_sql = rest.as_ptr().cast::<c_char>();
            let (mut p, mut c_tail) = (std::ptr::null_mut(), std::ptr::null());
            // SAFETY: c_sql and len describe rest, which outlives the call, and the
            // out-pointers are locals.
            let rc = unsafe { ffi::sqlite3_prepare_v3(db, c_sql, len, 0, &raw mut p, &raw mut c_tail) };
            if rc != ffi::SQLITE_OK {
                return Err(db_error(db, rc));
            }
            let used = if c_tail.is_null() {
                0
            } else {
                // SAFETY: SQLite sets c_tail to a position inside rest.
                (unsafe { c_tail.offset_from(c_sql) }) as usize
            };
            self.tail = if used == 0 || used >= rest.len() {
                self.sql.len()
            } else {
                self.tail + used
            };
            if !p.is_null() {
                return Ok(Some(Stmt {
                    p,
                    db,
                    _conn: self.conn,
                }));
            }
        }
        Ok(None)
    }
}

impl<'c> Stmt<'c> {
    fn check(&self, rc: c_int) -> Result<()> {
        if rc == ffi::SQLITE_OK {
            return Ok(());
        }
        Err(db_error(self.db, rc))
    }

    /// Hands the statement to whoever finalizes it from now on.
    pub(crate) fn into_raw(self) -> *mut ffi::sqlite3_stmt {
        let p = self.p;
        std::mem::forget(self);
        p
    }

    /// A statement of `into_raw`'s, to step and read, and not to finalize.
    ///
    /// # Safety
    ///
    /// `p` was prepared on `conn` and is not finalized while this lives.
    pub(crate) unsafe fn borrow_raw(conn: &'c Connection, p: *mut ffi::sqlite3_stmt) -> std::mem::ManuallyDrop<Self> {
        // SAFETY: the handle only goes to the C API, and conn is open.
        let db = unsafe { conn.handle() };
        std::mem::ManuallyDrop::new(Stmt { p, db, _conn: conn })
    }

    /// Binds: `$N`/`?N` match the N-th argument, a bare `?` its position.
    pub(crate) fn bind(&self, args: &[Value]) -> Result<()> {
        // SAFETY: p is live (see Stmt).
        let n = unsafe { ffi::sqlite3_bind_parameter_count(self.p) } as usize;
        for i in 1..=n {
            // SAFETY: p is live, and i is in the range SQLite just gave.
            let name = unsafe { ffi::sqlite3_bind_parameter_name(self.p, i as c_int) };
            let idx = match c_text(name) {
                None => {
                    if i > args.len() {
                        return Err(errf!(SqlState::UNDEFINED_PARAMETER, "missing argument with index {i}"));
                    }
                    i
                }
                Some(name) => {
                    let tail = &name[1..];
                    let ord = if name.starts_with('?') || name.starts_with('$') {
                        tail.parse::<usize>().ok().filter(|o| *o >= 1 && *o <= args.len())
                    } else {
                        None
                    };
                    match ord {
                        Some(o) => o,
                        None => {
                            return Err(errf!(
                                SqlState::UNDEFINED_PARAMETER,
                                "missing named argument {tail:?}",
                                tail = quoted(tail)
                            ));
                        }
                    }
                }
            };
            self.bind_value(i as c_int, &args[idx - 1])?;
        }
        Ok(())
    }

    fn bind_text(&self, i: c_int, b: &[u8]) -> Result<()> {
        let (c, len) = (b.as_ptr().cast::<c_char>(), b.len() as u64);
        let utf8 = ffi::SQLITE_UTF8 as std::os::raw::c_uchar;
        // SAFETY: p is live, and SQLITE_TRANSIENT makes SQLite copy c..c+len before returning.
        self.check(unsafe { ffi::sqlite3_bind_text64(self.p, i, c, len, ffi::SQLITE_TRANSIENT(), utf8) })
    }

    fn bind_value(&self, i: c_int, v: &Value) -> Result<()> {
        let p = self.p;
        // SAFETY: p is live (see Stmt), and SQLITE_TRANSIENT makes SQLite copy a blob
        // before the call returns. An out-of-range i is an error code, not undefined.
        let rc = unsafe {
            match v {
                Value::Null => ffi::sqlite3_bind_null(p, i),
                Value::Int(v) => ffi::sqlite3_bind_int64(p, i, *v),
                Value::Float(v) => ffi::sqlite3_bind_double(p, i, *v),
                Value::Bool(v) => ffi::sqlite3_bind_int64(p, i, i64::from(*v)),
                Value::Blob(v) if v.is_empty() => ffi::sqlite3_bind_zeroblob(p, i, 0),
                Value::Blob(v) => {
                    ffi::sqlite3_bind_blob64(p, i, v.as_ptr().cast(), v.len() as u64, ffi::SQLITE_TRANSIENT())
                }
                Value::Text(v) => return self.bind_text(i, &text_to_bytes(v)),
                Value::Time(t) => return self.bind_text(i, format_bound_time(t).as_bytes()),
            }
        };
        self.check(rc)
    }

    /// Moves to the next row; false at the end of the results.
    pub(crate) fn step(&self) -> Result<bool> {
        // SAFETY: p is live (see Stmt).
        match unsafe { ffi::sqlite3_step(self.p) } {
            ffi::SQLITE_ROW => Ok(true),
            ffi::SQLITE_DONE => Ok(false),
            rc => Err(db_error(self.db, rc)),
        }
    }

    /// Column i of the current row.
    pub(crate) fn value(&self, i: usize) -> ValueRef<'_> {
        let (p, i) = (self.p, i as c_int);
        // SAFETY: p is live and on a row, as step said, and i is one of its columns. A
        // slice is valid until the next step, and its borrow of self keeps it from
        // outliving the finalize. Both callers copy it before they step again.
        unsafe {
            match ffi::sqlite3_column_type(p, i) {
                ffi::SQLITE_INTEGER => ValueRef::Integer(ffi::sqlite3_column_int64(p, i)),
                ffi::SQLITE_FLOAT => ValueRef::Real(ffi::sqlite3_column_double(p, i)),
                ffi::SQLITE_TEXT => {
                    let c = ffi::sqlite3_column_text(p, i);
                    let n = ffi::sqlite3_column_bytes(p, i) as usize;
                    ValueRef::Text(if c.is_null() {
                        &[]
                    } else {
                        std::slice::from_raw_parts(c, n)
                    })
                }
                ffi::SQLITE_BLOB => {
                    let c = ffi::sqlite3_column_blob(p, i);
                    let n = ffi::sqlite3_column_bytes(p, i) as usize;
                    ValueRef::Blob(if c.is_null() || n == 0 {
                        &[]
                    } else {
                        std::slice::from_raw_parts(c.cast(), n)
                    })
                }
                _ => ValueRef::Null,
            }
        }
    }

    pub(crate) fn column_info(&self) -> (Vec<String>, Vec<String>) {
        // SAFETY: p is live (see Stmt).
        let n = unsafe { ffi::sqlite3_column_count(self.p) };
        let names = (0..n)
            .map(|i| {
                // SAFETY: p is live, and i is below the column count.
                let name = unsafe { ffi::sqlite3_column_name(self.p, i) };
                c_text(name).unwrap_or_default()
            })
            .collect();
        let decls = (0..n)
            .map(|i| {
                // SAFETY: p is live, and i is below the column count.
                let decl = unsafe { ffi::sqlite3_column_decltype(self.p, i) };
                c_text(decl).unwrap_or_default().to_ascii_uppercase()
            })
            .collect();
        (names, decls)
    }
}

/// The form a timestamp is written in, minus the zone name.
fn format_bound_time(t: &Timestamp) -> String {
    t.format("%Y-%m-%d %H:%M:%S%.f %z").to_string()
}

fn is_time_decl(d: &str) -> bool {
    matches!(d, "DATE" | "DATETIME" | "TIMESTAMP")
}

/// The type name of a computed column, from its first row. Named as Postgres names
/// them, since the protocol reads a type name by substring (see oid_from_decl_type):
/// bytes once went by a name holding INT, and were advertised as int8.
fn scan_type_name(v: Option<ValueRef<'_>>) -> &'static str {
    match v {
        Some(ValueRef::Integer(_)) => "int8",
        Some(ValueRef::Real(_)) => "float8",
        Some(ValueRef::Text(_)) => "text",
        Some(ValueRef::Blob(_)) => "bytea",
        _ => "",
    }
}

fn parse_digits(b: &[u8]) -> Option<u32> {
    let mut n = 0u32;
    if b.is_empty() {
        return None;
    }
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n * 10 + (c - b'0') as u32;
    }
    Some(n)
}

/// Parses the "<date>[ T]<time>[.frac][+-hh:mm]" shapes a time column may hold.
fn parse_time_parts(s: &str) -> Option<Timestamp> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(
        parse_digits(&b[..4])? as i32,
        parse_digits(&b[5..7])?,
        parse_digits(&b[8..10])?,
    )?;
    let utc = FixedOffset::east_opt(0)?;
    if b.len() == 10 {
        return date.and_hms_opt(0, 0, 0)?.and_local_timezone(utc).single();
    }
    if b[10] != b' ' && b[10] != b'T' {
        return None;
    }
    if b.len() < 16 || b[13] != b':' {
        return None;
    }
    let (h, m) = (parse_digits(&b[11..13])?, parse_digits(&b[14..16])?);
    if b.len() == 16 {
        return date.and_hms_opt(h, m, 0)?.and_local_timezone(utc).single();
    }
    if b.len() < 19 || b[16] != b':' {
        return None;
    }
    let sec = parse_digits(&b[17..19])?;
    let mut i = 19;
    let mut nanos = 0u32;
    if i < b.len() && b[i] == b'.' {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &b[start..i];
        if digits.is_empty() || digits.len() > 9 {
            return None;
        }
        nanos = parse_digits(digits)? * 10u32.pow(9 - digits.len() as u32);
    }
    let dt: NaiveDateTime = date.and_hms_nano_opt(h, m, sec, nanos)?;
    if i == b.len() {
        return dt.and_local_timezone(utc).single();
    }
    // -07:00
    if b.len() - i != 6 || (b[i] != b'+' && b[i] != b'-') || b[i + 3] != b':' {
        return None;
    }
    let (oh, om) = (parse_digits(&b[i + 1..i + 3])?, parse_digits(&b[i + 4..i + 6])?);
    let mut off = (oh * 3600 + om * 60) as i32;
    if b[i] == b'-' {
        off = -off;
    }
    dt.and_local_timezone(FixedOffset::east_opt(off)?).single()
}

/// Value::Time on success, else the text unchanged.
fn parse_driver_time(s: &str) -> Value {
    // Also the form with zone and monotonic reading: "2006-01-02 15:04:05.999999999 -0700 MST [m=+...]".
    if s.len() > 25 && s.as_bytes()[19..].contains(&b' ') {
        let cut = s.find("m=").map_or(s, |x| &s[..x]).trim();
        let mut parts = cut.rsplitn(2, ' ');
        if let (Some(_zone), Some(rest)) = (parts.next(), parts.next())
            && let Ok(t) = DateTime::parse_from_str(rest, "%Y-%m-%d %H:%M:%S%.f %z")
        {
            return Value::Time(t);
        }
    }
    let ts = s.strip_suffix('Z').unwrap_or(s);
    match parse_time_parts(ts) {
        Some(t) => Value::Time(t),
        None => Value::Text(s.to_string()),
    }
}

#[inline]
pub(crate) fn cell(v: ValueRef<'_>, decl: &str) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(b) => {
            let text = text_from_bytes(b);
            if is_time_decl(decl) {
                parse_driver_time(&text)
            } else {
                Value::Text(text.into_owned())
            }
        }
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

pub(crate) fn make_columns(
    names: Vec<String>,
    decls: &[String],
    hints: &[TypeHint],
    first: Option<&Stmt<'_>>,
) -> Vec<Column> {
    names
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            let decl_type = if decls[i].is_empty() {
                match first {
                    Some(row) => scan_type_name(Some(row.value(i))).to_string(),
                    None => hints.get(i).map_or("", TypeHint::name).to_string(),
                }
            } else {
                decls[i].clone()
            };
            Column { name, decl_type }
        })
        .collect()
}

pub(crate) fn do_query(q: &Connection, query: &str, args: &[Value], cmd: &str) -> Result<ResultSet> {
    let mut batch = Script::new(q, query);
    let mut out: Option<ResultSet> = None;
    // A script runs statement by statement; the last one with columns answers.
    while let Some(stmt) = batch.next()? {
        stmt.bind(args)?;
        let (names, mut decls) = stmt.column_info();
        let hints = computed_types(query, &decls);
        apply_strong_types(&mut decls, &hints);
        let n = names.len();
        let mut row = stmt.step()?;
        let columns = make_columns(names, &decls, &hints, row.then_some(&stmt));
        let mut data: Vec<Vec<Value>> = Vec::new();
        while row {
            let mut cells = Vec::with_capacity(n);
            for (i, decl) in decls.iter().enumerate() {
                cells.push(cell(stmt.value(i), decl));
            }
            data.push(cells);
            row = stmt.step()?;
        }
        if n > 0 || out.is_none() {
            let count = data.len() as i64;
            out = Some(ResultSet {
                is_query: true,
                columns,
                rows: data,
                rows_affected: count,
                last_insert_id: 0,
                command: cmd.into(),
            });
        }
    }
    Ok(out.unwrap_or(ResultSet {
        is_query: true,
        command: cmd.into(),
        ..Default::default()
    }))
}

pub(crate) fn do_exec(q: &Connection, query: &str, args: &[Value], cmd: &str) -> Result<ResultSet> {
    let mut batch = Script::new(q, query);
    while let Some(stmt) = batch.next()? {
        stmt.bind(args)?;
        while stmt.step()? {}
    }
    Ok(ResultSet {
        is_query: false,
        rows_affected: q.changes() as i64,
        last_insert_id: q.last_insert_rowid(),
        command: cmd.into(),
        ..Default::default()
    })
}

/// Returns a query's output columns without side effects, `None` for a
/// statement that produces no rows.
fn describe(
    sp: Option<&[String]>,
    st: &DbState,
    q: &Connection,
    query: &str,
    args: &[Value],
) -> Result<Option<Vec<Column>>> {
    let query = qualify_schema_names(st, query);
    let query = resolve_search_path(sp, st, q, &query);
    let query = strip_public_qualifier(&query);
    let query = resolve_current_schema(sp, st, &query);
    let query = resolve_current_database(st, &query);
    let query = resolve_to_regclass(&query);
    let query = rewrite_sql_functions(&query);
    let query = resolve_time_zones(q, &query);
    let query = resolve_zone_values(q, &query, args);
    let query = resolve_tsvectors(q, &query);
    let query = crate::resolve::temporal::resolve_temporal(q, &query);
    let query = resolve_booleans(st, q, &query);
    let Some(intro) = introspection_sql(&query) else {
        return Ok(None); // statement produces no rows
    };
    // A RETURNING rewrite selects nothing (WHERE 0), so its placeholders, any a
    // RETURNING item still has, stay unbound: NULL, which is all they need to be.
    let rewritten = intro != query;
    let args: &[Value] = if rewritten { &[] } else { args };

    let mut batch = Script::new(q, &intro);
    let Some(stmt) = batch.next().map_err(|e| missing_from_entry(e, &intro))? else {
        return Ok(Some(Vec::new()));
    };
    if !rewritten {
        stmt.bind(args)?;
    }
    let (names, mut decls) = stmt.column_info();
    let hints = computed_types(&intro, &decls);
    apply_strong_types(&mut decls, &hints);
    // A row is only a sample of the types. One that fails was asked for with probe
    // values, or has an error of its own to report at Execute. And a set built from a
    // parameter holds what the probe made of it, which says nothing of the real one.
    let sampled = args.is_empty() || !intro.contains("overlite_elements");
    let row = sampled && stmt.step().unwrap_or(false);
    Ok(Some(make_columns(names, &decls, &hints, row.then_some(&stmt))))
}

/// Whether a statement is expected to return rows.
fn is_query(sql: &str) -> bool {
    match first_word(sql).as_str() {
        "SELECT" | "WITH" | "PRAGMA" | "EXPLAIN" | "VALUES" | "SHOW" => return true,
        _ => {}
    }
    // INSERT/UPDATE/DELETE ... RETURNING also yields rows.
    code_only(sql)
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| w.eq_ignore_ascii_case("returning"))
}

/// The command tag verb, upper-cased. DDL verbs keep their second word.
fn leading_command(sql: &str) -> String {
    let up = sql.trim().to_uppercase();
    let fields: Vec<&str> = up.split_whitespace().collect();
    if fields.is_empty() {
        return String::new();
    }
    match fields[0] {
        "CREATE" | "DROP" | "ALTER" if fields.len() > 1 => format!("{} {}", fields[0], fields[1]),
        _ => fields[0].to_string(),
    }
}

const RE_TABLE_NAME: &str = r#"((?:"[^"]*"|\w+)(?:\s*\.\s*(?:"[^"]*"|\w+))*)"#;

static RE_INSERT_RETURNING: std::sync::LazyLock<::regex::Regex> = std::sync::LazyLock::new(|| {
    compile_or_panic(&format!(
        r"(?is)^\s*INSERT\s+INTO\s+{RE_TABLE_NAME}.*?\bRETURNING\b\s+(.+)$"
    ))
});
static RE_UPDATE_RETURNING: std::sync::LazyLock<::regex::Regex> = std::sync::LazyLock::new(|| {
    compile_or_panic(&format!(r"(?is)^\s*UPDATE\s+{RE_TABLE_NAME}.*?\bRETURNING\b\s+(.+)$"))
});
static RE_DELETE_RETURNING: std::sync::LazyLock<::regex::Regex> = std::sync::LazyLock::new(|| {
    compile_or_panic(&format!(
        r"(?is)^\s*DELETE\s+FROM\s+{RE_TABLE_NAME}.*?\bRETURNING\b\s+(.+)$"
    ))
});

/// A read-only statement whose columns match query's output, `None` when the
/// statement yields no rows at all.
fn introspection_sql(sql: &str) -> Option<String> {
    match first_word(sql).as_str() {
        "SELECT" | "WITH" | "VALUES" | "PRAGMA" | "EXPLAIN" => return Some(sql.to_string()),
        _ => {}
    }
    let code = blank_literals(sql);
    for re in [&*RE_INSERT_RETURNING, &*RE_UPDATE_RETURNING, &*RE_DELETE_RETURNING] {
        if let Some(m) = re.captures(&code)
            && let (Some(list), Some(table)) = (m.get(2), m.get(1))
        {
            return Some(format!(
                "SELECT {} FROM {} WHERE 0",
                &sql[list.range()],
                &sql[table.range()]
            ));
        }
    }
    None
}

fn first_word(sql: &str) -> String {
    let sql = trim_start_chars(sql, " \t\r\n(");
    let end = sql.find([' ', '\t', '\r', '\n', '(']).unwrap_or(sql.len());
    sql[..end].to_uppercase()
}

// Also the home of what the engine's tests share (new_test_engine, must_exec,
// text_or_empty), which is why the module is pub(crate).
#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use overlite_core::Command;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    /// A fresh directory, removed at the end of the test.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> TempDir {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("overlite-rs-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        pub(crate) fn join(&self, file: &str) -> String {
            self.0.join(file).to_str().unwrap().to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fresh on-disk engine, so tests exercise the same WAL path
    /// production uses. The directory has to outlive the engine.
    fn new_test_engine() -> (TempDir, Arc<Sqlite>) {
        let dir = TempDir::new();
        let eng = Sqlite::open(&dir.join("test.db")).unwrap();
        (dir, eng)
    }

    /// Runs on the engine's own connection.
    pub(crate) fn exec(eng: &Sqlite, sql: &str, args: &[Value]) -> Result<ResultSet> {
        eng.with_own_session(|s| s.execute(sql, args))
    }

    pub(crate) fn must_exec(eng: &Sqlite, sql: &str) -> ResultSet {
        exec(eng, sql, &[]).unwrap_or_else(|e| panic!("exec {sql:?}: {e}"))
    }

    /// Normalizes a TEXT cell (string or blob).
    pub(crate) fn text_or_empty(v: &Value) -> String {
        match v {
            Value::Text(s) => s.clone(),
            Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
            _ => String::new(),
        }
    }

    #[test]
    fn exec_create_insert_select() {
        let (_dir, eng) = new_test_engine();

        exec(
            &eng,
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
            &[],
        )
        .unwrap();

        let ins = exec(
            &eng,
            "INSERT INTO users (name, age) VALUES (?, ?)",
            &["alice".into(), 30i64.into()],
        )
        .unwrap();
        assert!(!ins.is_query, "insert is not a query");
        assert_eq!(ins.rows_affected, 1);
        assert_eq!(ins.last_insert_id, 1);
        assert_eq!(ins.command, Command::Insert);

        let sel = exec(&eng, "SELECT id, name, age FROM users", &[]).unwrap();
        assert!(sel.is_query, "select is a query");
        assert_eq!(sel.columns.len(), 3);
        assert_eq!(sel.columns[1].name, "name");

        assert_eq!(sel.rows.len(), 1);
        let row = &sel.rows[0];
        assert_eq!(row[0], Value::Int(1));
        assert_eq!(text_or_empty(&row[1]), "alice");
        assert_eq!(row[2], Value::Int(30));
    }

    #[test]
    fn exec_args_and_affected() {
        let (_dir, eng) = new_test_engine();

        must_exec(&eng, "CREATE TABLE t (n INTEGER)");
        for i in 1..=3i64 {
            exec(&eng, "INSERT INTO t (n) VALUES (?)", &[i.into()]).unwrap();
        }

        let upd = exec(&eng, "UPDATE t SET n = n + 10 WHERE n >= ?", &[2i64.into()]).unwrap();
        assert_eq!(upd.rows_affected, 2);
        assert_eq!(upd.command, Command::Update);
    }

    #[test]
    fn null_round_trip() {
        let (_dir, eng) = new_test_engine();

        must_exec(&eng, "CREATE TABLE t (a TEXT, b INTEGER)");
        must_exec(&eng, "INSERT INTO t (a, b) VALUES (NULL, NULL)");

        let sel = exec(&eng, "SELECT a, b FROM t", &[]).unwrap();
        assert_eq!(sel.rows[0][0], Value::Null);
        assert_eq!(sel.rows[0][1], Value::Null);
    }

    #[test]
    fn is_query_heuristic() {
        for (sql, want) in [
            ("SELECT 1", true),
            ("  select * from t", true),
            ("WITH x AS (SELECT 1) SELECT * FROM x", true),
            ("PRAGMA table_info(t)", true),
            ("INSERT INTO t VALUES (1)", false),
            ("UPDATE t SET a = 1", false),
            ("DELETE FROM t", false),
            ("CREATE TABLE t (a INT)", false),
            ("INSERT INTO t VALUES (1) RETURNING a", true),
        ] {
            assert_eq!(is_query(sql), want, "is_query({sql:?})");
        }
    }

    #[test]
    fn leading_command_cases() {
        for (sql, want) in [
            ("select 1", "SELECT"),
            ("insert into t values (1)", "INSERT"),
            ("create table t (a int)", "CREATE TABLE"),
            ("drop table t", "DROP TABLE"),
        ] {
            assert_eq!(leading_command(sql), want, "leading_command({sql:?})");
        }
    }

    // --- transactions ---

    #[test]
    fn transaction_commit_and_rollback() {
        let (_dir, eng) = new_test_engine();
        must_exec(&eng, "CREATE TABLE t (n INTEGER)");

        let count = || must_exec(&eng, "SELECT count(*) FROM t").rows[0][0].clone();

        // Commit persists.
        eng.with_own_session(|tx| {
            tx.begin().unwrap();
            tx.tx_execute("INSERT INTO t (n) VALUES (1)", &[], None).unwrap();
            tx.commit().unwrap();
        });
        assert_eq!(count(), Value::Int(1));

        // Rollback reverts.
        eng.with_own_session(|tx| {
            tx.begin().unwrap();
            tx.tx_execute("INSERT INTO t (n) VALUES (2)", &[], None).unwrap();
            tx.rollback().unwrap();
        });
        assert_eq!(count(), Value::Int(1)); // the second insert is gone
    }

    #[test]
    fn transaction_isolated_until_commit() {
        let (_dir, eng) = new_test_engine();
        must_exec(&eng, "CREATE TABLE t (n INTEGER)");

        eng.with_own_session(|tx| {
            tx.begin().unwrap();
            tx.tx_execute("INSERT INTO t (n) VALUES (42)", &[], None).unwrap();

            // Within the transaction the row is visible.
            let rs = tx.tx_execute("SELECT n FROM t", &[], None).unwrap();
            assert_eq!(rs.rows.len(), 1);

            tx.rollback().unwrap();
        });
        let after = must_exec(&eng, "SELECT count(*) FROM t");
        assert_eq!(after.rows[0][0], Value::Int(0));
    }

    // Transactions that read and then write, several at once, as an ORM's do: each
    // waits its turn, as on Postgres. Begun deferred, two would both read, then
    // race to write; the one that lost failed at once with "database is locked"
    // (SQLITE_BUSY_SNAPSHOT), which no busy timeout waits out.
    #[test]
    fn concurrent_writing_transactions_wait_their_turn() {
        let (_dir, eng) = new_test_engine();
        must_exec(&eng, "CREATE TABLE t (n INTEGER)");

        const WRITERS: i64 = 8;
        let writer = |i: i64| -> Result<()> {
            let s = eng.session()?;
            s.begin()?;
            let body = || -> Result<()> {
                s.tx_execute("SELECT count(*) FROM t", &[], None)?;
                std::thread::sleep(Duration::from_millis(10));
                s.tx_execute("INSERT INTO t (n) VALUES ($1)", &[i.into()], None)?;
                s.commit()
            };
            let r = body();
            if r.is_err() {
                let _ = s.rollback();
            }
            r
        };
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..WRITERS).map(|i| scope.spawn(move || writer(i))).collect();
            for h in handles {
                h.join().unwrap().unwrap();
            }
        });
        let rs = must_exec(&eng, "SELECT count(*) FROM t");
        assert_eq!(rs.rows[0][0], Value::Int(WRITERS));
    }

    // --- concurrent open ---

    // A client must get in while another holds a write transaction, as Postgres
    // lets it. Each new connection used to write its bootstrap (the default role,
    // the internal tables' migrations) and so waited on the writer's lock, past
    // the session timeout: refused as "too many clients" with the server empty.
    #[test]
    fn session_opens_while_another_writes() {
        let dir = TempDir::new();
        let eng = Sqlite::open(&dir.join("busy.db")).unwrap();

        let writer = eng.session().unwrap();
        writer.execute("CREATE TABLE t (id INTEGER)", &[]).unwrap();
        writer.begin().unwrap();
        writer.tx_execute("INSERT INTO t VALUES (1)", &[], None).unwrap();

        let start = Instant::now();
        let reader = eng.session().expect("a client could not get in while another writes");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "getting in waited on the writer"
        );

        let rs = reader.execute("SELECT count(*) FROM t", &[]).unwrap();
        assert_eq!(rs.rows.len(), 1);

        writer.rollback().unwrap();
    }

    // --- connection limit ---

    // Past the connection limit a client must be told the server is full. Waiting
    // on a pool instead left it hanging with no answer at all, so the "too many
    // clients" path the protocol already had was unreachable.
    #[test]
    fn session_refuses_past_the_limit() {
        let dir = TempDir::new();
        let eng = Sqlite::open(&dir.join("full.db")).unwrap();

        // One connection is the engine's own, so the clients get the rest.
        let mut held = Vec::new();
        for i in 0..MAX_CONNECTIONS - 1 {
            held.push(eng.session().unwrap_or_else(|e| panic!("session {i}: {e}")));
        }

        let err = eng.session().expect_err("the session past the limit was not refused");
        assert!(
            err.message().contains("too many clients"),
            "the error should say the server is full, got: {err}"
        );

        // Freeing one lets the next client in.
        drop(held.remove(0));
        held.push(eng.session().unwrap());
    }

    // --- RETURNING ---

    // A RETURNING statement is introspected by rebuilding it as a read-only SELECT
    // against the same table. The table pattern has to survive every quoting
    // combination a client may send; capturing only the first quoted run turned
    // `"sales"."orders"` into the table `"sales"`, and the query failed with
    // `no such table: sales`.
    #[test]
    fn introspection_sql_table_names() {
        for (input, want) in [
            ("INSERT INTO t VALUES ($1) RETURNING id", "SELECT id FROM t WHERE 0"),
            (
                r#"INSERT INTO "t" VALUES ($1) RETURNING id"#,
                r#"SELECT id FROM "t" WHERE 0"#,
            ),
            (
                "INSERT INTO sales.orders VALUES ($1) RETURNING id",
                "SELECT id FROM sales.orders WHERE 0",
            ),
            (
                r#"INSERT INTO "sales"."orders" VALUES ($1) RETURNING id"#,
                r#"SELECT id FROM "sales"."orders" WHERE 0"#,
            ),
            (
                r#"INSERT INTO sales."orders" VALUES ($1) RETURNING id"#,
                r#"SELECT id FROM sales."orders" WHERE 0"#,
            ),
            (
                r#"INSERT INTO "sales".orders VALUES ($1) RETURNING id"#,
                r#"SELECT id FROM "sales".orders WHERE 0"#,
            ),
            (
                r#"UPDATE "sales"."orders" SET n = n + 1 WHERE id = $1 RETURNING n"#,
                r#"SELECT n FROM "sales"."orders" WHERE 0"#,
            ),
            (
                r#"DELETE FROM "sales"."orders" WHERE id = $1 RETURNING id, n"#,
                r#"SELECT id, n FROM "sales"."orders" WHERE 0"#,
            ),
        ] {
            assert_eq!(
                introspection_sql(input).as_deref(),
                Some(want),
                "introspection_sql({input:?})"
            );
        }
    }

    #[test]
    fn introspection_sql_non_returning() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "CREATE TABLE t (id int)",
            "UPDATE t SET n = 1",
        ] {
            assert_eq!(introspection_sql(sql), None, "introspection_sql({sql:?})");
        }
    }
}
