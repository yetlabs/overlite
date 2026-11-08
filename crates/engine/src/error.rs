//! What the engine reports.

use overlite_core::SqlState;
use rusqlite::ffi;
use std::fmt;

/// What can go wrong running a statement or managing a database.
///
/// The text is what a client is sent, so it is the error's identity as far as
/// clients go; the kind says what the code can act on.
#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

/// The kinds of [`Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// SQLite refused or failed: its extended result code and what that means, read
    /// once in `sqlite_state`.
    Sqlite { code: i32, state: SqlState },
    /// The origin knows the SQLSTATE.
    State(SqlState),
    /// The connection limit was reached.
    TooManyClients,
    /// A client asked for a database that is not served.
    NoSuchDatabase,
}

/// The result of an engine operation.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// An error of a known kind.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
        }
    }

    /// An error whose SQLSTATE its origin knows.
    pub fn with_state(state: SqlState, message: impl Into<String>) -> Self {
        Error::new(ErrorKind::State(state), message)
    }

    /// An error SQLite reported: its result code, its own words, and the text clients
    /// are sent.
    pub(crate) fn sqlite(code: i32, sqlite_says: &str, message: String) -> Self {
        let state = sqlite_state(code, sqlite_says);
        Error::new(ErrorKind::Sqlite { code, state }, message)
    }

    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The text a client is sent.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn into_message(self) -> String {
        self.message
    }

    /// The SQLSTATE a client is sent when the statement's route has none of its own.
    #[must_use]
    pub fn sql_state(&self) -> SqlState {
        match self.kind {
            ErrorKind::Sqlite { state, .. } | ErrorKind::State(state) => state,
            ErrorKind::NoSuchDatabase => SqlState::INVALID_CATALOG_NAME,
            ErrorKind::TooManyClients => SqlState::TOO_MANY_CONNECTIONS,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::with_state(SqlState::IO_ERROR, e.to_string())
    }
}

const _: () = {
    const fn check<T: Send + Sync + 'static>() {}
    check::<Error>();
};

/// Builds an [`Error`] from its SQLSTATE and a format string.
macro_rules! errf {
    ($state:expr, $($arg:tt)*) => { $crate::error::Error::with_state($state, format!($($arg)*)) };
}
pub(crate) use errf;

/// The SQLSTATE of what SQLite reported. The result code decides wherever it can, and
/// a name in the text never does. SQLITE_ERROR is the one code that covers everything
/// a statement can be refused for, so only there is the text read, and then by its
/// shape: the place of each phrase in the message, around the names SQLite quotes.
fn sqlite_state(code: i32, says: &str) -> SqlState {
    match code {
        ffi::SQLITE_CONSTRAINT_UNIQUE | ffi::SQLITE_CONSTRAINT_PRIMARYKEY | ffi::SQLITE_CONSTRAINT_ROWID => {
            return SqlState::UNIQUE_VIOLATION;
        }
        ffi::SQLITE_CONSTRAINT_NOTNULL => return SqlState::NOT_NULL_VIOLATION,
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => return SqlState::FOREIGN_KEY_VIOLATION,
        ffi::SQLITE_CONSTRAINT_CHECK => return SqlState::CHECK_VIOLATION,
        _ => {}
    }
    match code & 0xff {
        ffi::SQLITE_ERROR => compile_state(says),
        ffi::SQLITE_CONSTRAINT => SqlState::INTEGRITY_CONSTRAINT_VIOLATION,
        ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED => SqlState::LOCK_NOT_AVAILABLE,
        ffi::SQLITE_INTERRUPT => SqlState::QUERY_CANCELED,
        ffi::SQLITE_MISMATCH => SqlState::DATATYPE_MISMATCH,
        ffi::SQLITE_READONLY => SqlState::READ_ONLY_SQL_TRANSACTION,
        ffi::SQLITE_PERM | ffi::SQLITE_AUTH => SqlState::INSUFFICIENT_PRIVILEGE,
        ffi::SQLITE_NOMEM => SqlState::OUT_OF_MEMORY,
        ffi::SQLITE_FULL => SqlState::DISK_FULL,
        ffi::SQLITE_IOERR | ffi::SQLITE_CANTOPEN => SqlState::IO_ERROR,
        ffi::SQLITE_CORRUPT | ffi::SQLITE_NOTADB => SqlState::DATA_CORRUPTED,
        ffi::SQLITE_TOOBIG => SqlState::PROGRAM_LIMIT_EXCEEDED,
        ffi::SQLITE_RANGE => SqlState::PROTOCOL_VIOLATION,
        _ => SqlState::INTERNAL_ERROR,
    }
}

