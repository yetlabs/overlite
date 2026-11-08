//! Host-based authentication: an ordered set of rules, read from pg_hba.conf or
//! its YAML equivalent, that decides how a connection authenticates.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use overlite_hba::{Conn, load};
//!
//! let dir = tempfile::tempdir()?;
//! std::fs::write(dir.path().join("pg_hba.conf"), "host all all 10.0.0.0/8 md5\n")?;
//!
//! let policy = load(dir.path().to_str().unwrap_or_default())?.expect("the file is there");
//! let conn = Conn {
//!     ip: "10.1.2.3".parse().ok(),
//!     database: "shop".to_string(),
//!     user: "app".to_string(),
//!     ..Default::default()
//! };
//! assert_eq!(policy.method(&conn).as_deref(), Some("md5"));
//! # Ok(())
//! # }
//! ```

mod error;
pub mod load;
pub(crate) mod parse;
mod policy;

pub use error::{Error, Result, RuleProblem};
pub use load::load;
pub use policy::{Conn, Policy};
