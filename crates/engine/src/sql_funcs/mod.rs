//! LANGUAGE sql functions are executed by inlining: a call f(a, b) is rewritten
//! into the function's body with the arguments substituted, wrapped in
//! parentheses. PL/pgSQL and other languages are accepted as a no-op.
//!
//! Definitions live in `_overlite_functions` (persistent, hidden) and are cached
//! in memory for the rewrite; the cache is reloaded on CREATE/DROP FUNCTION and
//! per connection.

pub(crate) mod parse;
pub(crate) mod rewrite;

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
use std::sync::{Arc, LazyLock, RwLock};

pub(crate) const FUNCTIONS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_functions (
  name    TEXT COLLATE NOCASE,
  arity   INTEGER,
  params  TEXT,
  body    TEXT,
  lang    TEXT,
  args    TEXT DEFAULT '',
  rettype TEXT DEFAULT '',
  src     TEXT DEFAULT '',
  PRIMARY KEY (name, arity)
)"#;

/// Migrate older tables (each errors harmlessly if present).
pub(crate) static FUNCTIONS_ADD_COLS_DDL: [&str; 3] = [
    r#"ALTER TABLE _overlite_functions ADD COLUMN args TEXT DEFAULT ''"#,
    r#"ALTER TABLE _overlite_functions ADD COLUMN rettype TEXT DEFAULT ''"#,
    r#"ALTER TABLE _overlite_functions ADD COLUMN src TEXT DEFAULT ''"#,
];

#[derive(Clone, Debug, Default)]
pub(crate) struct SqlFunc {
    oid: i64, // rowid + FUNC_OID_BASE; how pg_proc/pg_get_functiondef key it
    name: String,
    arity: i64,
    params: Vec<String>, // parameter names ("" for an unnamed slot), len == arity
    body: String,        // the SQL body, e.g. "SELECT price * (1 + rate)"
    lang: String,
    pub(crate) args: String,    // raw param list, e.g. "price numeric, rate numeric"
    pub(crate) rettype: String, // e.g. "numeric" or "TABLE(id int, sal int)"
    pub(crate) src: String,     // the original CREATE FUNCTION statement
}

/// Offsets a function's rowid into its catalog oid.
const FUNC_OID_BASE: i64 = 500000;

#[derive(Debug, Default)]
struct FuncCache {
    func_cache: HashMap<String, Arc<SqlFunc>>, // key: lower(name)+"/"+arity
    func_names: HashSet<String>,               // lower(name) present, for a quick scan guard
    func_by_oid: HashMap<i64, Arc<SqlFunc>>,   // oid -> def, for pg_get_function*
}

static FUNC_CACHE: LazyLock<RwLock<FuncCache>> = LazyLock::new(|| RwLock::new(FuncCache::default()));

/// The definition for a user-function oid.
pub(crate) fn func_by_oid_lookup(oid: i64) -> Option<Arc<SqlFunc>> {
    FUNC_CACHE.read_locked().func_by_oid.get(&oid).cloned()
}

fn func_key(name: &str, arity: i64) -> String {
    format!("{}/{}", lowercase(name), arity)
}

fn set_function_cache(fns: Vec<SqlFunc>) {
    let mut m = HashMap::with_capacity(fns.len());
    let mut names = HashSet::with_capacity(fns.len());
    let mut by_oid = HashMap::with_capacity(fns.len());
    for f in fns {
        let f = Arc::new(f);
        m.insert(func_key(&f.name, f.arity), f.clone());
        names.insert(lowercase(&f.name).into_owned());
        if f.oid != 0 {
            by_oid.insert(f.oid, f);
        }
    }
    *FUNC_CACHE.write_locked() = FuncCache {
        func_cache: m,
        func_names: names,
        func_by_oid: by_oid,
    };
}

fn lookup_func(name: &str, arity: i64) -> Option<Arc<SqlFunc>> {
    FUNC_CACHE.read_locked().func_cache.get(&func_key(name, arity)).cloned()
}

fn any_func_named(name: &str) -> bool {
    FUNC_CACHE.read_locked().func_names.contains(lowercase(name).as_ref())
}

fn have_functions() -> bool {
    !FUNC_CACHE.read_locked().func_cache.is_empty()
}

/// Rebuilds the cache from _overlite_functions. A failed read keeps the cache.
fn reload_functions(q: &Connection) {
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
    set_function_cache(fns);
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
pub(crate) fn refresh_functions_from_rows(rows: &[String]) {
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
    set_function_cache(fns);
}

/// Handles CREATE/DROP FUNCTION. A LANGUAGE sql function is stored and cached;
/// any other language is accepted without storing (a no-op). Storage errors are
/// swallowed, so this never returns Err.
pub(crate) fn try_function_ddl(q: &Connection, sql: &str) -> Result<Option<ResultSet>> {
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
            ];
            let r = do_exec(
                q,
                "INSERT OR REPLACE INTO _overlite_functions (name, arity, params, body, lang, args, rettype, src) VALUES (?,?,?,?,?,?,?,?)",
                &args,
                "",
            );
            if r.is_ok() {
                reload_functions(q);
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
            reload_functions(q);
        }
        return Ok(Some(ResultSet {
            command: "DROP FUNCTION".into(),
            ..Default::default()
        }));
    }
    Ok(None)
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
