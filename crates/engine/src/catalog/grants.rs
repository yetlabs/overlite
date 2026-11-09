//! Privilege enforcement is emulated: GRANT/REVOKE are recorded in
//! `_overlite_grants` and table ownership in `_overlite_owners`. The protocol
//! layer consults them before it runs a statement.

use crate::catalog::schema::stored_table_name;
use crate::error::{Error, Result, errf};
use overlite_core::text::quote_literal;
use overlite_core::{SqlState, Value};
use rusqlite::Connection;
use rusqlite::types::ValueRef;
use std::cell::RefCell;

/// One row per (grantee, table, privilege). grantee is a role name or the
/// literal "public"; privilege is SELECT/INSERT/UPDATE/DELETE/TRUNCATE or "ALL".
pub(crate) const GRANTS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_grants (
  grantee   TEXT COLLATE NOCASE,
  tablename TEXT COLLATE NOCASE,
  privilege TEXT COLLATE NOCASE
)"#;

/// One row per (grantee, table, column, privilege) of a column GRANT: privilege is
/// SELECT, INSERT, UPDATE or REFERENCES, never ALL, and columnname is the column's
/// name as the table spells it. Kept apart from `_overlite_grants`, whose every row
/// covers the whole table: a server from before column grants reads that table only,
/// and so sees none of these rather than taking them for table grants.
pub(crate) const COLUMN_GRANTS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_column_grants (
  grantee    TEXT COLLATE NOCASE,
  tablename  TEXT COLLATE NOCASE,
  columnname TEXT COLLATE NOCASE,
  privilege  TEXT COLLATE NOCASE
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
/// that name: its owner, its grants and column grants, its row-level security and policies, its nextval
/// defaults.
pub fn move_table_rows_sql(old: &str, new: &str) -> String {
    let (old, new) = (quote_literal(old), quote_literal(new));
    format!(
        "DELETE FROM _overlite_owners WHERE tablename = {new};
         UPDATE _overlite_owners SET tablename = {new} WHERE tablename = {old};
         UPDATE _overlite_grants SET tablename = {new} WHERE tablename = {old};
         UPDATE _overlite_column_grants SET tablename = {new} WHERE tablename = {old};
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
         DELETE FROM _overlite_column_grants WHERE tablename = {name};
         DELETE FROM _overlite_rls WHERE tablename = {name};
         DELETE FROM _overlite_policies WHERE tablename = {name};
         DELETE FROM _overlite_defaults WHERE tablename = {name}"
    )
}

std::thread_local! {
    /// The current role of the session this thread serves, for the functions below:
    /// a session runs on a thread of its own, and a SQLite function on the thread
    /// stepping its statement. Empty until the session sets it, which reads as a role
    /// unknown to `_overlite_roles`, one that bypasses the checks.
    static SESSION_ROLE: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Makes role the current role of the session on this thread, as has_*_privilege
/// and information_schema.column_privileges read it.
pub fn set_session_role(role: &str) {
    SESSION_ROLE.with(|r| role.clone_into(&mut r.borrow_mut()));
}

fn session_role() -> String {
    SESSION_ROLE.with(|r| r.borrow().clone())
}

/// The roles ?1 acts as: itself and those it inherits, transitively, lower-cased; as
/// the session's effective_roles reads them.
const EFFECTIVE_ROLES: &str = "WITH RECURSIVE eff(role) AS (
  SELECT lower(?1)
  UNION
  SELECT lower(m.roleof) FROM _overlite_memberships m
    JOIN eff e ON lower(m.member) = e.role
    JOIN _overlite_roles r ON lower(r.rolname) = e.role AND r.rolinherit <> 0
) ";

fn db_error(e: rusqlite::Error) -> Error {
    crate::sqlite::sqlite_error(e)
}

fn exists(db: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<bool> {
    let mut st = db.prepare(sql).map_err(db_error)?;
    st.exists(args).map_err(db_error)
}

/// Whether role skips privilege checks: a superuser, or a role unknown to
/// `_overlite_roles`, as the session's role_bypasses decides.
fn bypasses(db: &Connection, role: &str) -> Result<bool> {
    let mut st = db
        .prepare("SELECT rolsuper FROM _overlite_roles WHERE lower(rolname) = lower(?1)")
        .map_err(db_error)?;
    let mut rows = st.query([role]).map_err(db_error)?;
    let Some(row) = rows.next().map_err(db_error)? else {
        return Ok(true);
    };
    Ok(match row.get_ref(0).map_err(db_error)? {
        ValueRef::Integer(i) => i != 0,
        ValueRef::Real(f) => f != 0.0,
        ValueRef::Text(t) => !t.is_empty() && t != b"0" && !t.eq_ignore_ascii_case(b"false"),
        _ => false,
    })
}

/// What a privilege check asks about.
#[derive(Clone, Copy, Debug)]
enum Scope<'a> {
    Table,
    Column(&'a str),
    AnyColumn,
}

/// Whether role holds one of privs on table (a stored name) for scope, as the
/// session enforces it: bypass, ownership, a grant on the table (to the role, one it
/// inherits, or PUBLIC), or for a column scope a grant on that column or any.
/// with_grant_option asks for grant options, which only an owner holds here.
fn holds(
    db: &Connection,
    role: &str,
    table: &str,
    scope: Scope<'_>,
    privs: &[String],
    grant_option: bool,
) -> Result<bool> {
    if bypasses(db, role)? {
        return Ok(true);
    }
    let owner = [
        EFFECTIVE_ROLES,
        "SELECT 1 FROM _overlite_owners WHERE tablename = ?2 AND lower(owner) IN (SELECT role FROM eff)",
    ]
    .concat();
    if exists(db, &owner, &[&role, &table])? {
        return Ok(true);
    }
    if grant_option {
        return Ok(false);
    }
    let grantee = "AND (lower(grantee) IN (SELECT role FROM eff) OR lower(grantee) = 'public')";
    for p in privs {
        let on_table = [
            EFFECTIVE_ROLES,
            "SELECT 1 FROM _overlite_grants WHERE lower(tablename) = lower(?2) AND upper(privilege) IN (?3, 'ALL') ",
            grantee,
        ]
        .concat();
        if exists(db, &on_table, &[&role, &table, p])? {
            return Ok(true);
        }
        let held = match scope {
            Scope::Table => false,
            Scope::Column(c) => {
                let q = [
                    EFFECTIVE_ROLES,
                    "SELECT 1 FROM _overlite_column_grants WHERE tablename = ?2 AND privilege = ?3 AND columnname = ?4 ",
                    grantee,
                ]
                .concat();
                exists(db, &q, &[&role, &table, p, &c])?
            }
            Scope::AnyColumn => {
                let q = [
                    EFFECTIVE_ROLES,
                    "SELECT 1 FROM _overlite_column_grants WHERE tablename = ?2 AND privilege = ?3 ",
                    grantee,
                ]
                .concat();
                exists(db, &q, &[&role, &table, p])?
            }
        };
        if held {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The privileges of a has_*_privilege argument, `'SELECT, UPDATE WITH GRANT
/// OPTION'`, checked against those the object has, and whether grant options are
/// asked for.
fn privilege_list(arg: &Value, allowed: &[&str]) -> Result<(Vec<String>, bool)> {
    let Value::Text(s) = arg else {
        return Err(errf!(SqlState::INVALID_PARAMETER_VALUE, "unrecognized privilege type"));
    };
    let mut out = Vec::new();
    let mut grant_option = false;
    for item in s.split(',') {
        let mut p = item.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
        if let Some(rest) = p.strip_suffix(" WITH GRANT OPTION") {
            grant_option = true;
            p = rest.to_string();
        }
        if !allowed.contains(&p.as_str()) {
            return Err(errf!(
                SqlState::INVALID_PARAMETER_VALUE,
                "unrecognized privilege type: \"{}\"",
                item.trim()
            ));
        }
        out.push(p);
    }
    Ok((out, grant_option))
}

/// The role a has_*_privilege argument names: a name, or the oid of one.
fn role_arg(db: &Connection, arg: &Value) -> Result<Option<String>> {
    match arg {
        Value::Text(s) => Ok(Some(s.clone())),
        Value::Int(oid) => {
            let mut st = db
                .prepare("SELECT rolname FROM pg_roles WHERE oid = ?1")
                .map_err(db_error)?;
            let mut rows = st.query([oid]).map_err(db_error)?;
            Ok(match rows.next().map_err(db_error)? {
                Some(r) => Some(r.get::<_, String>(0).map_err(db_error)?),
                None => None,
            })
        }
        _ => Ok(None),
    }
}

/// Splits a relation name as regclass reads it: `s.t`, `"S"."T"`, `t`.
fn split_relation(s: &str) -> (String, String) {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = s.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            '.' if !quoted => {
                parts.push(if was_quoted { cur.clone() } else { cur.to_lowercase() });
                cur.clear();
                was_quoted = false;
            }
            c => cur.push(c),
        }
    }
    parts.push(if was_quoted { cur } else { cur.to_lowercase() });
    match parts.as_slice() {
        [t] => (String::new(), t.clone()),
        [.., s, t] => (s.clone(), t.clone()),
        [] => (String::new(), String::new()),
    }
}

/// The stored name of the table a has_*_privilege argument names: by name (as in
/// public when unqualified), 42P01 when there is none; or by oid, None when there is
/// none, where Postgres answers NULL.
fn table_arg(db: &Connection, arg: &Value) -> Result<Option<String>> {
    match arg {
        Value::Int(oid) => {
            let mut st = db
                .prepare(
                    "SELECT c.relname, n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = ?1",
                )
                .map_err(db_error)?;
            let mut rows = st.query([oid]).map_err(db_error)?;
            Ok(match rows.next().map_err(db_error)? {
                Some(r) => {
                    let rel: String = r.get(0).map_err(db_error)?;
                    let nsp: String = r.get(1).map_err(db_error)?;
                    Some(stored_table_name(&nsp, &rel))
                }
                None => None,
            })
        }
        Value::Text(s) => {
            let (schema, rel) = split_relation(s);
            let stored = stored_table_name(&schema, &rel);
            let there = exists(
                db,
                "SELECT 1 FROM main.sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
                &[&stored],
            )?;
            if !there {
                return Err(errf!(SqlState::UNDEFINED_TABLE, "relation \"{}\" does not exist", s));
            }
            Ok(Some(stored))
        }
        _ => Ok(None),
    }
}

/// The column of table a has_column_privilege argument names, as the table spells
/// it: by name, 42703 when there is none; or by number, None when there is none.
fn column_arg(db: &Connection, table: &str, arg: &Value) -> Result<Option<String>> {
    let cols: Vec<String> = {
        let mut st = db
            .prepare("SELECT name FROM pragma_table_xinfo(?1) WHERE hidden <> 1 ORDER BY cid")
            .map_err(db_error)?;
        st.query_map([table], |r| r.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_error)?
    };
    match arg {
        Value::Int(n) => Ok(usize::try_from(*n - 1).ok().and_then(|i| cols.get(i).cloned())),
        Value::Text(name) => {
            if let Some(c) = cols.iter().find(|c| c.eq_ignore_ascii_case(name)) {
                return Ok(Some(c.clone()));
            }
            let (_, relation) = crate::resolve::schema_qualify::split_stored_name(table);
            Err(errf!(
                SqlState::UNDEFINED_COLUMN,
                "column \"{name}\" of relation \"{relation}\" does not exist"
            ))
        }
        _ => Ok(None),
    }
}

const TABLE_PRIVILEGES: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "MAINTAIN",
];
const COLUMN_PRIVILEGES: &[&str] = &["SELECT", "INSERT", "UPDATE", "REFERENCES"];

/// has_table_privilege([user,] table, privilege).
pub(crate) fn has_table_privilege(db: &Connection, a: &[Value]) -> Result<Value> {
    let (user, rest) = match a {
        [t, p] => (Some(session_role()), [t, p]),
        [u, t, p] => (role_arg(db, u)?, [t, p]),
        _ => {
            return Err(errf!(
                SqlState::UNDEFINED_FUNCTION,
                "function has_table_privilege does not exist"
            ));
        }
    };
    let (privs, grant_option) = privilege_list(rest[1], TABLE_PRIVILEGES)?;
    let (Some(user), Some(table)) = (user, table_arg(db, rest[0])?) else {
        return Ok(Value::Null);
    };
    Ok(Value::Bool(holds(
        db,
        &user,
        &table,
        Scope::Table,
        &privs,
        grant_option,
    )?))
}

/// has_any_column_privilege([user,] table, privilege).
pub(crate) fn has_any_column_privilege(db: &Connection, a: &[Value]) -> Result<Value> {
    let (user, rest) = match a {
        [t, p] => (Some(session_role()), [t, p]),
        [u, t, p] => (role_arg(db, u)?, [t, p]),
        _ => {
            return Err(errf!(
                SqlState::UNDEFINED_FUNCTION,
                "function has_any_column_privilege does not exist"
            ));
        }
    };
    let (privs, grant_option) = privilege_list(rest[1], COLUMN_PRIVILEGES)?;
    let (Some(user), Some(table)) = (user, table_arg(db, rest[0])?) else {
        return Ok(Value::Null);
    };
    Ok(Value::Bool(holds(
        db,
        &user,
        &table,
        Scope::AnyColumn,
        &privs,
        grant_option,
    )?))
}

/// has_column_privilege([user,] table, column, privilege).
pub(crate) fn has_column_privilege(db: &Connection, a: &[Value]) -> Result<Value> {
    let (user, rest) = match a {
        [t, c, p] => (Some(session_role()), [t, c, p]),
        [u, t, c, p] => (role_arg(db, u)?, [t, c, p]),
        _ => {
            return Err(errf!(
                SqlState::UNDEFINED_FUNCTION,
                "function has_column_privilege does not exist"
            ));
        }
    };
    let (privs, grant_option) = privilege_list(rest[2], COLUMN_PRIVILEGES)?;
    let (Some(user), Some(table)) = (user, table_arg(db, rest[0])?) else {
        return Ok(Value::Null);
    };
    let Some(column) = column_arg(db, &table, rest[1])? else {
        return Ok(Value::Null);
    };
    Ok(Value::Bool(holds(
        db,
        &user,
        &table,
        Scope::Column(&column),
        &privs,
        grant_option,
    )?))
}

/// overlite_role_visible(role): whether the session's role acts as role, or
/// bypasses the checks; what information_schema.column_privileges shows a row for,
/// as Postgres' shows those of the roles the current one is a member of.
pub(crate) fn role_visible(db: &Connection, a: &[Value]) -> Result<Value> {
    let [Value::Text(name)] = a else {
        return Ok(Value::Bool(false));
    };
    let me = session_role();
    if bypasses(db, &me)? {
        return Ok(Value::Bool(true));
    }
    let q = [EFFECTIVE_ROLES, "SELECT 1 FROM eff WHERE role = lower(?2)"].concat();
    Ok(Value::Bool(exists(db, &q, &[&me, name])?))
}

/// overlite_aclitem(grantee, privileges, grantor): one element of an aclitem[] as
/// Postgres prints it, `carol=arw/postgres` (`=r/postgres` for PUBLIC), quoted as an
/// array element when it has to be. privileges is a comma list of SELECT, INSERT,
/// UPDATE and REFERENCES.
pub(crate) fn aclitem(a: &[Value]) -> Result<Value> {
    let [Value::Text(grantee), Value::Text(privs), Value::Text(grantor)] = a else {
        return Ok(Value::Null);
    };
    let privs: Vec<String> = privs.split(',').map(|p| p.trim().to_uppercase()).collect();
    let letters: String = [("INSERT", 'a'), ("SELECT", 'r'), ("UPDATE", 'w'), ("REFERENCES", 'x')]
        .iter()
        .filter(|(p, _)| privs.iter().any(|q| q == p))
        .map(|(_, l)| *l)
        .collect();
    let who = if grantee.eq_ignore_ascii_case("public") {
        String::new()
    } else {
        acl_name(grantee)
    };
    let item = format!("{who}={letters}/{}", acl_name(grantor));
    // As an element of the array's text: quoted when it holds what the array syntax uses.
    if item.chars().any(|c| matches!(c, '"' | '\\' | ',' | '{' | '}' | ' ')) {
        return Ok(Value::Text(format!(
            "\"{}\"",
            item.replace('\\', "\\\\").replace('"', "\\\"")
        )));
    }
    Ok(Value::Text(item))
}

/// A role name in an aclitem: bare when it is letters, digits and underscores
/// (Postgres' putid), else in double quotes.
fn acl_name(name: &str) -> String {
    if name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}
