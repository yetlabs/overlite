//! Per-connection session and the Extended Query protocol.

mod advisory;
pub(crate) mod cancel;
pub(crate) mod copy;
pub(crate) mod cursor;
pub(crate) mod explain;
mod multi_statement;
mod notify;
mod prepare;
pub(crate) mod search_path;
pub(crate) mod time_zone;
pub(crate) mod utility;

use crate::auth::grants::is_grant;
use crate::auth::rls::is_rls_ddl;
use crate::auth::roles::is_role_ddl;
use crate::auth::session_user::is_set_role;
use crate::ddl::alter::alter_table_routed;
use crate::ddl::comment::is_comment;
use crate::ddl::database::{database_generation, is_database_ddl};
use crate::ddl::enums::is_type_ddl;
use crate::ddl::exclude::creates_exclusions;
use crate::ddl::sequences::{has_seq_func, is_sequence_ddl, neutralize_sequences};
use crate::error::{Result, errf};
use crate::rewrite::blank::{is_blank_statement, statement_text};
use crate::rewrite::rewrite;
use crate::session::advisory::{TxAdvisoryHold, has_advisory_func, neutralize_advisory_locks, release_advisory_locks};
use crate::session::cancel::Canceler;
use crate::session::notify::{NotifyState, is_listen_notify, unregister_all};
use crate::session::search_path::{SearchPathOutcome, is_search_path_stmt};
use crate::session::utility::{TxControl, first_word_upper, intercept_utility, is_schema_ddl, log_query_error};
use crate::types::Format;
use crate::types::param_decode::{bytea_hex, decode_typed_param};
use crate::types::{
    INT_CATALOG_COLUMNS, NoBinaryForm, OID_BYTEA, OID_INT8, OID_TEXT, encode_value, is_catalog_query, oid_for_column,
};
use crate::wire::{
    MSG_BIND, MSG_BIND_COMPLETE, MSG_CLOSE, MSG_CLOSE_COMPLETE, MSG_DESCRIBE, MSG_EMPTY_QUERY, MSG_EXECUTE, MSG_FLUSH,
    MSG_NO_DATA, MSG_PARSE, MSG_PARSE_COMPLETE, MSG_PORTAL_SUSPENDED, MSG_QUERY, MSG_READY_FOR_QUERY, MSG_SYNC,
    MSG_TERMINATE, WireConn,
};
use overlite_core::text::{text_from_bytes, trim_end_chars};
use overlite_core::{Column, Command, Oid, ResultSet, SqlState, Value};
use overlite_engine::{Cluster, DbSession, code_only};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// Where a client's transaction stands. BEGIN opens one, a failure aborts it, and
/// COMMIT or ROLLBACK ends it either way; ROLLBACK TO reopens an aborted one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TxState {
    #[default]
    Idle,
    Open,
    Failed,
}

/// Per-connection state.
#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) c: WireConn,
    /// This client's dedicated engine connection.
    pub(crate) db: DbSession,

    prepared: HashMap<String, Rc<RefCell<Prepared>>>,
    portals: HashMap<String, Rc<RefCell<Portal>>>,

    /// A message in an extended request cycle errored; the rest of the cycle is
    /// skipped until the next Sync.
    failed: bool,

    pub(crate) cluster: Arc<dyn Cluster>,
    pub(crate) database: String,

    /// The state of the client's transaction.
    pub(crate) tx: TxState,

    /// The TimeZone before a SET in the open transaction, which a rollback restores.
    pub(crate) zone_tx_restore: Option<String>,
    /// The TimeZone before a SET LOCAL, which the end of the transaction restores.
    pub(crate) zone_local_restore: Option<String>,

    /// An open transaction changed which tables have RLS.
    pub(crate) rls_dirty: bool,

    /// An open transaction changed which schemas there are.
    pub(crate) schemas_dirty: bool,

    /// The generation of the set of databases this session's pg_database shows.
    seen_databases: u64,

    /// The statement running goes on one a Query message split at its ALTER TABLE
    /// actions, which said what there was to say about the table.
    pub(crate) later_action: bool,

    pub(crate) seq_curr: HashMap<String, i64>,
    pub(crate) seq_last: String,

    canceler: Arc<Canceler>,

    /// SQL-level prepared statements (PREPARE name AS ...).
    sql_prepared: HashMap<String, String>,

    pub(crate) auth_user: String,
    pub(crate) session_user: String,
    pub(crate) current_role: String,

    /// Schema resolution order set by SET search_path (None = "public").
    pub(crate) search_path: Option<Vec<String>>,

    /// Whether a role skips privilege checks; cleared on role DDL.
    pub(crate) bypass_cache: HashMap<String, bool>,

    pid: i32,
    listens: HashSet<String>,
    notify: Arc<NotifyState>,

    /// Advisory locks taken for the current transaction.
    tx_advisory: Vec<TxAdvisoryHold>,

    /// DECLAREd cursors.
    pub(crate) cursors: cursor::Cursors,
}

/// How a prepared statement runs: most reach the engine, and the rest are the
/// session's own, each holding its statement text.
#[derive(Debug, Default)]
enum Route {
    #[default]
    Engine,
    /// Nothing executable in it (a lone comment).
    Blank,
    /// Intercepted statements bypass the engine with a synthetic result.
    Utility(ResultSet),
    TxControl(TxControl),
    DatabaseDdl(String),
    SchemaDdl(String),
    SequenceDdl(String),
    TypeDdl(String),
    SetRole(String),
    RoleDdl(String),
    Grant(String),
    RlsDdl(String),
    AlterTable(String),
    /// SET TIME ZONE, SET timezone, RESET timezone.
    TimeZone(String),
    /// CREATE TABLE with an EXCLUDE constraint.
    CreateExclusions(String),
    ListenNotify(String),
    Comment(String),
    SearchPath(String),
    /// DECLARE, FETCH, MOVE, CLOSE.
    Cursor(String),
}

#[derive(Debug, Default)]
pub(crate) struct Prepared {
    /// Rewritten, ready for the engine.
    sql: String,
    /// Original statement, kept for sequence expansion (pre-rewrite).
    pub(crate) raw: String,
    pub(crate) num_params: usize,
    /// Each parameter's type: the client's from Parse, then Describe's.
    pub(crate) param_oids: Vec<Oid>,
    /// Each parameter's type as Bind infers it, computed on first need.
    pub(crate) bind_oids: RefCell<Option<Vec<Oid>>>,
    /// The row types Describe advertised for the statement.
    result_oids: Vec<Oid>,
    route: Route,
}

impl Prepared {
    fn routed(route: Route) -> Prepared {
        Prepared {
            route,
            ..Default::default()
        }
    }

    /// Everything a Describe answers without asking the engine.
    fn is_special(&self) -> bool {
        !matches!(self.route, Route::Engine)
    }

