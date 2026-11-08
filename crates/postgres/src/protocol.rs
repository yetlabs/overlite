//! The PostgreSQL protocol over a Cluster: startup, authentication and the main
//! loop of a connection. The backend message builders live in wire.

use crate::auth::grants::is_grant;
use crate::auth::scram::{ScramVerifier, parse_scram_verifier};
use crate::auth::session_user::is_set_role;
use crate::error::{Error, Result, errf};
use crate::rewrite::blank::{is_blank_statement, statement_text};
use crate::session::cancel::{cancel_all_backends, cancel_backend, rand_int32, register_backend, unregister_backend};
use crate::session::copy::parse_copy;
use crate::session::cursor::is_cursor_statement;
use crate::session::search_path::{SearchPathOutcome, startup_search_path};
use crate::session::time_zone::startup_zone;
use crate::session::utility::{TxControl, first_word_upper, intercept_utility, log_query, log_query_error};
use crate::session::{Session, TxState, trim_statement};
use crate::wire::tls::load_tls;
use crate::wire::{
    MSG_AUTHENTICATION, MSG_BACKEND_KEY_DATA, MSG_EMPTY_QUERY, MSG_PASSWORD_MESSAGE, Startup, WireConn, i32_be,
};
use md5::{Digest, Md5};
use overlite_core::text::{quoted, quoted_char, text_from_bytes};
use overlite_core::{SqlState, Value};
use overlite_engine::{Cluster, DbSession};
use overlite_hba as hba;
use rand::RngCore;
use std::net::{IpAddr, TcpStream};
use std::sync::Arc;

pub struct Protocol {
    /// When non-empty, clients must authenticate with it (POSTGRES_PASSWORD).
    pub(crate) password: String,
    /// When set, clients may upgrade the connection with SSL.
    pub(crate) tls: Option<Arc<rustls::ServerConfig>>,
    /// POSTGRES_HOST_AUTH_METHOD; empty means the default.
    pub(crate) configured_auth: String,
    /// A pg_hba policy that decides the auth method per connection.
    pub(crate) hba: Option<hba::Policy>,
}

impl std::fmt::Debug for Protocol {
    /// Leaves out the password, which is why this is not derived.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Protocol")
            .field("tls", &self.tls.is_some())
            .field("configured_auth", &self.configured_auth)
            .field("hba", &self.hba)
            .finish_non_exhaustive()
    }
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// The HBA policy from OVERLITE_HBA_DIR (default the working directory); a
/// parse error is logged and treated as "no policy".
fn load_hba() -> Option<hba::Policy> {
    let mut dir = env("OVERLITE_HBA_DIR");
    if dir.is_empty() {
        dir = ".".to_string();
    }
    match hba::load(&dir) {
        Ok(p) => p,
        Err(e) => {
            log::warn!("overlite: ignoring pg_hba in {dir}: {e}");
            None
        }
    }
}

/// A credential a password method authenticates against: a stored SCRAM
/// verifier (per-role) or a known plaintext (the global POSTGRES_PASSWORD).
pub(crate) struct Credential {
    pub(crate) plaintext: String,
    pub(crate) scram: Option<ScramVerifier>,
}

impl std::fmt::Debug for Credential {
    /// Leaves the secrets out, which is why this is not derived.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential").finish_non_exhaustive()
    }
}

impl Credential {
    /// Whether password satisfies the credential (cleartext path).
    fn matches(&self, password: &str) -> bool {
        match &self.scram {
            Some(v) => v.verifies(password),
            None => password == self.plaintext,
        }
    }
}

/// Runs cluster.release when the connection ends, however it ends.
#[derive(Debug)]
#[must_use = "dropping it releases the database"]
struct ReleaseGuard<'a> {
    cluster: &'a Arc<dyn Cluster>,
    database: String,
}

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        self.cluster.release(&self.database);
    }
}

#[derive(Debug)]
#[must_use = "dropping it unregisters the backend"]
struct BackendGuard(i32, i32);

impl Drop for BackendGuard {
    fn drop(&mut self) {
        unregister_backend(self.0, self.1);
    }
}

