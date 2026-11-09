//! DDL the session handles itself, because SQLite cannot run it as written.

pub(crate) mod alter;
pub(crate) mod alter_actions;
mod assign;
mod column_if_exists;
pub(crate) mod comment;
pub(crate) mod database;
mod defaults;
pub(crate) mod drop_constraint;
pub(crate) mod enums;
pub(crate) mod exclude;
pub(crate) mod identity;
pub(crate) mod objects;
pub(crate) mod sequences;
pub(crate) mod tree;
mod truncate;
