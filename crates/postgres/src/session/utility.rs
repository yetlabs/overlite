//! Statements clients expect to succeed but SQLite can't run, classifiers for
//! transaction control and savepoints, and the debug query log.

use crate::ddl::words::{Word, ddl_words, syntax_error_at};
use crate::error::{Error, Result, errf};
use crate::session::{Session, TxState};
use overlite_core::text::{eq_fold, quote_literal, split_words, trim_end_chars, trim_start_chars};
use overlite_core::{Column, Command, ResultSet, SqlState, Value};
use overlite_engine::code_only;

/// Logs the SQL of an executed query (OVERLITE_DEBUG=all).
pub(crate) fn log_query(phase: &str, sql: &str) {
    log::trace!("[{phase}] {sql}");
}

/// Logs the SQL behind a failed query (set OVERLITE_DEBUG=1), which is how to reproduce
/// a real client's exact statements.
pub(crate) fn log_query_error(phase: &str, err: &Error, original: &str, rewritten: &str) {
    if original == rewritten {
        log::debug!("[{phase}] query error: {err}\n  sql: {original}");
        return;
    }
    log::debug!("[{phase}] query error: {err}\n  sql:       {original}\n  rewritten: {rewritten}");
}

/// Handles statements that clients (psql, the JDBC driver DBeaver uses, ...)
/// expect to succeed but SQLite can't run: session control (SET/RESET/DISCARD)
/// and SHOW. A synthetic result when it handled the statement.
pub(crate) fn intercept_utility(sql: &str) -> Option<ResultSet> {
    let w = first_word_upper(sql);
    // SET CONSTRAINTS checks what it makes IMMEDIATE, in the engine.
    if w == "SET" && second_word_upper(sql) == "CONSTRAINTS" {
        return None;
    }
    match w.as_str() {
        "SET" | "RESET" | "DISCARD" | "DEALLOCATE" | "LOAD" | "CHECKPOINT" | "CLUSTER" | "ANALYZE" | "LOCK" => {
            return Some(ResultSet {
                command: w.into(),
                ..Default::default()
            });
        }
        "CLOSE" if second_word_upper(sql) == "ALL" => {
            return Some(ResultSet {
                command: "CLOSE CURSOR ALL".into(),
                ..Default::default()
            });
        }
        "SHOW" => return Some(show_result(sql)),
        "CREATE" | "DROP" => {
            // CREATE/DROP EXTENSION is ddl/objects.rs's: it takes only what is provided.
            // CREATE/DROP PROCEDURE/AGGREGATE: SQLite has no stored routines, but
            // accept the DDL so migrations proceed. FUNCTION is NOT here: it reaches
            // the engine, which stores LANGUAGE sql bodies (the session inlines them) and no-ops
            // other languages. The keyword is where the object's kind goes, never a
            // table or an index named so.
            let up = sql.to_uppercase();
            let lead = split_words(&up);
            let kind = match lead.get(1..) {
                Some(["OR", "REPLACE", kind, ..] | [kind, ..]) => *kind,
                _ => "",
            };
            if kind == "PROCEDURE" || kind == "AGGREGATE" {
                return Some(ResultSet {
                    command: (w + " " + kind).into(),
                    ..Default::default()
                });
            }
        }
        "ALTER"
            // Restore-time statements SQLite can't run: ownership changes, schema
            // alters, and constraints added to an existing table. Accepted as no-ops
            // so a pg_dump restores as far as SQLite allows (constraints not enforced).
            if alter_is_noop(sql) => {
                return Some(ResultSet {
                    command: ("ALTER ".to_string() + &second_word_upper(sql)).into(),
                    ..Default::default()
                });
            }
        _ => {}
    }
    None
}