    /// The result to describe, for a statement that has one in hand.
    fn utility(&self) -> Option<&ResultSet> {
        match &self.route {
            Route::Utility(rs) => Some(rs),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct Portal {
    prep: Rc<RefCell<Prepared>>,
    params: Vec<Value>,
    /// Result-column format codes from Bind.
    formats: Vec<Format>,
    /// The row types a Describe of the portal advertised.
    result_oids: Vec<Oid>,
    /// Partial fetch: the query runs once, then successive Executes page
    /// through the buffered rows.
    result: Option<ResultSet>,
    pos: usize,
}

impl Portal {
    /// The row types a Describe advertised, if the result has that many columns.
    fn described_oids(&self, n: usize) -> Option<Vec<Oid>> {
        if self.result_oids.len() == n {
            return Some(self.result_oids.clone());
        }
        let p = self.prep.borrow();
        if p.result_oids.len() == n {
            return Some(p.result_oids.clone());
        }
        None
    }
}

impl Session {
    #[allow(clippy::too_many_arguments)] // one argument per field a new session starts from
    pub(crate) fn new(
        c: WireConn,
        db: DbSession,
        user: &str,
        pid: i32,
        canceler: Arc<Canceler>,
        cluster: Arc<dyn Cluster>,
        database: String,
    ) -> Session {
        let user = if user.is_empty() { "postgres" } else { user };
        let notify = NotifyState::new(c.sink.clone());
        Session {
            c,
            db,
            prepared: HashMap::new(),
            portals: HashMap::new(),
            failed: false,
            cluster,
            database,
            tx: TxState::Idle,
            rls_dirty: false,
            schemas_dirty: false,
            seen_databases: database_generation(),
            later_action: false,
            zone_tx_restore: None,
            zone_local_restore: None,
            seq_curr: HashMap::new(),
            seq_last: String::new(),
            canceler,
            sql_prepared: HashMap::new(),
            auth_user: user.to_string(),
            session_user: user.to_string(),
            current_role: user.to_string(),
            search_path: None,
            bypass_cache: HashMap::new(),
            pid,
            listens: HashSet::new(),
            notify,
            tx_advisory: Vec::new(),
            cursors: cursor::Cursors::default(),
        }
    }

    /// Applies the session-level expansions (sequence calls, advisory locks,
    /// enum columns) to the raw statement, then the dialect rewrite. The
    /// expansions run first so the literals they inject are protected by the
    /// string-literal-aware rewrite.
    pub(crate) fn rewrite_for_exec(&mut self, raw: &str, params: &[Value]) -> Result<String> {
        let skip_all = self.take_row_locks(raw)?;
        let raw = &*self.identity_insert(raw)?;
        let x = self.apply_rls(raw);
        let x = self.expand_session_user(&x);
        let x = self.expand_sequences(&x)?;
        let x = self.expand_advisory_locks(&x, params)?;
        let x = self.expand_enums(&x)?;
        let sql = if x == raw {
            rewrite(&x)
        } else {
            crate::rewrite::rewrite_expanded(raw, &x)
        };
        if skip_all {
            // SKIP LOCKED while another transaction holds the write lock: every row is
            // locked, so none is returned.
            return Ok(["SELECT * FROM (", &sql, ") WHERE 0"].concat());
        }
        Ok(sql)
    }

    /// A SELECT ... FOR UPDATE (SHARE, ...) in a transaction takes the write lock,
    /// SQLite's only lock, which holds every row: it waits for it, or with NOWAIT
    /// fails at once with 55P03 as Postgres does on a locked row. With SKIP LOCKED it
    /// answers true when another transaction holds the lock, as every row is then
    /// locked. Outside a transaction a statement is its own, and holds no lock past
    /// itself.
    fn take_row_locks(&mut self, raw: &str) -> Result<bool> {
        if self.tx != TxState::Open {
            return Ok(false);
        }
        let Some(lock) = crate::rewrite::row_lock::row_lock(raw) else {
            return Ok(false);
        };
        let got = self.db.begin_writing(lock != crate::rewrite::row_lock::RowLock::Wait)?;
        match lock {
            _ if got => Ok(false),
            crate::rewrite::row_lock::RowLock::SkipLocked => Ok(true),
            _ => {
                let table = crate::rewrite::row_lock::locked_table(raw);
                Err(errf!(
                    SqlState::LOCK_NOT_AVAILABLE,
                    "could not obtain lock on row in relation \"{table}\""
                ))
            }
        }
    }

    /// Runs a statement in the current transaction, or autocommit if none, so
    /// that a CancelRequest can interrupt it.
    pub(crate) fn exec(&mut self, sql: &str, args: &[Value]) -> Result<ResultSet> {
        let databases = database_generation();
        if self.seen_databases != databases {
            self.seen_databases = databases; // first: the refresh comes back through here
            self.refresh_database_list()?;
        }
        self.canceler.arm();
        let sp = self.search_path.as_deref();
        let r = if self.in_tx() {
            self.db.tx_execute(sql, args, sp)
        } else {
            self.db.execute_sp(sql, args, sp)
        };
        self.canceler.disarm();
        if let Some((before, local)) = overlite_engine::take_zone_change() {
            self.zone_changed_by_sql(before, local)?;
        }
        Ok(r?)
    }

    fn describe_sql(&mut self, sql: &str, args: &[Value]) -> Result<Option<Vec<Column>>> {
        Ok(self.db.describe(sql, args, self.search_path.as_deref())?)
    }

    /// 'I' idle, 'T' in a transaction, 'E' in a failed transaction.
    fn ready_for_query(&mut self) -> Result<()> {
        let status = match self.tx {
            TxState::Idle => b'I',
            TxState::Open => b'T',
            TxState::Failed => b'E',
        };
        self.c.send(MSG_READY_FOR_QUERY, &[status])
    }

    /// BEGIN/COMMIT/ROLLBACK; returns the command tag.
    pub(crate) fn apply_tx_control(&mut self, control: TxControl) -> Result<()> {
        match control {
            TxControl::Begin => {
                if self.tx == TxState::Idle {
                    self.db.begin()?;
                    self.tx = TxState::Open;
                    overlite_engine::mark_transaction_start(true);
                }
                Ok(())
            }
            TxControl::Commit => self.end_tx(true),
            TxControl::Rollback => self.end_tx(false),
        }
    }

    /// Commits (or rolls back) the transaction and clears state. A failed
    /// transaction always rolls back, even on COMMIT.
    pub(crate) fn end_tx(&mut self, commit: bool) -> Result<()> {
        let failed = match self.tx {
            TxState::Idle => return Ok(()), // no transaction in progress; harmless no-op
            TxState::Open => false,
            TxState::Failed => true,
        };
        let cursors = !self.cursors.is_empty();
        if cursors {
            self.end_cursors(commit && !failed);
        }
        self.tx = TxState::Idle;
        overlite_engine::mark_transaction_start(false);
        let r = if commit && !failed {
            self.db.commit()
        } else {
            self.db.rollback()
        };
        if self.rls_dirty {
            self.rls_dirty = false;
            self.invalidate_rls();
        }
        if self.schemas_dirty {
            self.schemas_dirty = false;
            self.db.publish_schemas(commit && !failed && r.is_ok());
        }
        self.release_tx_advisory_locks();
        if cursors {
            self.relist_cursors();
        }
        let zone = self.end_tx_zone(commit && !failed && r.is_ok());
        r?;
        zone
    }

    /// Whether a transaction is open, aborted or not.
    pub(crate) fn in_tx(&self) -> bool {
        self.tx != TxState::Idle
    }

    /// Guards statements issued while a transaction is aborted.
    pub(crate) fn aborted_tx_error(&self) -> bool {
        self.tx == TxState::Failed
    }

    /// Reads and dispatches frontend messages until the client disconnects.
    pub(crate) fn run_loop(&mut self) -> Result<()> {
        loop {
            let (typ, body) = match self.c.read_message() {
                Ok(m) => m,
                Err(e) => {
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    ) {
                        return Ok(());
                    }
                    return Err(e.into());
                }
            };
            self.set_busy(); // a NOTIFY may only write while we're idle

            match typ {
                MSG_QUERY => {
                    self.failed = false;
                    self.handle_query(&body)?;
                    self.ready_for_query()?;
                    self.c.flush()?;
                    self.set_idle();
                }
                MSG_PARSE | MSG_BIND | MSG_DESCRIBE | MSG_EXECUTE | MSG_CLOSE => {
                    // Extended request cycle: buffer responses, no ReadyForQuery
                    // until Sync. Skip the rest of a failed cycle.
                    if self.failed {
                        continue;
                    }
                    self.dispatch_extended(typ, &body)?;
                }
                MSG_SYNC => {
                    self.failed = false;
                    self.ready_for_query()?;
                    self.c.flush()?;
                    self.set_idle();
                }
                MSG_FLUSH => self.c.flush()?,
                MSG_TERMINATE => return Ok(()),
                _ => {
                    let msg = format!("invalid frontend message type {typ}");
                    self.c.send_fatal(SqlState::PROTOCOL_VIOLATION, &msg)?;
                    self.c.flush()?;
                    return Err(errf!(SqlState::PROTOCOL_VIOLATION, "{msg}"));
                }
            }
        }
    }

    fn dispatch_extended(&mut self, typ: u8, body: &[u8]) -> Result<()> {
        match typ {
            MSG_PARSE => self.handle_parse(body),
            MSG_BIND => self.handle_bind(body),
            MSG_DESCRIBE => self.handle_describe(body),
            MSG_EXECUTE => self.handle_execute(body),
            MSG_CLOSE => self.handle_close(body),
            _ => Ok(()),
        }
    }

    /// Reports a per-cycle error and arms the skip-until-Sync flag.
    fn proto_error(&mut self, code: SqlState, msg: &str) -> Result<()> {
        self.failed = true;
        self.c.send_error(code, msg)
    }

    fn put_prepared(&mut self, name: String, p: Prepared) -> Result<()> {
        self.prepared.insert(name, Rc::new(RefCell::new(p)));
        self.c.send(MSG_PARSE_COMPLETE, &[])
    }

    fn handle_parse(&mut self, body: &[u8]) -> Result<()> {
        let mut r = new_reader(body);
        let name = r.cstring();
        let query = r.cstring();
        let n_oids = r.int16();
        let mut declared = Vec::with_capacity(n_oids.max(0) as usize);
        for _ in 0..n_oids {
            declared.push(Oid::new(r.int32() as u32));
        }
        if r.err {
            return self.proto_error(SqlState::PROTOCOL_VIOLATION, "malformed Parse message");
        }

        let raw = trim_statement(&query);
        if is_blank_statement(&raw) {
            return self.put_prepared(name, Prepared::routed(Route::Blank));
        }
        crate::ast::record::statement(&raw);
        let raw = match statement_text(&raw) {
            Ok(raw) => raw.into_owned(),
            Err(e) => {
                self.mark_tx_failed();
                return self.proto_error(e.sql_state(), e.message());
            }
        };
        if let Some(route) = self.route_of(&raw) {
            return self.put_prepared(name, Prepared::routed(route));
        }
        let sql = rewrite(&raw);
        let num_params = count_params(&sql);
        self.put_prepared(
            name,
            Prepared {
                sql,
                raw,
                num_params,
                param_oids: declared,
                ..Default::default()
            },
        )
    }

    /// The session's own route for a statement, None for one the engine runs.
    fn route_of(&mut self, raw: &str) -> Option<Route> {
        let text = || raw.to_string();
        if cursor::is_cursor_statement(raw) {
            return Some(Route::Cursor(text()));
        }
        if let Some(control) = TxControl::of(raw) {
            return Some(Route::TxControl(control));
        }
        if is_schema_ddl(raw) {
            return Some(Route::SchemaDdl(text()));
        }
        if alter_table_routed(raw) {
            return Some(Route::AlterTable(text()));
        }
        if creates_exclusions(raw) {
            return Some(Route::CreateExclusions(text()));
        }
        if is_listen_notify(raw) {
            return Some(Route::ListenNotify(text()));
        }
        if is_database_ddl(raw) {
            return Some(Route::DatabaseDdl(text()));
        }
        if is_comment(raw) {
            return Some(Route::Comment(text()));
        }
        if time_zone::zone_change(raw).is_some() {
            return Some(Route::TimeZone(text()));
        }
        if is_search_path_stmt(raw) {
            if first_word_upper(raw).eq_ignore_ascii_case("SHOW") {
                // SHOW: no mutation; bake the current value
                return match self.try_search_path(raw) {
                    Some(SearchPathOutcome::Shown(rs)) => Some(Route::Utility(rs)),
                    _ => None,
                };
            }
            // SET/RESET: mutate at Execute
            return Some(Route::SearchPath(text()));
        }
        if is_set_role(raw) {
            return Some(Route::SetRole(text()));
        }
        if explain::is_explain(raw) {
            // The plan is built here, at Parse; a statement the plan cannot be had
            // for goes to the engine, which reports why.
            if let Ok(rs) = self.explain(raw) {
                return Some(Route::Utility(rs));
            }
        }
        if let Some(rs) = intercept_utility(raw) {
            return Some(Route::Utility(rs));
        }
        if is_role_ddl(raw) {
            return Some(Route::RoleDdl(text()));
        }
        if is_sequence_ddl(raw) {
            return Some(Route::SequenceDdl(text()));
        }
        if is_type_ddl(raw) {
            return Some(Route::TypeDdl(text()));
        }
        if is_grant(raw) {
            return Some(Route::Grant(text()));
        }
        if is_rls_ddl(raw) {
            return Some(Route::RlsDdl(text()));
        }
        None
    }

    fn handle_bind(&mut self, body: &[u8]) -> Result<()> {
        let mut r = new_reader(body);
        let portal_name = r.cstring();
        let stmt_name = r.cstring();

        let n_fmt = r.int16();
        let mut fmts = Vec::with_capacity(n_fmt.max(0) as usize);
        for _ in 0..n_fmt {
            fmts.push(Format::from_code(r.int16()));
        }

        let prep = self.prepared.get(&stmt_name).cloned();
        let oids: Vec<Oid> = prep.as_ref().map(|p| p.borrow().param_oids.clone()).unwrap_or_default();
        let n_params = r.int16();
        let mut params = vec![Value::Null; n_params.max(0) as usize];
        for (i, param) in params.iter_mut().enumerate() {
            let length = r.int32();
            if length == -1 {
                continue;
            }
            let oid = oids.get(i).copied().unwrap_or(Oid::UNSPECIFIED);
            *param = decode_param(format_for(&fmts, i), oid, r.bytes(length));
        }
        if let Some(p) = &prep
            && !r.err
        {
            let p = p.borrow();
            self.coerce_text_bools(&p, &fmts, &mut params);
        }

        let n_res_fmt = r.int16();
        let mut res_fmts = Vec::with_capacity(n_res_fmt.max(0) as usize);
        for _ in 0..n_res_fmt {
            res_fmts.push(Format::from_code(r.int16()));
        }
        if r.err {
            return self.proto_error(SqlState::PROTOCOL_VIOLATION, "malformed Bind message");
        }

        let Some(prep) = prep else {
            return self.proto_error(
                SqlState::INVALID_SQL_STATEMENT_NAME,
                &format!("unknown prepared statement {}", quote_name(&stmt_name)),
            );
        };
        let wanted = highest_param(&prep.borrow().raw);
        if (n_params.max(0) as usize) < wanted {
            return self.proto_error(
                SqlState::PROTOCOL_VIOLATION,
                &format!(
                    "bind message supplies {n_params} parameters, but prepared statement {} requires {wanted}",
                    quote_name(&stmt_name)
                ),
            );
        }
        self.portals.insert(
            portal_name,
            Rc::new(RefCell::new(Portal {
                prep,
                params,
                formats: res_fmts,
                result_oids: Vec::new(),
                result: None,
                pos: 0,
            })),
        );
        self.c.send(MSG_BIND_COMPLETE, &[])
    }

    fn handle_describe(&mut self, body: &[u8]) -> Result<()> {
        let mut r = new_reader(body);
        let kind = r.byte();
        let name = r.cstring();
        if r.err {
            return self.proto_error(SqlState::PROTOCOL_VIOLATION, "malformed Describe message");
        }

        let prep: Rc<RefCell<Prepared>>;
        let mut pt: Option<Rc<RefCell<Portal>>> = None;
        let args: Vec<Value>;

        match kind {
            b'S' => {
                let Some(p) = self.prepared.get(&name).cloned() else {
                    return self.proto_error(
                        SqlState::INVALID_SQL_STATEMENT_NAME,
                        &format!("unknown prepared statement {}", quote_name(&name)),
                    );
                };
                prep = p;
                let cursor_sql = match &prep.borrow().route {
                    Route::Cursor(sql) => Some(sql.clone()),
                    _ => None,
                };
                if let Some(sql) = cursor_sql {
                    let declared = prep.borrow().param_oids.clone();
                    return self.describe_cursor_statement(&sql, &declared);
                }
                if prep.borrow().is_special() {
                    self.c.send_parameter_description(0)?;
                    let p = prep.borrow();
                    return self.send_util_describe(p.utility());
                }
                let (raw, num, declared) = {
                    let p = prep.borrow();
                    (p.raw.clone(), p.num_params, p.param_oids.clone())
                };
                let oids = self.param_types(&raw, num, &declared);
                self.c.send_parameter_types(&oids)?;
                prep.borrow_mut().param_oids = oids;
                // Probe with 0 (not NULL) so expression columns like "$1 * 2"
                // keep a concrete type we can advertise; NULL would erase it.
                args = probe_args(num);
            }
            b'P' => {
                if !self.portals.contains_key(&name)
                    && let Some((cols, oids)) = self.cursor_description(&name, true)
                {
                    // A cursor is a portal: psycopg describes its named cursors so.
                    return self.c.send_row_description(&cols, &oids);
                }
                let Some(p) = self.portals.get(&name).cloned() else {
                    return self.proto_error(
                        SqlState::INVALID_CURSOR_NAME,
                        &format!("unknown portal {}", quote_name(&name)),
                    );
                };
                prep = p.borrow().prep.clone();
                let cursor_sql = match &prep.borrow().route {
                    Route::Cursor(sql) => Some(sql.clone()),
                    _ => None,
                };
                if let Some(sql) = cursor_sql {
                    return match self.cursor_description(&sql, false) {
                        Some((cols, oids)) => self.c.send_row_description(&cols, &oids),
                        None => self.c.send(MSG_NO_DATA, &[]),
                    };
                }
                if prep.borrow().is_special() {
                    let p = prep.borrow();
                    return self.send_util_describe(p.utility());
                }
                args = p.borrow().params.clone();
                pt = Some(p);
            }
            _ => return self.proto_error(SqlState::PROTOCOL_VIOLATION, "invalid Describe target"),
        }

        let (raw, sql) = {
            let p = prep.borrow();
            (p.raw.clone(), p.sql.clone())
        };
        let mut desc_sql = sql.clone();
        if has_seq_func(&raw) || has_advisory_func(&raw) {
            // Describe must parse for column types but never advance a sequence
            // or take a lock; neutralize the calls to a constant, then rewrite.
            desc_sql = crate::rewrite::rewrite_expanded(&raw, &neutralize_advisory_locks(&neutralize_sequences(&raw)));
        }
        let cols = match self.describe_sql(&desc_sql, &args) {
            Ok(c) => c,
            Err(e) => {
                log_query_error("describe", &e, &sql, &sql);
                // The code the statement would fail with at Execute.
                return self.proto_error(e.sql_state(), e.message());
            }
        };
        let Some(cols) = cols else {
            return self.c.send(MSG_NO_DATA, &[]);
        };
        let catalog = is_catalog_query(&raw);
        let mut oids = Vec::with_capacity(cols.len());
        for (i, col) in cols.iter().enumerate() {
            let mut oid = oid_for_column(col, &[], i);
            if catalog && oid == OID_TEXT && col.decl_type.is_empty() && INT_CATALOG_COLUMNS.contains(col.name.as_str())
            {
                oid = OID_INT8;
            }
            oids.push(oid);
        }
        self.c.send_row_description(&cols, &oids)?;
        match pt {
            Some(p) => p.borrow_mut().result_oids = oids,
            None => prep.borrow_mut().result_oids = oids,
        }
        Ok(())
    }

    /// False when a value had no form to send in; the error has gone out then.
    fn send_rows_of(&mut self, rs: &ResultSet, formats: &[Format]) -> Result<bool> {
        let oids: Vec<Oid> = rs
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| oid_for_column(col, &rs.rows, i))
            .collect();
        self.send_rows(&rs.rows, &oids, formats)
    }

    /// Sends DataRows, or ends the statement at a value with no binary form. Such a
    /// value cannot exist in Postgres, so there is no answer of its to follow: this is
    /// the code it uses for a value and a binary format that do not fit, on the way in.
    fn send_rows(&mut self, rows: &[Vec<Value>], oids: &[Oid], formats: &[Format]) -> Result<bool> {
        match self.c.send_data_rows(rows, oids, formats)? {
            None => Ok(true),
            Some(why) => {
                self.mark_tx_failed();
                self.proto_error(SqlState::INVALID_BINARY_REPRESENTATION, &why.to_string())?;
                Ok(false)
            }
        }
    }

    fn handle_execute(&mut self, body: &[u8]) -> Result<()> {
        let mut r = new_reader(body);
        let portal_name = r.cstring();
        let max_rows = r.int32(); // 0 = no limit
        if r.err {
            return self.proto_error(SqlState::PROTOCOL_VIOLATION, "malformed Execute message");
        }

        let Some(pt) = self.portals.get(&portal_name).cloned() else {
            return self.proto_error(
                SqlState::INVALID_CURSOR_NAME,
                &format!("unknown portal {}", quote_name(&portal_name)),
            );
        };
        let mut pt = pt.borrow_mut();
        let prep_rc = pt.prep.clone();
        let prep = prep_rc.borrow();

        match &prep.route {
            Route::Blank => return self.c.send(MSG_EMPTY_QUERY, &[]),
            Route::DatabaseDdl(sql) => {
                return match self.try_database_ddl(sql) {
                    Some(Err(e)) => {
                        self.mark_tx_failed();
                        self.proto_error(e.sql_state(), e.message())
                    }
                    other => {
                        let tag = other.and_then(std::result::Result::ok).unwrap_or_default();
                        self.refresh_database_list()?;
                        self.c.send_command_complete(&tag.to_string())
                    }
                };
            }
            Route::TxControl(control) => {
                return match self.apply_tx_control(*control) {
                    Ok(()) => self.c.send_command_complete(control.tag()),
                    Err(e) => {
                        self.mark_tx_failed();
                        self.proto_error(e.sql_state(), e.message())
                    }
                };
            }
            _ => {}
        }

        if let Some(r) = self.try_savepoint(&prep.raw) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => self.proto_error(e.sql_state(), e.message()),
            };
        }

