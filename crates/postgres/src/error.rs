//! What a connection can fail with.

use overlite_core::SqlState;
use overlite_engine::ErrorKind as EngineKind;
use std::fmt;

/// What can go wrong serving a connection or running a statement.
///
/// The text is what a client is sent, so it is the error's identity as far as
/// clients go; the kind says what the code can act on.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<std::io::Error>,
    /// Postgres' HINT, for an error that has one.
    hint: Option<String>,
    /// Where in the statement the error is, in characters from 1: the P field.
    position: Option<u32>,
}

/// The kinds of [`Error`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The engine reported it.
    Engine(EngineKind),
    /// The socket or a file failed.
    Io(std::io::ErrorKind),
    /// The origin knows the SQLSTATE.
    Statement(SqlState),
}

/// The result of an operation on a connection.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// An error whose SQLSTATE its origin knows.
    pub(crate) fn with_state(state: SqlState, message: impl Into<String>) -> Self {
        Error {
            kind: ErrorKind::Statement(state),
            message: message.into(),
            source: None,
            hint: None,
            position: None,
        }
    }

    /// This error with Postgres' HINT.
    #[must_use]
    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// This error pointing at a character of the statement, from 1.
    #[must_use]
    pub(crate) fn at_position(mut self, position: u32) -> Self {
        self.position = Some(position);
        self
    }

    /// The HINT a client is sent, if any.
    #[must_use]
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// The position a client is sent, if any.
    #[must_use]
    pub fn position(&self) -> Option<u32> {
        self.position
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

    /// The SQLSTATE a client is sent.
    #[must_use]
    pub fn sql_state(&self) -> SqlState {
        match self.kind {
            ErrorKind::Statement(state)
            | ErrorKind::Engine(EngineKind::Sqlite { state, .. } | EngineKind::State(state)) => state,
            ErrorKind::Engine(EngineKind::NoSuchDatabase) => SqlState::INVALID_CATALOG_NAME,
            ErrorKind::Engine(EngineKind::TooManyClients) => SqlState::TOO_MANY_CONNECTIONS,
            ErrorKind::Io(_) => SqlState::CONNECTION_FAILURE,
            ErrorKind::Engine(_) => SqlState::INTERNAL_ERROR,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|e| e as _)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error {
            kind: ErrorKind::Io(e.kind()),
            message: e.to_string(),
            source: Some(e),
            hint: None,
            position: None,
        }
    }
}

impl From<overlite_engine::Error> for Error {
    fn from(e: overlite_engine::Error) -> Self {
        Error {
            kind: ErrorKind::Engine(e.kind()),
            message: e.into_message(),
            source: None,
            hint: None,
            position: None,
        }
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
