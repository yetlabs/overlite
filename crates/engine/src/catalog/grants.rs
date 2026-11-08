//! Privilege enforcement is emulated: GRANT/REVOKE are recorded in
//! `_overlite_grants` and table ownership in `_overlite_owners`. The protocol
//! layer consults them before it runs a statement.

use overlite_core::text::quote_literal;

/// One row per (grantee, table, privilege). grantee is a role name or the
/// literal "public"; privilege is SELECT/INSERT/UPDATE/DELETE/TRUNCATE or "ALL".
pub(crate) const GRANTS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_grants (
  grantee   TEXT COLLATE NOCASE,
  tablename TEXT COLLATE NOCASE,
  privilege TEXT COLLATE NOCASE
)"#;

/// The role that created each table (its owner, who holds all privileges
/// implicitly and alone may drop/alter it). tablename here and in the grants is the
/// stored name: bare in public, "<schema>.<table>" elsewhere.
pub(crate) const OWNERS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_owners (
  tablename TEXT PRIMARY KEY COLLATE NOCASE,
  owner     TEXT COLLATE NOCASE
)"#;

/// Role membership (GRANT role TO role): member is a member of roleof, and
/// inherits its privileges when member has INHERIT. admin_option marks WITH
/// ADMIN OPTION.
pub(crate) const MEMBERSHIPS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_memberships (
  member       TEXT COLLATE NOCASE,
  roleof       TEXT COLLATE NOCASE,
  admin_option INTEGER DEFAULT 0
)"#;

/// Adds admin_option to membership tables created before WITH ADMIN OPTION
/// existed; it errors (harmlessly, ignored) when present.
pub(crate) const MEMBERSHIPS_ADD_ADMIN_DDL: &str =
    r#"ALTER TABLE _overlite_memberships ADD COLUMN admin_option INTEGER DEFAULT 0"#;

/// Moves what is recorded about a table to its new stored name, over anything left at
/// that name: its owner, its grants, its row-level security and policies, its nextval
/// defaults.
pub fn move_table_rows_sql(old: &str, new: &str) -> String {
    let (old, new) = (quote_literal(old), quote_literal(new));
    format!(
        "DELETE FROM _overlite_owners WHERE tablename = {new};
         UPDATE _overlite_owners SET tablename = {new} WHERE tablename = {old};
         UPDATE _overlite_grants SET tablename = {new} WHERE tablename = {old};
         DELETE FROM _overlite_rls WHERE tablename = {new};
         UPDATE _overlite_rls SET tablename = {new} WHERE tablename = {old};
         DELETE FROM _overlite_policies WHERE tablename = {new};
         UPDATE _overlite_policies SET tablename = {new} WHERE tablename = {old};
         DELETE FROM _overlite_defaults WHERE tablename = {new};
         UPDATE _overlite_defaults SET tablename = {new} WHERE tablename = {old}"
    )
}

/// Removes what is recorded about a table.
pub fn forget_table_rows_sql(name: &str) -> String {
    let name = quote_literal(name);
    format!(
        "DELETE FROM _overlite_owners WHERE tablename = {name};
         DELETE FROM _overlite_grants WHERE tablename = {name};
         DELETE FROM _overlite_rls WHERE tablename = {name};
         DELETE FROM _overlite_policies WHERE tablename = {name};
         DELETE FROM _overlite_defaults WHERE tablename = {name}"
    )
}