/// The two numbers of "table x has 2 values for 3 columns".
fn alias_count(says: &str) -> Option<(u32, u32)> {
    let (_, rest) = says.rsplit_once(" has ")?;
    let (have, rest) = rest.split_once(" values for ")?;
    Some((have.parse().ok()?, rest.strip_suffix(" columns")?.parse().ok()?))
}

/// What a statement SQLite refused to compile is refused for.
fn compile_state(says: &str) -> SqlState {
    /// What the message opens with, whatever name follows.
    const PREFIXES: &[(&str, SqlState)] = &[
        ("no such table: ", SqlState::UNDEFINED_TABLE),
        ("malformed JSON", SqlState::INVALID_TEXT_REPRESENTATION),
        ("no such view: ", SqlState::UNDEFINED_TABLE),
        ("no such column: ", SqlState::UNDEFINED_COLUMN),
        ("no such function: ", SqlState::UNDEFINED_FUNCTION),
        ("wrong number of arguments to function ", SqlState::UNDEFINED_FUNCTION),
        ("no such index: ", SqlState::UNDEFINED_OBJECT),
        ("no such trigger: ", SqlState::UNDEFINED_OBJECT),
        ("no such collation sequence: ", SqlState::UNDEFINED_OBJECT),
        ("no such savepoint: ", SqlState::INVALID_SAVEPOINT_SPECIFICATION),
        ("no such module: ", SqlState::FEATURE_NOT_SUPPORTED),
        ("unknown database ", SqlState::UNDEFINED_TABLE),
        ("duplicate column name: ", SqlState::DUPLICATE_COLUMN),
        ("ambiguous column name: ", SqlState::AMBIGUOUS_COLUMN),
        ("there is already ", SqlState::DUPLICATE_TABLE),
        ("unrecognized token: ", SqlState::SYNTAX_ERROR),
        ("incomplete input", SqlState::SYNTAX_ERROR),
        ("no tables specified", SqlState::SYNTAX_ERROR),
        ("all VALUES must have the same number of terms", SqlState::SYNTAX_ERROR),
        ("SELECTs to the left and right of ", SqlState::SYNTAX_ERROR),
        ("misuse of aggregate", SqlState::GROUPING_ERROR),
        ("misuse of window function ", SqlState::WINDOWING_ERROR),
        ("a GROUP BY clause is required", SqlState::GROUPING_ERROR),
        ("HAVING clause on a non-aggregate query", SqlState::GROUPING_ERROR),
        ("aggregate functions are not allowed", SqlState::GROUPING_ERROR),
        ("foreign key mismatch", SqlState::INVALID_FOREIGN_KEY),
        ("cannot commit", SqlState::NO_ACTIVE_SQL_TRANSACTION),
        ("cannot rollback", SqlState::NO_ACTIVE_SQL_TRANSACTION),
        ("cannot modify ", SqlState::WRONG_OBJECT_TYPE),
        ("too many columns", SqlState::TOO_MANY_COLUMNS),
        ("too many ", SqlState::PROGRAM_LIMIT_EXCEEDED),
        ("cannot add a ", SqlState::FEATURE_NOT_SUPPORTED),
        ("Cannot add a NOT NULL", SqlState::NOT_NULL_VIOLATION),
        ("Cannot add a ", SqlState::FEATURE_NOT_SUPPORTED),
        ("cannot drop ", SqlState::FEATURE_NOT_SUPPORTED),
        ("ON CONFLICT clause does not match", SqlState::INVALID_COLUMN_REFERENCE),
    ];
    if says.starts_with("near ") && says.ends_with(": syntax error") {
        return SqlState::SYNTAX_ERROR;
    }
    // The count of a derived table's alias list, against a CTE the rewrite wrote.
    if says.starts_with("table _alias")
        && let Some((have, want)) = alias_count(says)
    {
        return if have < want {
            SqlState::INVALID_COLUMN_REFERENCE
        } else {
            SqlState::FEATURE_NOT_SUPPORTED
        };
    }
    if says.ends_with(" columns") && says.contains(" values for ") {
        return SqlState::SYNTAX_ERROR;
    }
    // A table's message names the table first, so its name reads as anything: what
    // comes after it is the part to go by, and the last words of it at that.
    if let Some(rest) = says.strip_prefix("table ") {
        if rest.contains(" has no column named ") {
            return SqlState::UNDEFINED_COLUMN;
        }
        if rest.ends_with(" values were supplied") || rest.ends_with(" columns") {
            return SqlState::SYNTAX_ERROR;
        }
        if rest.ends_with(" already exists") {
            return SqlState::DUPLICATE_TABLE;
        }
        if rest.ends_with(" may not be dropped") || rest.ends_with(" may not be altered") {
            return SqlState::INSUFFICIENT_PRIVILEGE;
        }
    }
    if (says.starts_with("index ") || says.starts_with("view ")) && says.ends_with(" already exists") {
        return SqlState::DUPLICATE_TABLE;
    }
    if says.starts_with("trigger ") && says.ends_with(" already exists") {
        return SqlState::DUPLICATE_OBJECT;
    }
    if says.ends_with(" BY term out of range") || says.contains(" BY term out of range - ") {
        return SqlState::INVALID_COLUMN_REFERENCE;
    }
    if says.ends_with(" term does not match any column in the result set") {
        return SqlState::INVALID_COLUMN_REFERENCE;
    }
    if says.starts_with("circular reference") || says.ends_with("recursive reference in a subquery: ") {
        return SqlState::INVALID_RECURSION;
    }
    PREFIXES
        .iter()
        .find(|(start, _)| says.starts_with(start))
        .map_or(SqlState::INTERNAL_ERROR, |(_, state)| *state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_codes_decide() {
        for (code, says, want) in [
            (2067, "UNIQUE constraint failed: users.email", "23505"),
            (1555, "UNIQUE constraint failed: users.id", "23505"),
            (787, "FOREIGN KEY constraint failed", "23503"),
            (1299, "NOT NULL constraint failed: users.name", "23502"),
            (275, "CHECK constraint failed: x", "23514"),
            (5, "database is locked", "55P03"),
            (9, "interrupted", "57014"),
            (20, "datatype mismatch", "42804"),
            (1, "no such table: nope", "42P01"),
            (1, "no such column: nope", "42703"),
            (1, r#"near "selec": syntax error"#, "42601"),
            (1, "table users already exists", "42P07"),
            (1, "incomplete input", "42601"),
            (1, "something else", "XX000"),
            // A name is a name, whatever it says.
            (1, "no such table: interrupted", "42P01"),
            (1, "no such table: permission denied", "42P01"),
            (1, "no such table: table x already exists", "42P01"),
            (1, "no such column: syntax error", "42703"),
            (1, "table permission denied already exists", "42P07"),
            (1, "table t has no column named already exists", "42703"),
            (1, r#"near "already exists": syntax error"#, "42601"),
            (1, "no such function: division by zero", "42883"),
            (2067, "UNIQUE constraint failed: no such table.x", "23505"),
        ] {
            assert_eq!(sqlite_state(code, says).as_str(), want, "{code} {says}");
        }
    }
}
