//! What can go wrong reading a policy.

use std::fmt;

/// Why a pg_hba file could not be read. The text is what operators see in the log.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The file could not be read.
    Io(std::io::Error),
    /// A pg_hba.conf line is longer than the parser takes.
    LineTooLong,
    /// A pg_hba.conf line is not a rule.
    Rule { line: usize, problem: RuleProblem },
    /// A pg_hba.yaml file does not parse.
    Yaml(String),
}

/// What is wrong with one pg_hba.conf line.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuleProblem {
    /// A `local` line lacks a column.
    ShortLocal,
    /// A line of another type lacks a column. Holds the type.
    ShortHost(String),
}

/// The result of reading a policy.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::LineTooLong => f.write_str("bufio.Scanner: token too long"),
            Error::Rule { line, problem } => write!(f, "pg_hba.conf line {line}: {problem}"),
            Error::Yaml(msg) => f.write_str(msg),
        }
    }
}

impl fmt::Display for RuleProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleProblem::ShortLocal => f.write_str("local rule needs database, user, method"),
            RuleProblem::ShortHost(typ) => write!(f, "{typ} rule needs database, user, address, method"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl std::error::Error for RuleProblem {}

const _: () = {
    const fn check<T: Send + Sync + 'static>() {}
    check::<Error>();
};

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