/// Reports whether an ALTER statement is one we accept without effect (SQLite
/// can't apply it). ALTER TABLE ADD COLUMN etc. return false and pass to SQLite.
pub(crate) fn alter_is_noop(sql: &str) -> bool {
    let up = code_only(sql).to_uppercase();
    match second_word_upper(sql).as_str() {
        "SCHEMA" => true,
        "DATABASE" => false,
        // VALIDATE CONSTRAINT: a constraint here held for every row when it was added.
        "TABLE" => up.contains(" OWNER TO ") || up.contains(" ADD CONSTRAINT ") || up.contains(" VALIDATE CONSTRAINT "),
        // A function's owner decides what its SECURITY DEFINER body may read: GRANT's.
        "FUNCTION" | "ROUTINE" | "PROCEDURE" => false,
        _ => up.contains(" OWNER TO "),
    }
}

/// The upper-cased second whitespace-delimited word.
pub(crate) fn second_word_upper(sql: &str) -> String {
    let f = split_words(sql);
    if f.len() < 2 {
        return String::new();
    }
    trim_end_chars(f[1], ";").to_uppercase()
}

/// A statement the session runs on the transaction it owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TxControl {
    Begin,
    Commit,
    Rollback,
}

impl TxControl {
    /// The statement's kind, or None for anything else. "ROLLBACK TO [SAVEPOINT] x" is
    /// a savepoint op, not a full rollback.
    pub(crate) fn of(sql: &str) -> Option<TxControl> {
        if SavepointKind::of(sql).is_some() {
            return None;
        }
        match first_word_upper(sql).as_str() {
            "BEGIN" | "START" => Some(TxControl::Begin),
            "COMMIT" | "END" => Some(TxControl::Commit),
            "ROLLBACK" | "ABORT" => Some(TxControl::Rollback),
            _ => None,
        }
    }

    /// The command tag.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            TxControl::Begin => "BEGIN",
            TxControl::Commit => "COMMIT",
            TxControl::Rollback => "ROLLBACK",
        }
    }
}

/// A statement SQLite understands, which the session routes to the active
/// transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SavepointKind {
    Savepoint,
    Release,
    RollbackTo,
}

impl SavepointKind {
    /// The statement's kind, or None for anything else.
    pub(crate) fn of(sql: &str) -> Option<SavepointKind> {
        let f = split_words(sql);
        let first = f.first()?.to_uppercase();
        match first.as_str() {
            "SAVEPOINT" => Some(SavepointKind::Savepoint),
            "RELEASE" => Some(SavepointKind::Release),
            "ROLLBACK" | "ABORT" if f[1..].iter().any(|x| eq_fold(x, "to")) => Some(SavepointKind::RollbackTo),
            _ => None,
        }
    }

    /// The statement's name, which is also the command tag of all but ROLLBACK TO.
    pub(crate) fn name(self) -> &'static str {
        match self {
            SavepointKind::Savepoint => "SAVEPOINT",
            SavepointKind::Release => "RELEASE",
            SavepointKind::RollbackTo => "ROLLBACK TO",
        }
    }
}

/// Whether try_schema_ddl takes this statement.
pub(crate) fn is_schema_ddl(sql: &str) -> bool {
    let f = split_words(sql);
    f.len() >= 2
        && eq_fold(trim_end_chars(f[1], ";"), "schema")
        && ["CREATE", "DROP", "ALTER"].iter().any(|w| eq_fold(f[0], w))
}

impl Session {
    /// Handles SAVEPOINT/RELEASE/ROLLBACK TO on the active transaction. None means
    /// it was not a savepoint statement.
    /// ROLLBACK TO recovers an aborted transaction (clears the failed flag).
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_savepoint(&mut self, sql: &str) -> Option<Result<Command>> {
        let kind = SavepointKind::of(sql)?;
        if !self.in_tx() {
            return Some(Err(Error::with_state(
                SqlState::NO_ACTIVE_SQL_TRANSACTION,
                format!("{} can only be used in transaction blocks", kind.name()),
            )));
        }
        if kind == SavepointKind::RollbackTo {
            if let Err(e) = self.db.tx_execute(sql, &[], None) {
                return Some(Err(e.into()));
            }
            self.tx = TxState::Open; // recovered to the savepoint
            return Some(Ok(Command::from("ROLLBACK")));
        }
        if self.aborted_tx_error() {
            return Some(Err(Error::with_state(
                SqlState::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            )));
        }
        if let Err(e) = self.db.tx_execute(sql, &[], None) {
            self.mark_tx_failed();
            return Some(Err(e.into()));
        }
        Some(Ok(Command::from(kind.name())))
    }