        if self.aborted_tx_error() {
            return self.proto_error(
                SqlState::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            );
        }

        match &prep.route {
            Route::SchemaDdl(sql) => {
                let r = self.try_schema_ddl(sql);
                return self.finish_ddl(r);
            }
            Route::RoleDdl(sql) => {
                let r = self.try_role_ddl(sql);
                self.clear_priv_cache(); // superuser/managed status may have changed
                return self.finish_ddl(r);
            }
            Route::SequenceDdl(sql) => {
                let r = self.try_sequence_ddl(sql);
                return self.finish_ddl(r);
            }
            Route::TypeDdl(sql) => {
                let r = self.try_type_ddl(sql);
                return self.finish_ddl(r);
            }
            Route::SetRole(sql) => {
                return match self.apply_set_role(sql) {
                    Ok(()) => self.c.send_command_complete(&first_word_upper(sql)),
                    Err(e) => {
                        self.mark_tx_failed();
                        self.proto_error(e.sql_state(), e.message())
                    }
                };
            }
            Route::Grant(sql) => {
                let r = self.apply_grant(sql);
                return self.finish_ddl(Some(r));
            }
            Route::RlsDdl(sql) => {
                let r = self.try_rls_ddl(sql);
                return self.finish_ddl(r);
            }
            Route::AlterTable(sql) => {
                return match self.alter_table_prepared(sql) {
                    Err(e) => {
                        self.mark_tx_failed();
                        self.proto_exec_error(&e)
                    }
                    Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                };
            }
            Route::CreateExclusions(sql) => {
                return match self
                    .create_with_exclusions(sql)
                    .unwrap_or_else(|| Ok(Command::from("CREATE TABLE")))
                {
                    Err(e) => {
                        self.mark_tx_failed();
                        self.proto_exec_error(&e)
                    }
                    Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                };
            }
            Route::ListenNotify(sql) => {
                let tag = self.try_listen_notify(sql).unwrap_or_default();
                return self.c.send_command_complete(&tag.to_string());
            }
            Route::Comment(sql) => {
                let r = self.try_comment(sql);
                return self.finish_ddl(r);
            }
            Route::TimeZone(sql) => {
                let Some(change) = time_zone::zone_change(sql) else {
                    return self.c.send_command_complete("SET");
                };
                return match self.apply_zone_change(&change) {
                    Ok(()) => self.c.send_command_complete(if first_word_upper(sql) == "RESET" {
                        "RESET"
                    } else {
                        "SET"
                    }),
                    Err(e) => {
                        self.mark_tx_failed();
                        self.proto_error(e.sql_state(), e.message())
                    }
                };
            }
            Route::SearchPath(sql) => {
                if let Some(SearchPathOutcome::Shown(rs)) = self.try_search_path(sql) {
                    if !self.send_rows_of(&rs, &pt.formats)? {
                        return Ok(());
                    }
                    return self.c.send_command_complete(&rs.command_tag());
                }
                return self.c.send_command_complete("SET");
            }
            Route::Utility(u) => {
                if u.is_query && !self.send_rows_of(u, &pt.formats)? {
                    return Ok(());
                }
                return self.c.send_command_complete(&u.command_tag());
            }
            Route::Cursor(sql) => {
                return match self.cursor_statement(sql, &pt.params) {
                    Ok(out) => {
                        if let Some(f) = &out.rows
                            && !self.send_rows(&f.rows, &f.oids, &pt.formats)?
                        {
                            return Ok(());
                        }
                        self.c.send_command_complete(&out.tag)
                    }
                    Err(e) => {
                        self.mark_tx_failed();
                        self.failed = true;
                        self.send_cursor_error(&e)
                    }
                };
            }
            Route::Engine | Route::Blank | Route::DatabaseDdl(_) | Route::TxControl(_) => {}
        }

