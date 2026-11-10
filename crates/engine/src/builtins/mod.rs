//! Rust implementations of the Postgres types and functions SQLite lacks.

pub(crate) mod arrays;
pub(crate) mod canonical;
pub(crate) mod casts;
pub mod collation;
pub(crate) mod contrib;
pub(crate) mod dates;
pub(crate) mod decimal;
pub(crate) mod fts;
pub(crate) mod hstore;
pub(crate) mod json;
pub(crate) mod like;
pub(crate) mod pgfuncs;
pub(crate) mod ranges;
pub(crate) mod regex;
pub(crate) mod srf;
pub(crate) mod temporal;
pub(crate) mod uuid;
