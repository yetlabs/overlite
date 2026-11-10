//! The storage engine: SQLite connections, the catalog emulated on top of them,
//! the functions Postgres has and SQLite lacks, and per-statement name
//! resolution.
//!
//! An [`Sqlite`] is one database; a [`DbSession`] is one client's connection to it, and
//! a [`Cluster`] is the set of databases of a process.
//!
//! ```
//! # fn main() -> overlite_engine::Result<()> {
//! use overlite_core::Value;
//! use overlite_engine::Sqlite;
//!
//! let db = Sqlite::open(":memory:")?;
//! let session = db.session()?;
//! session.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)", &[])?;
//! session.execute("INSERT INTO t VALUES ($1, $2)", &[Value::Int(1), "ann".into()])?;
//!
//! let rs = session.execute("SELECT name FROM t", &[])?;
//! assert_eq!(rs.rows, vec![vec![Value::Text("ann".to_string())]]);
//! # Ok(())
//! # }
//! ```

mod blank;
mod builtins;
pub(crate) mod catalog;
mod cluster;
mod column_types;
mod cursor;
mod db_state;
mod deferred;
mod error;
mod resolve;
mod sql_funcs;
mod sqlite;
mod storage;
mod table_types;

pub use blank::{blank_comments, blank_literals, code_only};
pub use builtins::arrays::pg_array_text;
pub use builtins::casts::{
    CastType, assign as assign_value, assignment_type, cast, interval_parts, timestamptz_in_session,
};
pub use builtins::collation::{
    CollationSpec, collation_known, collations_generation, forget_collation, register_collation, spec_of_locale,
};
pub use builtins::dates::{session_offset_at, session_zone, session_zone_is_utc, set_session_zone, take_zone_change};
pub use builtins::hstore::hstore_in;
pub use builtins::json::{Json, json_decode_first, json_marshal, json_unmarshal};
pub use builtins::ranges::{Range, range_parts, tstzrange_session_text};
pub use builtins::temporal::mark_transaction_start;
pub use catalog::constraints::{Identity, NOT_VALID_MARK, identity_columns, relation_name, unquote_sql_ident};
pub use catalog::declared_typmod;
pub use catalog::grants::{forget_table_rows_sql, move_table_rows_sql, set_session_role};
pub use catalog::schema::stored_table_name;
pub use catalog::visible::{
    QualifyQuery, SearchPathChange, ViewReader, parse_search_path, set_display_search_path, set_view_reader,
    take_search_path_change,
};
pub use cluster::{Cluster, DEFAULT_MAX_OPEN_DATABASES, open_cluster, single};
pub use error::{Error, ErrorKind, Result};
pub use resolve::TYPED_MARK;
pub use resolve::schema_qualify::split_stored_name;
pub use sql_funcs::function_ddl_target;
pub use sql_funcs::rewrite::{FunctionCall, inline_function_body};
pub use sqlite::{DbSession, InterruptHandle, Sqlite, ViewsAndTriggers};
pub use storage::{FORMAT_VERSION, backup_path};
pub use table_types::{ColumnType, TableTypes};