        // Run the query once; successive Executes on the same portal page
        // through the buffered rows (partial fetch).
        if pt.result.is_none() {
            if let Err(e) = self.check_privileges(&prep.raw) {
                self.mark_tx_failed();
                return self.proto_error(e.sql_state(), e.message());
            }
            let raw = self.apply_column_defaults(&prep.raw);
            if let Some(r) = self.try_rls_insert(&raw, &pt.params) {
                return match r {
                    Err(e) => {
                        self.mark_tx_failed();
                        self.proto_error(e.sql_state(), e.message())
                    }
                    Ok(rs) => {
                        // DO UPDATE ... RETURNING passed the row check
                        if rs.is_query && !self.send_rows_of(&rs, &pt.formats)? {
                            return Ok(());
                        }
                        pt.result = None;
                        pt.pos = 0;
                        self.c.send_command_complete(&rs.command_tag())
                    }
                };
            }
            let exec_sql = match self.rewrite_for_exec(&raw, &pt.params) {
                Ok(s) => s,
                Err(e) => {
                    self.mark_tx_failed();
                    return self.proto_error(e.sql_state(), e.message());
                }
            };
            // What RETURNING hands back is held until the statement is done, in
            // Postgres, and a failure there undoes it. So a write's rows are checked
            // before one goes out, inside a transaction of the statement's own.
            let returning = returns_rows_from_write(&prep.raw);
            let own_tx = returning && !self.in_tx();
            if own_tx {
                self.apply_tx_control(TxControl::Begin)?;
            }
            let rs = match self.exec(&exec_sql, &pt.params) {
                Ok(rs) => rs,
                Err(e) => {
                    log_query_error("execute", &e, &exec_sql, &prep.sql);
                    self.mark_tx_failed();
                    if own_tx {
                        self.end_tx(false)?;
                    }
                    return self.proto_exec_error(&e);
                }
            };
            if returning && rs.is_query {
                let oids = pt.described_oids(rs.columns.len()).unwrap_or_else(|| {
                    rs.columns
                        .iter()
                        .enumerate()
                        .map(|(i, col)| oid_for_column(col, &rs.rows, i))
                        .collect()
                });
                if let Some(why) = first_without_form(&rs.rows, &oids, &pt.formats) {
                    self.mark_tx_failed();
                    if own_tx {
                        self.end_tx(false)?;
                    }
                    return self.proto_error(SqlState::INVALID_BINARY_REPRESENTATION, &why.to_string());
                }
            }
            if own_tx && let Err(e) = self.end_tx(true) {
                return self.proto_exec_error(&e);
            }
            self.record_ownership(&prep.raw);
            pt.result = Some(rs);
            pt.pos = 0;
        }
        drop(prep);