impl Protocol {
    pub fn from_env() -> Protocol {
        Protocol {
            password: env("POSTGRES_PASSWORD"),
            tls: load_tls(),
            configured_auth: env("POSTGRES_HOST_AUTH_METHOD"),
            hba: load_hba(),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        "postgres"
    }

    pub fn default_port(&self) -> u16 {
        5432
    }

    /// Interrupts every running statement, for a shutdown.
    pub(crate) fn cancel_running(&self) {
        cancel_all_backends();
    }

    /// Owns conn until the client disconnects.
    pub(crate) fn serve(&self, conn: TcpStream, cluster: Arc<dyn Cluster>) -> Result<()> {
        let mut c = WireConn::new(conn);

        let params = match c.read_startup(self.tls.as_ref())? {
            Startup::Cancel(req) => {
                // A CancelRequest is a bare connection: interrupt and close.
                cancel_backend(req.pid, req.secret);
                return Ok(());
            }
            Startup::Params(p) => p,
        };
        let get = |k: &str| params.get(k).cloned().unwrap_or_default();

        // Asking for a database this server does not hold is an error.
        let database = get("database");
        let eng = match cluster.engine(&database) {
            Ok(e) => e,
            Err(e) => {
                let _ = c.send_fatal(e.sql_state(), e.message());
                let _ = c.flush();
                return Err(e.into());
            }
        };
        let _release = ReleaseGuard {
            cluster: &cluster,
            database: database.clone(),
        };

        // A dedicated connection per client, opened before auth so we can look
        // up the connecting role's password.
        let db = match eng.session() {
            Ok(d) => d,
            Err(e) => {
                let _ = c.send_fatal(SqlState::TOO_MANY_CONNECTIONS, &format!("too many connections: {e}"));
                let _ = c.flush();
                return Err(e.into());
            }
        };

        let ip = c.peer_ip();
        let secured = c.secured;
        self.authenticate(&mut c, &get("user"), &get("database"), ip, secured, &db)?;

        // A backend key lets a second connection cancel a running query.
        let (pid, secret) = (rand_int32() & 0x7fffffff, rand_int32());
        let cl = register_backend(pid, secret);
        cl.set_handle(db.interrupt_handle());
        let _backend = BackendGuard(pid, secret);

        // The TimeZone the client asks for at startup, else UTC; this thread's from now.
        let zone = startup_zone(&params).unwrap_or_else(|| "UTC".to_string());
        let zone = match overlite_engine::set_session_zone(&zone) {
            Ok(name) => name,
            Err(e) => {
                c.send_error(e.sql_state(), e.message())?;
                return c.flush();
            }
        };
        self.complete_handshake(&mut c, pid, secret, &zone)?;

        let mut s = Session::new(c, db, &get("user"), pid, cl, cluster.clone(), database);
        // The engine's pg_database knows only its own file; the cluster knows
        // the rest, so the view is replaced once the connection is up.
        let _ = s.refresh_database_list();
        // Honor a search_path given at connection time.
        if let Some(sp) = startup_search_path(&params) {
            s.search_path = Some(sp);
        }
        s.run_loop()
    }

    /// trust when no password is set, otherwise POSTGRES_HOST_AUTH_METHOD
    /// (scram-sha-256 by default).
    fn auth_method(&self) -> AuthMethod {
        if self.password.is_empty() {
            return AuthMethod::Trust;
        }
        match self.configured_auth.to_lowercase().as_str() {
            "trust" => AuthMethod::Trust,
            "password" => AuthMethod::Password,
            "md5" => AuthMethod::Md5,
            _ => AuthMethod::Scram,
        }
    }

    fn authenticate(
        &self,
        c: &mut WireConn,
        user: &str,
        database: &str,
        ip: Option<IpAddr>,
        ssl: bool,
        sess: &DbSession,
    ) -> Result<()> {
        // A role marked NOLOGIN can't open a session, whatever the auth method.
        if !role_may_login(sess, user) {
            let _ = c.send_fatal(
                SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                &format!("role {user:?} is not permitted to log in", user = quoted(user)),
            );
            let _ = c.flush();
            return Err(errf!(
                SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                "role {user:?} is not permitted to log in",
                user = quoted(user)
            ));
        }
        let method = self.resolve_method(user, database, ip, ssl);
        if method == AuthMethod::Reject {
            let _ = c.send_fatal(
                SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                &format!(
                    "no pg_hba.conf entry for user {user:?}, database {database:?}",
                    user = quoted(user),
                    database = quoted(database)
                ),
            );
            let _ = c.flush();
            return Err(errf!(
                SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                "connection rejected by pg_hba: user={user} db={database}"
            ));
        }
        if method == AuthMethod::Trust {
            return Ok(());
        }
        let Some(cred) = self.credential_for(sess, user) else {
            return self.auth_failed(c); // password method but no credential configured
        };
        match method {
            AuthMethod::Password => self.auth_cleartext(c, &cred),
            AuthMethod::Md5 => self.auth_md5(c, user, &cred),
            _ => self.auth_scram(c, user, &cred),
        }
    }

    /// The connecting role's stored SCRAM verifier if it has one, otherwise the
    /// global POSTGRES_PASSWORD.
    fn credential_for(&self, sess: &DbSession, user: &str) -> Option<Credential> {
        if let Some(v) = lookup_role_verifier(sess, user) {
            return Some(Credential {
                plaintext: String::new(),
                scram: Some(v),
            });
        }
        if !self.password.is_empty() {
            return Some(Credential {
                plaintext: self.password.clone(),
                scram: None,
            });
        }
        None
    }

    fn resolve_method(&self, user: &str, db: &str, ip: Option<IpAddr>, ssl: bool) -> AuthMethod {
        let Some(policy) = &self.hba else {
            return self.auth_method();
        };
        let conn = hba::Conn {
            local: ip.is_none(),
            ssl,
            database: db.to_string(),
            user: user.to_string(),
            ip,
        };
        match policy.method(&conn) {
            Some(m) => AuthMethod::from_hba(&m),
            None => AuthMethod::Reject,
        }
    }

    /// Reports the standard auth failure to the client.
    pub(crate) fn auth_failed(&self, c: &mut WireConn) -> Result<()> {
        let _ = c.send_fatal(SqlState::INVALID_PASSWORD, "password authentication failed");
        let _ = c.flush();
        Err(errf!(SqlState::INVALID_PASSWORD, "password authentication failed"))
    }

    fn auth_cleartext(&self, c: &mut WireConn, cred: &Credential) -> Result<()> {
        c.send(MSG_AUTHENTICATION, &i32_be(3))?;
        c.flush()?;
        let (typ, body) = c.read_auth_message()?;
        if typ != MSG_PASSWORD_MESSAGE {
            return Err(errf!(
                SqlState::PROTOCOL_VIOLATION,
                "expected password message, got {:?}",
                quoted_char(typ as char)
            ));
        }
        let text = String::from_utf8_lossy(&body);
        if !cred.matches(text.trim_end_matches('\0')) {
            return self.auth_failed(c);
        }
        Ok(())
    }

    /// AuthenticationMD5Password: needs the plaintext, so a role stored only as
    /// a SCRAM verifier can't use md5 (Postgres behaves the same).
    fn auth_md5(&self, c: &mut WireConn, user: &str, cred: &Credential) -> Result<()> {
        let mut salt = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut salt);
        let mut b = i32_be(5).to_vec();
        b.extend_from_slice(&salt);
        c.send(MSG_AUTHENTICATION, &b)?;
        c.flush()?;
        let (typ, body) = c.read_auth_message()?;
        if typ != MSG_PASSWORD_MESSAGE {
            return Err(errf!(
                SqlState::PROTOCOL_VIOLATION,
                "expected password message, got {:?}",
                quoted_char(typ as char)
            ));
        }
        let text = String::from_utf8_lossy(&body);
        if cred.plaintext.is_empty() || text.trim_end_matches('\0') != md5_password(&cred.plaintext, user, &salt) {
            return self.auth_failed(c);
        }
        Ok(())
    }

