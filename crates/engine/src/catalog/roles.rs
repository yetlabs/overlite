//! Roles are stored in `_overlite_roles` in the main database, which exists to
//! make CREATE/ALTER/DROP ROLE and \du behave.

use crate::catalog::schema::CATALOG_ROLE;
use overlite_core::text::quote_literal;

pub(crate) const ROLES_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_roles (
  rolname       TEXT PRIMARY KEY COLLATE NOCASE,
  rolsuper      INTEGER DEFAULT 0,
  rolinherit    INTEGER DEFAULT 1,
  rolcreaterole INTEGER DEFAULT 0,
  rolcreatedb   INTEGER DEFAULT 0,
  rolcanlogin   INTEGER DEFAULT 0,
  rolreplication INTEGER DEFAULT 0,
  rolbypassrls  INTEGER DEFAULT 0,
  rolpassword   TEXT
)"#;

/// Adds rolpassword to tables created before per-role passwords existed; it
/// errors (harmlessly, ignored) when the column is present.
pub(crate) const ROLES_ADD_PASSWORD_DDL: &str = r#"ALTER TABLE _overlite_roles ADD COLUMN rolpassword TEXT"#;

/// Inserts the configured default role (a superuser that can log in) if it isn't
/// already present.
pub(crate) fn seed_default_role_sql() -> String {
    [
        r#"INSERT OR IGNORE INTO _overlite_roles
	 (rolname, rolsuper, rolinherit, rolcreaterole, rolcreatedb, rolcanlogin, rolbypassrls)
	 VALUES ("#,
        &quote_literal(&CATALOG_ROLE),
        r#", 1, 1, 1, 1, 1, 1)"#,
    ]
    .concat()
}