        let Some(rs) = pt.result.take() else {
            return Ok(());
        };
        if !rs.is_query {
            pt.pos = 0;
            return self.c.send_command_complete(&rs.command_tag());
        }

        // Extended protocol: DataRows only (the RowDescription went out at
        // Describe).
        let oids = pt.described_oids(rs.columns.len()).unwrap_or_else(|| {
            rs.columns
                .iter()
                .enumerate()
                .map(|(i, col)| oid_for_column(col, &rs.rows, i))
                .collect()
        });
        let mut end = rs.rows.len();
        if max_rows > 0 && pt.pos + (max_rows as usize) < end {
            end = pt.pos + max_rows as usize;
        }
        if !self.send_rows(&rs.rows[pt.pos..end], &oids, &pt.formats)? {
            pt.pos = 0;
            return Ok(());
        }
        pt.pos = end;
        if pt.pos < rs.rows.len() {
            pt.result = Some(rs);
            return self.c.send(MSG_PORTAL_SUSPENDED, &[]); // more rows await
        }
        let tag = rs.command_tag();
        pt.pos = 0;
        self.c.send_command_complete(&tag)
    }

    fn finish_ddl(&mut self, r: Option<Result<Command>>) -> Result<()> {
        match r {
            Some(Err(e)) => {
                self.mark_tx_failed();
                self.proto_error(e.sql_state(), e.message())
            }
            other => {
                let tag = other.and_then(std::result::Result::ok).unwrap_or_default();
                self.c.send_command_complete(&tag.to_string())
            }
        }
    }

    fn handle_close(&mut self, body: &[u8]) -> Result<()> {
        let mut r = new_reader(body);
        let kind = r.byte();
        let name = r.cstring();
        if r.err {
            return self.proto_error(SqlState::PROTOCOL_VIOLATION, "malformed Close message");
        }
        match kind {
            b'S' => {
                self.prepared.remove(&name);
            }
            b'P' => {
                self.portals.remove(&name);
            }
            _ => {}
        }
        self.c.send(MSG_CLOSE_COMPLETE, &[])
    }

    /// A RowDescription for SHOW, NoData for SET/BEGIN/etc.
    fn send_util_describe(&mut self, rs: Option<&ResultSet>) -> Result<()> {
        let Some(rs) = rs.filter(|r| r.is_query) else {
            return self.c.send(MSG_NO_DATA, &[]);
        };
        let oids: Vec<Oid> = rs
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| oid_for_column(col, &rs.rows, i))
            .collect();
        self.c.send_row_description(&rs.columns, &oids)
    }
}