    /// AuthenticationOk, the parameter statuses, and the first ReadyForQuery.
    fn complete_handshake(&self, c: &mut WireConn, pid: i32, secret: i32, zone: &str) -> Result<()> {
        c.send(MSG_AUTHENTICATION, &i32_be(0))?;
        for (k, v) in [
            ("server_version", "16.0 (overlite)"),
            ("server_encoding", "UTF8"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("integer_datetimes", "on"),
            ("standard_conforming_strings", "on"), // required by pgx's simple protocol
            ("TimeZone", zone),
        ] {
            c.send_parameter_status(k, v)?;
        }
        let mut b = i32_be(pid).to_vec();
        b.extend_from_slice(&i32_be(secret));
        c.send(MSG_BACKEND_KEY_DATA, &b)?;
        c.send_ready_for_query()?;
        c.flush()
    }
}

/// A role is blocked only when it exists and its rolcanlogin flag is 0.
fn role_may_login(sess: &DbSession, user: &str) -> bool {
    let rs = sess.execute(
        "SELECT rolcanlogin FROM _overlite_roles WHERE rolname = ? COLLATE NOCASE",
        &[Value::from(user)],
    );
    match rs {
        Ok(rs) if !rs.rows.is_empty() && !rs.rows[0][0].is_null() => rs.rows[0][0].to_string() != "0",
        _ => true,
    }
}

/// A role's stored SCRAM verifier from _overlite_roles.
fn lookup_role_verifier(sess: &DbSession, user: &str) -> Option<ScramVerifier> {
    let rs = sess
        .execute(
            "SELECT rolpassword FROM _overlite_roles WHERE rolname = ? COLLATE NOCASE AND rolpassword IS NOT NULL",
            &[Value::from(user)],
        )
        .ok()?;
    if rs.rows.is_empty() || rs.rows[0][0].is_null() {
        return None;
    }
    parse_scram_verifier(&rs.rows[0][0].to_string())
}

/// How a connection authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthMethod {
    Trust,
    Reject,
    Md5,
    Password,
    Scram,
}