    /// Handles CREATE/DROP/ALTER SCHEMA by delegating to the engine's SchemaManager.
    /// None when it did not recognize the statement.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_schema_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_schema_ddl(sql) {
            return None;
        }
        if self.in_tx() && !self.db.schema_ddl_transactional() {
            // In multi-file mode ATTACH/DETACH (which back schema create/drop) can't
            // run inside a SQLite transaction.
            return Some(Err(errf!(
                SqlState::ACTIVE_SQL_TRANSACTION,
                "CREATE/DROP SCHEMA cannot run inside a transaction block"
            )));
        }
        let r = self.run_schema_ddl(sql);
        if r.is_ok() {
            if self.in_tx() {
                self.schemas_dirty = true;
            } else {
                self.db.publish_schemas(true);
            }
        }
        Some(r)
    }

    fn run_schema_ddl(&mut self, sql: &str) -> Result<Command> {
        let mut words = ddl_words(sql)?;
        if words.last() == Some(&Word::Punct(';')) {
            words.pop();
        }
        // After the verb and SCHEMA.
        let rest = words.get(2..).unwrap_or(&[]);
        match words.first() {
            Some(verb) if verb.is("create") => {
                let (name, if_not_exists) = parse_create_schema(rest)?;
                if name.to_ascii_lowercase().starts_with("pg_") {
                    return Err(errf!(SqlState::RESERVED_NAME, "unacceptable schema name \"{name}\""));
                }
                self.db.create_schema(&name, if_not_exists)?;
                Ok(Command::from("CREATE SCHEMA"))
            }
            Some(verb) if verb.is("drop") => {
                let (names, if_exists, cascade) = parse_drop_schema(rest)?;
                for name in &names {
                    if !self.schema_exists(name)? {
                        if !if_exists {
                            return Err(errf!(SqlState::INVALID_SCHEMA_NAME, "schema \"{name}\" does not exist"));
                        }
                        self.c
                            .send_notice(&format!("schema \"{name}\" does not exist, skipping"))?;
                        continue;
                    }
                    self.db.drop_schema(name, if_exists, cascade)?;
                }
                self.rls_changed();
                Ok(Command::from("DROP SCHEMA"))
            }
            _ => {
                let Some(name) = rest.first().and_then(Word::name) else {
                    return Err(syntax_error_at(rest.first()));
                };
                if !self.schema_exists(name)? {
                    return Err(errf!(SqlState::INVALID_SCHEMA_NAME, "schema \"{name}\" does not exist"));
                }
                // ALTER SCHEMA x RENAME TO y
                if rest.get(1).is_some_and(|w| w.is("rename")) {
                    if !rest.get(2).is_some_and(|w| w.is("to")) {
                        return Err(syntax_error_at(rest.get(2)));
                    }
                    let Some(to) = rest.get(3).and_then(Word::name) else {
                        return Err(syntax_error_at(rest.get(3)));
                    };
                    self.db.rename_schema(name, to)?;
                    self.rls_changed();
                }
                Ok(Command::from("ALTER SCHEMA")) // OWNER TO: nothing to keep
            }
        }
    }

    fn schema_exists(&mut self, name: &str) -> Result<bool> {
        let q = format!(
            "SELECT 1 FROM pg_namespace WHERE lower(nspname) = lower({})",
            quote_literal(name)
        );
        Ok(!self.exec(&q, &[])?.rows.is_empty())
    }
}

/// `[IF NOT EXISTS] name [AUTHORIZATION role]`, or `AUTHORIZATION role`, which is the
/// name as well.
fn parse_create_schema(rest: &[Word]) -> Result<(String, bool)> {
    let if_not_exists = rest.len() >= 3 && rest[0].is("if") && rest[1].is("not") && rest[2].is("exists");
    let rest = &rest[if if_not_exists { 3 } else { 0 }..];
    let at = usize::from(rest.first().is_some_and(|w| w.is("authorization")));
    match rest.get(at).and_then(Word::name) {
        Some(name) => Ok((name.to_string(), if_not_exists)),
        None => Err(syntax_error_at(rest.get(at))),
    }
}