impl Drop for Session {
    /// A client that drops mid-transaction leaves it uncommitted; roll it back,
    /// and stop receiving notifications.
    fn drop(&mut self) {
        unregister_all(&self.notify);
        release_advisory_locks(self);
        if self.in_tx() {
            let _ = self.db.rollback();
            self.tx = TxState::Idle;
        }
    }
}

/// Whether a statement writes and hands rows back.
fn returns_rows_from_write(sql: &str) -> bool {
    matches!(first_word_upper(sql).as_str(), "INSERT" | "UPDATE" | "DELETE")
        && code_only(sql)
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|w| w.eq_ignore_ascii_case("returning"))
}

/// The first value of the rows that has no form in its format.
fn first_without_form(rows: &[Vec<Value>], oids: &[Oid], formats: &[Format]) -> Option<NoBinaryForm> {
    rows.iter().find_map(|row| {
        row.iter()
            .enumerate()
            .find_map(|(i, cell)| encode_value(format_for(formats, i), oids[i], cell).err())
    })
}

/// n non-NULL placeholder values used only for type introspection.
fn probe_args(n: usize) -> Vec<Value> {
    vec![Value::Int(0); n]
}

pub(crate) fn trim_statement(sql: &str) -> String {
    trim_end_chars(sql.trim(), ";").trim().to_string()
}