impl AuthMethod {
    /// Maps a pg_hba method name to one overlite implements.
    fn from_hba(name: &str) -> AuthMethod {
        match name.to_lowercase().as_str() {
            "trust" | "peer" | "cert" => AuthMethod::Trust,
            "reject" => AuthMethod::Reject,
            "md5" => AuthMethod::Md5,
            "password" => AuthMethod::Password,
            _ => AuthMethod::Scram,
        }
    }
}

/// The Postgres MD5 auth response for a password/user/salt.
fn md5_password(password: &str, user: &str, salt: &[u8]) -> String {
    let inner = md5_hex(format!("{password}{user}").as_bytes());
    let mut b = inner.into_bytes();
    b.extend_from_slice(salt);
    format!("md5{}", md5_hex(&b))
}

fn md5_hex(b: &[u8]) -> String {
    hex::encode(Md5::digest(b))
}

impl Session {
    pub(crate) fn handle_simple_query(&mut self, body: &[u8]) -> Result<()> {
        let text = text_from_bytes(body);
        let sql = trim_statement(text.trim_end_matches('\0'));

        // A statement with nothing executable in it, including one that is only
        // a comment, gets EmptyQueryResponse and never reaches the engine.
        if is_blank_statement(&sql) {
            return self.c.send(MSG_EMPTY_QUERY, &[]);
        }
        let sql = match statement_text(&sql) {
            Ok(sql) => sql,
            Err(e) => return self.fail_tx_send(&e),
        };
        let sql = sql.as_ref();

        // Transaction control is handled by the session.
        if let Some(control) = TxControl::of(sql) {
            return match self.apply_tx_control(control) {
                Ok(()) => self.c.send_command_complete(control.tag()),
                Err(e) => {
                    self.mark_tx_failed();
                    self.c.send_error(e.sql_state(), e.message())
                }
            };
        }

        // Savepoints (incl. ROLLBACK TO, which recovers an aborted tx) are
        // handled before the aborted-tx guard.
        if let Some(r) = self.try_savepoint(sql) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => self.c.send_error(e.sql_state(), e.message()),
            };
        }

        // Reject non-tx-control statements while the transaction is aborted.
        if self.aborted_tx_error() {
            return self.c.send_error(
                SqlState::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            );
        }

        if is_cursor_statement(sql) {
            return self.simple_cursor_statement(sql);
        }

        if let Some(r) = self.try_schema_ddl(sql) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if let Some(r) = self.try_role_ddl(sql) {
            return match r {
                Ok(tag) => {
                    self.clear_priv_cache(); // superuser/managed status may have changed
                    self.c.send_command_complete(&tag.to_string())
                }
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if is_grant(sql) {
            return match self.apply_grant(sql) {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if let Err(e) = self.check_alter_table(sql) {
            return self.fail_tx_send(&e);
        }

        match self.alter_skipped(sql) {
            Ok(true) => return self.c.send_command_complete("ALTER TABLE"),
            Ok(false) => {}
            Err(e) => return self.fail_tx_send(&e),
        }

        if let Some(r) = self.try_rls_ddl(sql) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if let Some(r) = self.create_with_exclusions(sql) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => {
                    self.mark_tx_failed();
                    self.send_exec_error(&e)
                }
            };
        }

        if let Some(r) = self.try_alter_table(sql) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => {
                    self.mark_tx_failed();
                    self.send_exec_error(&e)
                }
            };
        }

        if let Some(tag) = self.try_listen_notify(sql) {
            return self.c.send_command_complete(&tag.to_string());
        }

        if let Some(r) = self.try_sequence_ddl(sql) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if let Some(r) = self.try_type_ddl(sql) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if let Some(r) = self.try_comment(sql) {
            return match r {
                Ok(tag) => self.c.send_command_complete(&tag.to_string()),
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if is_set_role(sql) {
            return match self.apply_set_role(sql) {
                Ok(()) => self.c.send_command_complete(&first_word_upper(sql)),
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if let Some(r) = self.try_sql_prepare(sql) {
            return r;
        }

        if let Some(cs) = parse_copy(sql) {
            return self.handle_copy(&cs);
        }

        if let Some(change) = crate::session::time_zone::zone_change(sql) {
            return match self.apply_zone_change(&change) {
                Ok(()) => self.c.send_command_complete(if first_word_upper(sql) == "RESET" {
                    "RESET"
                } else {
                    "SET"
                }),
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if let Some(r) = self.try_search_path(sql) {
            return match r {
                SearchPathOutcome::Shown(rs) => {
                    self.c.send_result_set(&rs)?;
                    self.c.send_command_complete(&rs.command_tag())
                }
                SearchPathOutcome::Changed => self.c.send_command_complete("SET"),
            };
        }

        if let Some(rs) = intercept_utility(sql) {
            if rs.is_query {
                self.c.send_result_set(&rs)?;
            }
            return self.c.send_command_complete(&rs.command_tag());
        }

        if let Err(e) = self.check_privileges(sql) {
            return self.fail_tx_send(&e);
        }

        if crate::session::explain::is_explain(sql) {
            return match self.explain(sql) {
                Ok(rs) => {
                    self.c.send_result_set(&rs)?;
                    self.c.send_command_complete(&rs.command_tag())
                }
                Err(e) => self.fail_tx_send(&e),
            };
        }

        if let Some(r) = self.try_database_ddl(sql) {
            return match r {
                Ok(tag) => {
                    self.refresh_database_list()?;
                    self.c.send_command_complete(&tag.to_string())
                }
                Err(e) => self.fail_tx_send(&e),
            };
        }

        // Record/strip nextval column defaults (CREATE TABLE) or inject them
        // (INSERT).
        let sql = self.apply_column_defaults(sql);

        if let Some(r) = self.try_rls_insert(&sql, &[]) {
            return match r {
                Ok(rs) => {
                    if rs.is_query {
                        // DO UPDATE ... RETURNING passed the row check
                        self.c.send_result_set(&rs)?;
                    }
                    self.c.send_command_complete(&rs.command_tag())
                }
                Err(e) => self.fail_tx_send(&e),
            };
        }

        // Expand sequence calls and enum columns on the raw statement, before
        // the dialect rewrite (see rewrite_for_exec).
        let rewritten = match self.rewrite_for_exec(&sql, &[]) {
            Ok(r) => r,
            Err(e) => return self.fail_tx_send(&e),
        };
        log_query("simple", &rewritten);
        let rs = match self.exec(&rewritten, &[]) {
            Ok(rs) => rs,
            Err(e) => {
                log_query_error("simple", &e, &sql, &rewritten);
                self.mark_tx_failed();
                return self.send_exec_error(&e);
            }
        };
        self.record_ownership(&sql);
        if rs.is_query {
            self.c.send_result_set(&rs)?;
        }
        self.c.send_command_complete(&rs.command_tag())
    }

    /// Marks an open transaction as failed.
    pub(crate) fn mark_tx_failed(&mut self) {
        if self.tx == TxState::Open {
            self.tx = TxState::Failed;
        }
    }

    pub(crate) fn fail_send(&mut self, e: &Error) -> Result<()> {
        self.fail_tx_send(e)
    }

    fn fail_tx_send(&mut self, e: &Error) -> Result<()> {
        self.mark_tx_failed();
        self.c.send_error(e.sql_state(), e.message())
    }
}