/// `[IF EXISTS] name [, ...] [CASCADE | RESTRICT]`.
fn parse_drop_schema(rest: &[Word]) -> Result<(Vec<String>, bool, bool)> {
    let if_exists = rest.len() >= 2 && rest[0].is("if") && rest[1].is("exists");
    let rest = &rest[if if_exists { 2 } else { 0 }..];
    let (names, cascade) = match rest.last() {
        Some(w) if w.is("cascade") => (&rest[..rest.len() - 1], true),
        Some(w) if w.is("restrict") => (&rest[..rest.len() - 1], false),
        _ => (rest, false),
    };
    let mut out = Vec::new();
    for at in (0..names.len()).step_by(2) {
        match names[at].name() {
            Some(name) => out.push(name.to_string()),
            None => return Err(syntax_error_at(names.get(at))),
        }
        if names.get(at + 1).is_some_and(|w| *w != Word::Punct(',')) {
            return Err(syntax_error_at(names.get(at + 1)));
        }
    }
    if out.is_empty() {
        return Err(syntax_error_at(None));
    }
    Ok((out, if_exists, cascade))
}

pub(crate) fn unquote_ident(s: &str) -> String {
    let s = trim_end_chars(s, ";");
    match s.strip_prefix('"').map(|q| q.strip_suffix('"').unwrap_or(q)) {
        Some(inside) => inside.replace("\"\"", "\""),
        None => s.strip_suffix('"').unwrap_or(s).to_string(),
    }
}

pub(crate) fn first_word_upper(sql: &str) -> String {
    let sql = trim_start_chars(sql, " \t\r\n(");
    let i = sql.find([' ', '\t', '\r', '\n', '(', ';']).unwrap_or(sql.len());
    sql[..i].to_uppercase()
}

/// The one-row result a SHOW returns, with the column named after the setting (as
/// PostgreSQL does).
fn show_result(sql: &str) -> ResultSet {
    let name = parse_show_name(sql);
    let value = show_value(&name);
    ResultSet {
        is_query: true,
        columns: vec![Column {
            name,
            decl_type: "text".to_string(),
        }],
        rows: vec![vec![Value::Text(value)]],
        rows_affected: 1,
        command: "SHOW".into(),
        ..Default::default()
    }
}

fn parse_show_name(sql: &str) -> String {
    let f = split_words(trim_end_chars(sql.trim(), ";"));
    if f.len() < 2 {
        return "?column?".to_string();
    }
    let rest = f[1..].join(" ").to_lowercase();
    if rest == "transaction isolation level" {
        return "transaction_isolation".to_string();
    }
    // The parameters Postgres names in mixed case.
    if rest == "timezone" || rest == "time zone" {
        return "TimeZone".to_string();
    }
    if rest == "datestyle" {
        return "DateStyle".to_string();
    }
    trim_end_chars(f[1], ";").to_lowercase()
}

/// A plausible value for a GUC. Unknown settings return an empty string, which is
/// what a client gets for an unset parameter.
fn show_value(name: &str) -> String {
    match name {
        "search_path" => "\"$user\", public",
        "server_version" => "16.0",
        "server_version_num" => "160000",
        "server_encoding" | "client_encoding" => "UTF8",
        "standard_conforming_strings" | "integer_datetimes" | "is_superuser" => "on",
        "transaction_isolation" | "default_transaction_isolation" => "read committed",
        "transaction_read_only" | "default_transaction_read_only" | "in_hot_standby" => "off",
        "datestyle" | "DateStyle" => "ISO, MDY",
        "timezone" | "TimeZone" => return overlite_engine::session_zone(),
        "max_index_keys" => "32",
        "max_identifier_length" => "63",
        "application_name" => "overlite",
        _ => "",
    }
    .to_string()
}