fn quote_name(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".to_string();
    }
    format!("\"{s}\"")
}

/// 0 codes means all text, 1 code applies to everything, otherwise per column.
#[inline]
pub(crate) fn format_for(fmts: &[Format], i: usize) -> Format {
    match fmts.len() {
        0 => Format::Text,
        1 => fmts[0],
        _ => fmts.get(i).copied().unwrap_or_default(),
    }
}

/// Turns a bound parameter into a Value. Text passes through (SQLite coerces).
/// Binary is read by the parameter's type when it has one, otherwise by length.
fn decode_param(format: Format, oid: Oid, raw: &[u8]) -> Value {
    if format == Format::Text {
        let text = text_from_bytes(raw);
        if oid == OID_BYTEA
            && let Some(b) = bytea_hex(&text)
        {
            return Value::Blob(b);
        }
        return Value::Text(text.into_owned());
    }
    if let Some(v) = decode_typed_param(oid, raw) {
        return v;
    }
    if let Ok(b) = raw.try_into() {
        return Value::Int(i64::from_be_bytes(b));
    }
    if let Ok(b) = raw.try_into() {
        return Value::Int(i64::from(i32::from_be_bytes(b)));
    }
    if let Ok(b) = raw.try_into() {
        return Value::Int(i64::from(i16::from_be_bytes(b)));
    }
    match raw {
        [byte] => Value::Bool(*byte != 0),
        _ => Value::Blob(raw.to_vec()),
    }
}

/// The highest `$N` of a statement, outside literals, comments and quoted names.
fn highest_param(sql: &str) -> usize {
    highest_in(&code_only(sql))
}

/// The highest `$N` of text code_only has been through.
fn highest_in(code: &str) -> usize {
    let b = code.as_bytes();
    let (mut max, mut i) = (0, 0);
    while let Some(at) = b[i..].iter().position(|c| *c == b'$') {
        let start = i + at + 1;
        let digits = b[start..].iter().take_while(|c| c.is_ascii_digit()).count();
        if let Ok(n) = code[start..start + digits].parse::<usize>() {
            max = max.max(n);
        }
        i = start + digits;
    }
    max
}

/// Counts positional placeholders: the highest $N index, or the number of bare
/// "?" placeholders, whichever is greater. Inside a string, a quoted identifier
/// or a comment neither counts.
fn count_params(sql: &str) -> usize {
    let code = code_only(sql);
    let bare = code.bytes().filter(|c| *c == b'?').count();
    highest_in(&code).max(bare)
}

/// A cursor over a message body. On a short read it records an error and
/// returns zero values, so callers check `err` once at the end.
#[derive(Debug)]
pub(crate) struct BodyReader<'a> {
    b: &'a [u8],
    pos: usize,
    pub(crate) err: bool,
}

pub(crate) fn new_reader(b: &[u8]) -> BodyReader<'_> {
    BodyReader { b, pos: 0, err: false }
}

impl<'a> BodyReader<'a> {
    fn byte(&mut self) -> u8 {
        if self.pos + 1 > self.b.len() {
            self.err = true;
            return 0;
        }
        let v = self.b[self.pos];
        self.pos += 1;
        v
    }

    fn int16(&mut self) -> i32 {
        let Some(b) = self.b.get(self.pos..).and_then(<[u8]>::first_chunk) else {
            self.err = true;
            return 0;
        };
        self.pos += 2;
        i32::from(i16::from_be_bytes(*b))
    }

    pub(crate) fn int32(&mut self) -> i32 {
        let Some(b) = self.b.get(self.pos..).and_then(<[u8]>::first_chunk) else {
            self.err = true;
            return 0;
        };
        self.pos += 4;
        i32::from_be_bytes(*b)
    }

    pub(crate) fn cstring(&mut self) -> String {
        let rest = &self.b[self.pos.min(self.b.len())..];
        match rest.iter().position(|c| *c == 0) {
            None => {
                self.err = true;
                String::new()
            }
            Some(i) => {
                let s = text_from_bytes(&rest[..i]).into_owned();
                self.pos += i + 1;
                s
            }
        }
    }

    pub(crate) fn bytes(&mut self, n: i32) -> &'a [u8] {
        if n < 0 || self.pos + n as usize > self.b.len() {
            self.err = true;
            return &[];
        }
        let v = &self.b[self.pos..self.pos + n as usize];
        self.pos += n as usize;
        v
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::tokens::code_tokens;
    use crate::rewrite::{needs_no_rewrite, rewrite_full};
    use crate::session::utility::SavepointKind;
    use overlite_engine::blank_literals;

