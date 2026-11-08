//! Per-statement name resolution, run just before a statement reaches SQLite.

pub(crate) mod booleans;
pub(crate) mod current_database;
pub(crate) mod current_schema;
pub(crate) mod names;
pub(crate) mod public_qualifier;
pub(crate) mod regclass;
pub(crate) mod schema_qualify;
pub(crate) mod search_path;
pub(crate) mod temporal;
pub(crate) mod time_zone;
pub(crate) mod tsvector;
