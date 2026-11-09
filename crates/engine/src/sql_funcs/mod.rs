//! LANGUAGE sql functions are executed by inlining: a call f(a, b) is replaced by the
//! function's body with the arguments substituted, wrapped in parentheses.
//! PL/pgSQL and other languages are accepted as a no-op.
//!
//! The engine stores the definitions and finds the calls ([`DbSession::function_calls`]);
//! it inlines nothing itself. The protocol layer does, before it runs a statement,
//! because what a body reads is held to the privileges and policies of the role the
//! body runs as (the caller's, or its owner's for SECURITY DEFINER), which only that
//! layer knows. A call the protocol layer has not inlined reaches SQLite as a call of
//! a function SQLite does not have, and fails.
//!
//! Definitions live in `_overlite_functions` (persistent, hidden). The names of the
//! functions a call may reach are kept per database ([`DbState`]), to find calls without
//! a query; the definitions are read from the table, so a transaction sees its own. A
//! function named as one SQLite has (a built-in, or a function of this engine's) is
//! stored but never inlined: a call of that name is the built-in's, as pg_catalog comes
//! first in Postgres.

pub(crate) mod parse;
pub(crate) mod rewrite;

use crate::db_state::DbState;
use crate::error::Result;
use crate::resolve::schema_qualify::lowercase;
use crate::sql_funcs::parse::{parse_create_function, parse_drop_function};
use crate::sql_funcs::rewrite::atoi_safe;
use crate::sqlite::{do_exec, do_query};
use overlite_core::sync::RwLockExt;
use overlite_core::text::{eq_fold, split_words, trim_start_chars};
use overlite_core::{ResultSet, Value};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, OnceLock, RwLock};

pub(crate) const FUNCTIONS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_functions (
  name    TEXT COLLATE NOCASE,
  arity   INTEGER,
  params  TEXT,
  body    TEXT,
  lang    TEXT,
  args    TEXT DEFAULT '',
  rettype TEXT DEFAULT '',
  src     TEXT DEFAULT '',
  owner   TEXT DEFAULT '',
  secdef  INTEGER DEFAULT 0,
  acl     TEXT DEFAULT NULL,
  PRIMARY KEY (name, arity)
)"#;

/// Migrate older tables (each errors harmlessly if present). A function from before
/// owners has none: like a table with no owner row, only a role that bypasses the
/// checks may change it. It was created SECURITY INVOKER whatever it said, which is how
/// it ran. acl NULL is the default ACL: EXECUTE for PUBLIC, and its owner.
pub(crate) static FUNCTIONS_ADD_COLS_DDL: [&str; 6] = [
    r#"ALTER TABLE _overlite_functions ADD COLUMN args TEXT DEFAULT ''"#,
    r#"ALTER TABLE _overlite_functions ADD COLUMN rettype TEXT DEFAULT ''"#,
    r#"ALTER TABLE _overlite_functions ADD COLUMN src TEXT DEFAULT ''"#,
    r#"ALTER TABLE _overlite_functions ADD COLUMN owner TEXT DEFAULT ''"#,
    r#"ALTER TABLE _overlite_functions ADD COLUMN secdef INTEGER DEFAULT 0"#,
    r#"ALTER TABLE _overlite_functions ADD COLUMN acl TEXT DEFAULT NULL"#,
];

#[derive(Clone, Debug, Default)]
pub(crate) struct SqlFunc {
    oid: i64, // rowid + FUNC_OID_BASE; how pg_proc/pg_get_functiondef key it
    name: String,
    arity: i64,
    params: Vec<String>, // parameter names ("" for an unnamed slot), len == arity
    body: String,        // the SQL body, e.g. "SELECT price * (1 + rate)"
    lang: String,
    /// SECURITY DEFINER, as CREATE FUNCTION said.
    secdef: bool,
    pub(crate) args: String,    // raw param list, e.g. "price numeric, rate numeric"
    pub(crate) rettype: String, // e.g. "numeric" or "TABLE(id int, sal int)"
    pub(crate) src: String,     // the original CREATE FUNCTION statement
}

/// Offsets a function's rowid into its catalog oid.
const FUNC_OID_BASE: i64 = 500000;

