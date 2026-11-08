//! The PostgreSQL side of overlite: the wire protocol, sessions, authentication,
//! and the translation of Postgres SQL to SQLite.
//!
//! A [`Server`] accepts connections for a [`Protocol`] over an engine `Cluster`:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use overlite_postgres::{Protocol, Server};
//!
//! let cluster = overlite_engine::open_cluster("data/app.db", 0)?;
//! let mut server = Server::new("127.0.0.1:5432", Protocol::from_env(), cluster)?;
//! server.serve()?;
//! # Ok(())
//! # }
//! ```
//!
//! SQL goes through [`rewrite`], whose fast path is [`needs_no_rewrite`]:
//!
//! ```
//! use overlite_postgres::{needs_no_rewrite, rewrite};
//!
//! assert!(needs_no_rewrite("SELECT id FROM t WHERE id = 1"));
//! assert_eq!(
//!     rewrite("SELECT a FROM t WHERE a ~ 'x'"),
//!     "SELECT a FROM t WHERE a REGEXP 'x'"
//! );
//! ```

mod ast;
mod auth;
mod ddl;
mod error;
mod protocol;
pub(crate) mod rewrite;
mod server;
mod session;
mod types;
mod wire;

/// The `log` target of the two lines that have always gone out without a timestamp.
pub const UNSTAMPED_LOG_TARGET: &str = "overlite::unstamped";

pub use error::{Error, ErrorKind, Result};
pub use protocol::Protocol;
pub use rewrite::{needs_no_rewrite, rewrite, rewrite_full};
pub use server::Server;