    /// Statements whose comments hold quotes, `$n`, `?`, casts, dollar quotes and
    /// keywords, each beside the same statement with its comments taken out by hand.
    pub(crate) const COMMENTED: &[(&str, &str)] = &[
        (
            "SELECT relname FROM pg_class WHERE -- when it's a partition\n relname = $1",
            "SELECT relname FROM pg_class WHERE relname = $1",
        ),
        (
            "SELECT a FROM t WHERE /* it's */ b = $1",
            "SELECT a FROM t WHERE b = $1",
        ),
        (
            "SELECT a FROM t WHERE /* outer /* it's */ still ' in */ b = $1",
            "SELECT a FROM t WHERE b = $1",
        ),
        (
            "SELECT a FROM t -- $2 is not a parameter\nWHERE b = $1",
            "SELECT a FROM t WHERE b = $1",
        ),
        (
            "SELECT a /* $3 */ FROM t WHERE b = $1 -- and $2",
            "SELECT a FROM t WHERE b = $1",
        ),
        (
            "SELECT a FROM t WHERE b = $1 -- trailing '",
            "SELECT a FROM t WHERE b = $1",
        ),
        (
            "SELECT a FROM t WHERE b = 'it''s' -- '\n AND c = $1",
            "SELECT a FROM t WHERE b = 'it''s' AND c = $1",
        ),
        (
            "SELECT a FROM t WHERE b = '-- not a comment' OR c = $1 -- it's",
            "SELECT a FROM t WHERE b = '-- not a comment' OR c = $1",
        ),
        (
            "SELECT a FROM t WHERE b = '/* not */' /* it's */ OR c = $1",
            "SELECT a FROM t WHERE b = '/* not */' OR c = $1",
        ),
        (
            "SELECT a FROM t WHERE c = $1 -- FOR UPDATE",
            "SELECT a FROM t WHERE c = $1",
        ),
        (
            "SELECT a FROM t /* RETURNING a */ WHERE c = $1",
            "SELECT a FROM t WHERE c = $1",
        ),
        (
            "UPDATE t SET a = a /* , b = $2 */ WHERE c = $1",
            "UPDATE t SET a = a WHERE c = $1",
        ),
        ("SELECT a FROM t -- \"x\nWHERE c = $1", "SELECT a FROM t WHERE c = $1"),
        ("SELECT a FROM t -- $$\nWHERE c = $1", "SELECT a FROM t WHERE c = $1"),
        (
            "SELECT a FROM t WHERE b = E'it\\'s' -- x'\n OR c = $1",
            "SELECT a FROM t WHERE b = E'it\\'s' OR c = $1",
        ),
        ("SELECT a FROM t /* -- */ WHERE c = $1", "SELECT a FROM t WHERE c = $1"),
        ("SELECT a FROM t -- /*\nWHERE c = $1", "SELECT a FROM t WHERE c = $1"),
        ("SELECT a FROM t WHERE c = $1 -- why?", "SELECT a FROM t WHERE c = $1"),
        ("SELECT a FROM t WHERE c = ? -- ?", "SELECT a FROM t WHERE c = ?"),
        (
            "SELECT a FROM t WHERE c = $1 /* ::boolean 'x' */",
            "SELECT a FROM t WHERE c = $1",
        ),
        (
            "SELECT a::text /* ::int */ FROM t WHERE c = $1",
            "SELECT a::text FROM t WHERE c = $1",
        ),
        (
            "/* BEGIN; */ SELECT a FROM t WHERE c = $1",
            "SELECT a FROM t WHERE c = $1",
        ),
        ("BEGIN /* ISOLATION LEVEL SERIALIZABLE */", "BEGIN"),
        ("SAVEPOINT s1 -- it's", "SAVEPOINT s1"),
        ("SET search_path /* it's */ TO s1", "SET search_path TO s1"),
        (
            "CREATE TABLE t2 (a int /* it's */, b text -- $1\n)",
            "CREATE TABLE t2 (a int , b text )",
        ),
        (
            "SELECT a FROM t WHERE c = ANY ( $1 ) -- it's",
            "SELECT a FROM t WHERE c = ANY ( $1 )",
        ),
        (
            "SELECT a FROM t WHERE b ILIKE $1 /* ~~* */",
            "SELECT a FROM t WHERE b ILIKE $1",
        ),
        (
            "SELECT j->>'k' /* ' */ FROM t WHERE c = $1",
            "SELECT j->>'k' FROM t WHERE c = $1",
        ),
        ("SELECT now() /* AT TIME ZONE 'x' */ FROM t", "SELECT now() FROM t"),
        ("COMMENT ON TABLE t IS 'x' -- it's", "COMMENT ON TABLE t IS 'x'"),
        ("GRANT SELECT ON t TO r /* , it's */", "GRANT SELECT ON t TO r"),
        ("LISTEN ch -- it's", "LISTEN ch"),
        ("CREATE SEQUENCE s1 /* it's */ START 5", "CREATE SEQUENCE s1 START 5"),
        (
            "INSERT INTO t (a, b) VALUES ($1, /* it's */ $2) RETURNING a -- '",
            "INSERT INTO t (a, b) VALUES ($1, $2) RETURNING a",
        ),
        (
            "SELECT a FROM t WHERE c = $1 /* ; DROP TABLE t; */",
            "SELECT a FROM t WHERE c = $1",
        ),
    ];

    /// Text with its runs of whitespace made one space.
    fn squash(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn tokens(s: &str) -> Vec<String> {
        code_tokens(s).iter().map(|t| s[t.span.clone()].to_string()).collect()
    }

    /// What each router says of a statement.
    fn routes(sql: &str) -> Vec<bool> {
        vec![
            is_blank_statement(sql),
            TxControl::of(sql).is_some(),
            SavepointKind::of(sql).is_some(),
            is_schema_ddl(sql),
            alter_table_routed(sql),
            is_listen_notify(sql),
            is_database_ddl(sql),
            is_comment(sql),
            is_search_path_stmt(sql),
            is_set_role(sql),
            intercept_utility(sql).is_some(),
            is_role_ddl(sql),
            is_sequence_ddl(sql),
            is_type_ddl(sql),
            is_grant(sql),
            is_rls_ddl(sql),
        ]
    }

    // A comment is whitespace to Postgres. The door, statement_text, turns each to
    // spaces, so that every scanner after it reads the statement as if the comment
    // were not there; the scanners that also read raw text skip comments themselves.
    #[test]
    fn comments_are_whitespace_to_every_scanner() {
        for (with, without) in COMMENTED {
            let door = statement_text(with).unwrap();
            let bare = statement_text(without).unwrap();
            assert_eq!(tokens(&door), tokens(&bare), "tokens of {with:?}");
            assert_eq!(count_params(with), count_params(without), "count_params({with:?})");
            assert_eq!(
                count_params(&door),
                count_params(without),
                "count_params(door of {with:?})"
            );
            assert_eq!(highest_param(with), highest_param(without), "highest_param({with:?})");
            assert_eq!(
                squash(&code_only(with)),
                squash(&code_only(without)),
                "code_only({with:?})"
            );
            assert_eq!(
                squash(&blank_literals(with)),
                squash(&blank_literals(without)),
                "blank_literals({with:?})"
            );
            assert_eq!(
                first_word_upper(&door),
                first_word_upper(&bare),
                "first word of {with:?}"
            );
            assert_eq!(routes(&door), routes(&bare), "routes of {with:?}");
            assert_eq!(
                needs_no_rewrite(&door),
                needs_no_rewrite(&bare),
                "fast path of {with:?}"
            );
            assert_eq!(squash(&rewrite(&door)), squash(&rewrite(&bare)), "rewrite of {with:?}");
            assert_eq!(
                squash(&rewrite_full(&door)),
                squash(&rewrite_full(&bare)),
                "rewrite_full of {with:?}"
            );
        }
    }

    #[test]
    fn count_params_cases() {
        for (sql, want) in [
            ("SELECT 1", 0),
            ("SELECT $1", 1),
            ("SELECT $1, $2", 2),
            ("SELECT $2, $1", 2), // highest index wins
            ("INSERT INTO t VALUES (?, ?)", 2),
            ("SELECT $1 WHERE a = $1", 1), // reused index counts once
            ("SELECT 'why?'", 0),          // inside a string, not a placeholder
            ("SELECT 'it''s $3?', $1", 1),
            (r#"SELECT "a?b" FROM t"#, 0),
            ("SELECT s REGEXP '(?i)x'", 0), // what ~* writes
            ("SELECT 1 WHERE -- it's\n a = $1", 1),
            ("SELECT 1 /* it's $3 */ WHERE a = $1", 1),
            ("SELECT 1 /* a /* it's */ $3 */ WHERE a = $1", 1),
            ("SELECT $1 -- why?", 1),
            ("SELECT $$ $4 ? $$, $1", 1),
        ] {
            assert_eq!(count_params(sql), want, "count_params({sql:?})");
        }
    }
}
