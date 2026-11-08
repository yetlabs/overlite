//! COMMENT ON stores object comments that SQLite has no place for, in
//! `_overlite_comments` (hidden by the `_overlite_*` GLOB filter), surfaced
//! through the `pg_description` view. objkind is 'table'/'view'/'index'/
//! 'sequence'/'column'/...; objname is the relation (or type/schema) name;
//! subname is the column name for column comments (empty otherwise). A removed
//! comment is stored as a deletion, so the table only holds live comments.

/// The extensions CREATE EXTENSION has made: pg_extension lists them.
pub(crate) const EXTENSIONS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_extensions (
  extname TEXT PRIMARY KEY,
  extversion TEXT
)"#;

/// The collations CREATE COLLATION has made: pg_collation lists them, and each
/// connection registers them.
pub(crate) const COLLATIONS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_collations (
  collname TEXT PRIMARY KEY,
  provider TEXT,
  locale TEXT,
  deterministic INTEGER
)"#;

pub(crate) const COMMENTS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_comments (
  objkind TEXT COLLATE NOCASE,
  objname TEXT COLLATE NOCASE,
  subname TEXT COLLATE NOCASE DEFAULT '',
  comment TEXT
)"#;