/// The definitions by oid, for pg_get_functiondef and its kin, which run as SQLite
/// functions and see no database: what they render is display only.
static FUNC_BY_OID: LazyLock<RwLock<HashMap<i64, Arc<SqlFunc>>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// The definition for a user-function oid.
pub(crate) fn func_by_oid_lookup(oid: i64) -> Option<Arc<SqlFunc>> {
    FUNC_BY_OID.read_locked().get(&oid).cloned()
}

/// The names, lower-cased, of every function SQLite has on a connection: its own and
/// this engine's, the same on every connection. Set once, by the first connection.
static BUILTINS: OnceLock<HashSet<String>> = OnceLock::new();

/// Notes the functions SQLite has on q, once its functions are registered.
pub(crate) fn note_builtins(q: &Connection) {
    if BUILTINS.get().is_some() {
        return;
    }
    let names = q
        .prepare("SELECT DISTINCT lower(name) FROM pragma_function_list")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<HashSet<_>>>()
        });
    if let Ok(names) = names {
        let _ = BUILTINS.set(names);
    }
}

/// Whether a call of name may reach a stored function: not when SQLite has a function
/// of that name, nor for the prefixes of the catalog's and this engine's.
fn inlinable(name: &str) -> bool {
    let low = lowercase(name);
    !(low.starts_with("pg_")
        || low.starts_with("overlite_")
        || BUILTINS.get().is_some_and(|b| b.contains(low.as_ref())))
}

fn set_function_cache(st: &DbState, fns: Vec<SqlFunc>) {
    let mut names = HashSet::with_capacity(fns.len());
    let mut by_oid = FUNC_BY_OID.write_locked();
    for f in fns {
        let f = Arc::new(f);
        if inlinable(&f.name) {
            names.insert(lowercase(&f.name).into_owned());
        }
        if f.oid != 0 {
            by_oid.insert(f.oid, f);
        }
    }
    drop(by_oid);
    *st.func_names.write_locked() = Arc::new(names);
}

/// Rebuilds the names from _overlite_functions. A failed read keeps them.
pub(crate) fn reload_functions(st: &DbState, q: &Connection) {
    let Ok(rs) = do_query(
        q,
        "SELECT rowid, name, arity, params, body, lang, args, rettype, src FROM _overlite_functions",
        &[],
        "",
    ) else {
        return;
    };
    let mut fns = Vec::new();
    for row in &rs.rows {
        let Some(f) = scan_func(row) else {
            return;
        };
        if eq_fold(&f.lang, "sql") {
            fns.push(f);
        }
    }
    set_function_cache(st, fns);
}

/// Scans one row of reload_functions; `None` is a Scan error (a NULL column).
fn scan_func(row: &[Value]) -> Option<SqlFunc> {
    let text = |i: usize| (!row[i].is_null()).then(|| row[i].to_text_lossy());
    let int = |i: usize| {
        (!row[i].is_null())
            .then(|| row[i].to_string().parse::<i64>().ok())
            .flatten()
    };
    let rowid = int(0)?;
    let mut f = SqlFunc {
        name: text(1)?,
        arity: int(2)?,
        ..Default::default()
    };
    let params = text(3)?;
    f.body = text(4)?;
    f.lang = text(5)?;
    f.args = text(6)?;
    f.rettype = text(7)?;
    f.src = text(8)?;
    f.oid = rowid + FUNC_OID_BASE;
    if params.is_empty() {
        f.params = vec![String::new(); f.arity as usize];
    } else {
        f.params = params.split('\x1f').map(str::to_string).collect();
    }
    Some(f)
}

/// Rebuilds the cache from rows of columns joined with char(30) (params itself
/// uses char(31) between names). Used by setup_connection, which only has a
/// single-column query helper.
pub(crate) fn refresh_functions_from_rows(st: &DbState, rows: &[String]) {
    let mut fns = Vec::new();
    for r in rows {
        let c: Vec<&str> = r.split('\x1e').collect();
        if c.len() < 9 || !eq_fold(c[5], "sql") {
            continue;
        }
        let mut f = SqlFunc {
            oid: atoi_safe(c[0]) + FUNC_OID_BASE,
            name: c[1].to_string(),
            arity: atoi_safe(c[2]),
            body: c[4].to_string(),
            lang: c[5].to_string(),
            args: c[6].to_string(),
            rettype: c[7].to_string(),
            src: c[8].to_string(),
            ..Default::default()
        };
        if c[3].is_empty() {
            f.params = vec![String::new(); f.arity as usize];
        } else {
            f.params = c[3].split('\x1f').map(str::to_string).collect();
        }
        fns.push(f);
    }
    set_function_cache(st, fns);
}

