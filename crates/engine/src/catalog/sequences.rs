//! Sequences are stored in `_overlite_sequences` in the main database. The
//! protocol layer implements CREATE/ALTER/DROP SEQUENCE plus nextval/currval/
//! setval/lastval against this table, expanding each call to a concrete integer
//! before it reaches SQLite. last_value holds the current value; is_called
//! distinguishes "not yet advanced" from "advanced". min_value/max_value are
//! materialized at CREATE time.

/// Records `DEFAULT nextval('seq')` column defaults (which SQLite can't
/// express), so the protocol can inject the value on insert. tablename is the stored
/// name: bare in public, "<schema>.<table>" elsewhere.
pub(crate) const COLUMN_DEFAULTS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_defaults (
  tablename TEXT COLLATE NOCASE,
  colname   TEXT COLLATE NOCASE,
  seqname   TEXT
)"#;

/// Exists once a database's default rows hold stored names: see
/// DEFAULTS_STORED_NAMES_UPGRADE.
pub(crate) const DEFAULTS_STORED_NAMES_MARK: &str = "_overlite_defaults_stored_names";

/// Moves a database from an older server, which looked defaults up by bare name. A
/// table created through the search_path left a bare row, which reached every table of
/// that name, and a second such table left another row for the same column, of which
/// the first won. A bare name now means the public table alone, so each namesake with
/// that column gets a copy of the row that served it. Losing one would turn generated
/// keys into NULLs without a word; keeping it shares a sequence, and the values stay
/// unique. A row spelled with a schema matched nothing then: it becomes the row it was
/// meant to be, so such a column starts to get its default.
pub(crate) const DEFAULTS_STORED_NAMES_UPGRADE: &str = r#"
UPDATE _overlite_defaults SET tablename = replace(tablename, '"."', '.') WHERE tablename LIKE '%"."%';
UPDATE _overlite_defaults SET tablename = substr(tablename, 8) WHERE lower(tablename) LIKE 'public.%';
DELETE FROM _overlite_defaults WHERE rowid NOT IN
  (SELECT min(rowid) FROM _overlite_defaults GROUP BY lower(tablename), lower(colname));
INSERT INTO _overlite_defaults (tablename, colname, seqname)
  SELECT m.name, d.colname, d.seqname
  FROM _overlite_defaults d
  JOIN _overlite_schemas s
  JOIN sqlite_master m ON m.type = 'table' AND lower(m.name) = lower(s.name || '.' || d.tablename)
  JOIN pragma_table_info(m.name) c ON lower(c.name) = lower(d.colname)
  WHERE instr(d.tablename, '.') = 0
    AND NOT EXISTS (SELECT 1 FROM _overlite_defaults o WHERE o.tablename = m.name AND o.colname = d.colname);
"#;

pub(crate) const SEQUENCES_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_sequences (
  seqname     TEXT PRIMARY KEY COLLATE NOCASE,
  last_value  INTEGER NOT NULL DEFAULT 1,
  increment   INTEGER NOT NULL DEFAULT 1,
  min_value   INTEGER NOT NULL DEFAULT 1,
  max_value   INTEGER NOT NULL DEFAULT 9223372036854775807,
  start_value INTEGER NOT NULL DEFAULT 1,
  cache_size  INTEGER NOT NULL DEFAULT 1,
  is_cycled   INTEGER NOT NULL DEFAULT 0,
  is_called   INTEGER NOT NULL DEFAULT 0
)"#;
