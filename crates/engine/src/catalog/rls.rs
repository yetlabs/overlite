//! Row-level security is emulated: the flags and policies are recorded here and
//! the protocol layer injects each policy's USING/WITH CHECK expression into
//! statements that touch the table.

/// Per-table RLS flags (ENABLE / FORCE ROW LEVEL SECURITY). tablename here and in the
/// policies is the stored name: bare in public, "<schema>.<table>" elsewhere.
pub(crate) const RLS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_rls (
  tablename TEXT PRIMARY KEY COLLATE NOCASE,
  enabled   INTEGER DEFAULT 0,
  forced    INTEGER DEFAULT 0
)"#;

/// CREATE POLICY definitions. command is ALL/SELECT/INSERT/UPDATE/DELETE; roles
/// is a comma-separated lower-cased list ('' = to public); permissive is 1 for
/// PERMISSIVE, 0 for RESTRICTIVE.
pub(crate) const POLICIES_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_policies (
  polname    TEXT COLLATE NOCASE,
  tablename  TEXT COLLATE NOCASE,
  command    TEXT,
  roles      TEXT,
  permissive INTEGER DEFAULT 1,
  using_expr TEXT,
  check_expr TEXT
)"#;

/// Exists once a database's RLS rows hold stored names: see RLS_STORED_NAMES_UPGRADE.
pub(crate) const RLS_STORED_NAMES_MARK: &str = "_overlite_rls_stored_names";

/// Moves a database from an older server, which looked RLS up by bare name. A table
/// outside public was protected by a row of its bare name, as the statement wrote it
/// through the search_path, and so by any row of the public table of that name. A bare
/// name now means the public table alone, which would have opened those tables, so
/// each gets a copy of what protected it. Rows are added, not taken away: a flag it
/// already has can only be turned on. Once a row holds a stored name it means that
/// table, which is why this has to happen once, before anything writes the new form.
/// `"s1"."t"` was stored whole and matched nothing; it is the row it was meant to be.
pub(crate) const RLS_STORED_NAMES_UPGRADE: &str = r#"
UPDATE OR IGNORE _overlite_rls SET tablename = replace(tablename, '"."', '.') WHERE tablename LIKE '%"."%';
UPDATE _overlite_policies SET tablename = replace(tablename, '"."', '.') WHERE tablename LIKE '%"."%';
INSERT INTO _overlite_rls (tablename, enabled, forced)
  SELECT m.name, r.enabled, r.forced
  FROM _overlite_rls r
  JOIN _overlite_schemas s
  JOIN sqlite_master m ON m.type = 'table' AND lower(m.name) = lower(s.name || '.' || r.tablename)
  WHERE instr(r.tablename, '.') = 0
  ON CONFLICT (tablename) DO UPDATE SET enabled = max(enabled, excluded.enabled), forced = max(forced, excluded.forced);
INSERT INTO _overlite_policies (polname, tablename, command, roles, permissive, using_expr, check_expr)
  SELECT p.polname, m.name, p.command, p.roles, p.permissive, p.using_expr, p.check_expr
  FROM _overlite_policies p
  JOIN _overlite_schemas s
  JOIN sqlite_master m ON m.type = 'table' AND lower(m.name) = lower(s.name || '.' || p.tablename)
  WHERE instr(p.tablename, '.') = 0
    AND NOT EXISTS (SELECT 1 FROM _overlite_policies o WHERE o.tablename = m.name AND o.polname = p.polname);
"#;