/// Handles CREATE/DROP FUNCTION. A LANGUAGE sql function is stored and cached;
/// any other language is accepted without storing (a no-op). Storage errors are
/// swallowed, so this never returns Err.
///
/// CREATE OR REPLACE keeps the function's owner and ACL, as Postgres does; the
/// protocol layer records the owner of a new one and has checked who may replace or
/// drop one.
pub(crate) fn try_function_ddl(st: &DbState, q: &Connection, sql: &str) -> Result<Option<ResultSet>> {
    let w = leading_word(sql);
    if eq_fold(w, "create") && has_function_keyword(sql) {
        if let Some(f) = parse_create_function(sql)
            && eq_fold(&f.lang, "sql")
            && !f.name.is_empty()
        {
            let params = f.params.join("\x1f");
            let args = [
                Value::Text(f.name),
                Value::Int(f.arity),
                Value::Text(params),
                Value::Text(f.body),
                Value::Text("sql".to_string()),
                Value::Text(f.args),
                Value::Text(f.rettype),
                Value::Text(sql.trim().to_string()),
                Value::Int(i64::from(f.secdef)),
            ];
            let r = do_exec(
                q,
                "INSERT INTO _overlite_functions (name, arity, params, body, lang, args, rettype, src, secdef) VALUES (?,?,?,?,?,?,?,?,?) \
                 ON CONFLICT (name, arity) DO UPDATE SET params = excluded.params, body = excluded.body, lang = excluded.lang, \
                 args = excluded.args, rettype = excluded.rettype, src = excluded.src, secdef = excluded.secdef",
                &args,
                "",
            );
            if r.is_ok() {
                reload_functions(st, q);
            }
        }
        return Ok(Some(ResultSet {
            command: "CREATE FUNCTION".into(),
            ..Default::default()
        }));
    }
    if eq_fold(w, "drop") && has_function_keyword(sql) {
        if let Some((name, arity)) = parse_drop_function(sql) {
            if arity >= 0 {
                let args = [Value::Text(name), Value::Int(arity)];
                let _ = do_exec(
                    q,
                    "DELETE FROM _overlite_functions WHERE name = ? AND arity = ?",
                    &args,
                    "",
                );
            } else {
                let args = [Value::Text(name)];
                let _ = do_exec(q, "DELETE FROM _overlite_functions WHERE name = ?", &args, "");
            }
            reload_functions(st, q);
        }
        return Ok(Some(ResultSet {
            command: "DROP FUNCTION".into(),
            ..Default::default()
        }));
    }
    Ok(None)
}

/// The function a CREATE or DROP FUNCTION names: its name, its arity (-1 for a DROP
/// without an argument list, which means every one of that name), and whether it is a
/// CREATE of a LANGUAGE sql function. None for any other statement.
pub fn function_ddl_target(sql: &str) -> Option<(String, i64, bool)> {
    let w = leading_word(sql);
    if !(eq_fold(w, "create") || eq_fold(w, "drop")) || !has_function_keyword(sql) {
        return None;
    }
    if eq_fold(w, "create") {
        let f = parse_create_function(sql)?;
        return Some((f.name, f.arity, eq_fold(&f.lang, "sql")));
    }
    if eq_fold(w, "drop") {
        let (name, arity) = parse_drop_function(sql)?;
        return Some((name, arity, false));
    }
    None
}

fn has_function_keyword(sql: &str) -> bool {
    let low = sql.to_lowercase();
    let f = split_words(&low);
    matches!(f.get(1..), Some(["function", ..] | ["or", "replace", "function", ..]))
}

fn leading_word(sql: &str) -> &str {
    let sql = trim_start_chars(sql, " \t\n\r(");
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() && !(b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') {
        i += 1;
    }
    &sql[..i]
}
