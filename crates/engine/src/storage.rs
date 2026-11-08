//! The storage format: how a value of each Postgres type is kept in a SQLite file,
//! and the conversion of a file that holds other forms, once, when it is opened.
//!
//! ARCHITECTURE.md, "Storage format", has the table. A file carries the version of the
//! format it holds in `_overlite_storage_format`. One without it, or with an older
//! version, is copied beside itself with SQLite's backup API, then every value of a
//! column of a known type that is not in its canonical form is rewritten to it, in one
//! transaction that also writes the version: a crash leaves the file as it was, and the
//! next open does it again. A value its type cannot read is left as it is, and its
//! column is listed in `_overlite_mixed_columns`, the only columns the readings of
//! resolve/booleans.rs still take both forms in. Each open tries those again.

use crate::builtins::arrays::array_as_json;
use crate::builtins::casts::{CastType, assign, assignment_type, cast};
use crate::builtins::json::json_is_valid;
use crate::error::errf;
use crate::error::{Error, Result};
use crate::sqlite::sqlite_error;
use overlite_core::text::{quote_ident, quote_literal};
use overlite_core::{SqlState, Value, parse_bool};
use rusqlite::{Connection, OpenFlags, ffi};
use std::ffi::CString;
use std::time::Instant;

/// The version of the storage format this server writes.
pub const FORMAT_VERSION: i64 = 1;

/// The version of the format a file holds: one row.
const FORMAT_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS _overlite_storage_format (version INTEGER NOT NULL)";
/// The columns holding a value their type cannot read, which keep both readings.
const MIXED_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS _overlite_mixed_columns (
  tablename TEXT NOT NULL COLLATE NOCASE,
  colname   TEXT NOT NULL COLLATE NOCASE,
  decl      TEXT NOT NULL,
  invalid   INTEGER NOT NULL,
  PRIMARY KEY (tablename, colname)
)";

/// Rows a step of the conversion covers, by rowid, between two lines of progress.
const CHUNK: i64 = 500_000;

/// What a stored value is to its column's type.
#[derive(Debug, PartialEq)]
pub(crate) enum Canon {
    /// Already in the canonical form, or NULL.
    Same,
    /// A value of the type in another form: this is its canonical one.
    To(Value),
    /// Not a value of the type at all: it stays as it is.
    Invalid,
}

/// A declared type whose values have one stored form, and how to reach it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    /// INTEGER 1 or 0.
    Bool,
    /// The text the type's input function gives: date, time, timetz, timestamp,
    /// timestamptz, interval, uuid.
    Input(CastType),
    /// What an assignment makes of a value: numeric(p,s), varchar(n), char(n).
    Assign(String),
    /// A BLOB.
    Bytea,
    /// Valid JSON text, as written.
    Json,
    /// A JSON array.
    Array(String),
}

/// The kind of a declared type, None for one stored as it comes (integers, floats,
/// text, enums, hstore and the types kept as text).
pub(crate) fn kind_of(decl: &str) -> Option<Kind> {
    let d = decl
        .trim()
        .trim_matches('"')
        .to_ascii_lowercase()
        .replace("decimaltext", "numeric");
    if d == "boolean" || d == "bool" {
        return Some(Kind::Bool);
    }
    if let Some(element) = d.strip_suffix("[]") {
        return Some(Kind::Array(element.trim().to_string()));
    }
    let ty = CastType::parse(&d)?;
    Some(match ty {
        CastType::Date
        | CastType::Time(_)
        | CastType::TimeTz(_)
        | CastType::Timestamp(_)
        | CastType::TimestampTz(_)
        | CastType::Interval
        | CastType::Uuid => Kind::Input(ty),
        CastType::Numeric(Some(_)) | CastType::Varchar(Some(_)) | CastType::Char(_) => {
            Kind::Assign(assignment_type(&d).unwrap_or(d))
        }
        CastType::Bytea => Kind::Bytea,
        CastType::Json | CastType::Jsonb => Kind::Json,
        _ => return None,
    })
}

/// What v is to a column of kind.
pub(crate) fn canonical(v: &Value, kind: &Kind) -> Canon {
    match (kind, v) {
        (_, Value::Null) => Canon::Same,
        (Kind::Bool, Value::Int(0 | 1)) => Canon::Same,
        (Kind::Bool, Value::Float(f)) if f.to_bits() == 0f64.to_bits() || f.to_bits() == 1f64.to_bits() => {
            Canon::To(Value::Int(i64::from(f.to_bits() == 1f64.to_bits())))
        }
        (Kind::Bool, Value::Text(s)) => match parse_bool(s) {
            Some(b) => Canon::To(Value::Int(i64::from(b))),
            None => Canon::Invalid,
        },
        (Kind::Bool, _) => Canon::Invalid,
        (Kind::Input(ty), Value::Text(s)) => match cast(v, ty) {
            Ok(Value::Text(c)) if &c == s => Canon::Same,
            Ok(c @ Value::Text(_)) => Canon::To(c),
            _ => Canon::Invalid,
        },
        (Kind::Input(_), _) => Canon::Invalid,
        (Kind::Assign(decl), Value::Text(_)) => match assign(v, decl) {
            Ok(c) if &c == v => Canon::Same,
            Ok(c) => Canon::To(c),
            Err(_) => Canon::Invalid,
        },
        (Kind::Assign(_), _) => Canon::Invalid,
        (Kind::Bytea, Value::Blob(_)) => Canon::Same,
        (Kind::Bytea, Value::Text(_)) => match cast(v, &CastType::Bytea) {
            Ok(b @ Value::Blob(_)) => Canon::To(b),
            _ => Canon::Invalid,
        },
        (Kind::Bytea, _) => Canon::Invalid,
        (Kind::Json, Value::Text(s)) if json_is_valid(s) => Canon::Same,
        (Kind::Json, _) => Canon::Invalid,
        (Kind::Array(element), Value::Text(s)) => {
            let t = s.trim();
            let json = if t.starts_with('[') {
                if !json_is_valid(t) {
                    return Canon::Invalid;
                }
                v.clone()
            } else if t.starts_with('{') && t.ends_with('}') && !t[1..t.len() - 1].contains(['{', '}']) {
                // Postgres' text form of a one-dimensional array.
                array_as_json(v)
            } else {
                return Canon::Invalid;
            };
            let fitted = match assignment_type(element) {
                Some(e) => match assign(&json, &format!("{e}[]")) {
                    Ok(f) => f,
                    Err(_) => return Canon::Invalid,
                },
                None => json,
            };
            if &fitted == v { Canon::Same } else { Canon::To(fitted) }
        }
        (Kind::Array(_), _) => Canon::Invalid,
    }
}

/// overlite_storage_state(v, decl): 0 canonical, 1 convertible, 2 not of the type.
pub(crate) fn storage_state(v: &Value, decl: &str) -> i64 {
    match kind_of(decl).map_or(Canon::Same, |k| canonical(v, &k)) {
        Canon::Same => 0,
        Canon::To(_) => 1,
        Canon::Invalid => 2,
    }
}

/// overlite_storage_canonical(v, decl): v in its canonical form, or v.
pub(crate) fn storage_canonical(v: &Value, decl: &str) -> Value {
    match kind_of(decl).map(|k| canonical(v, &k)) {
        Some(Canon::To(c)) => c,
        _ => v.clone(),
    }
}

/// A column the conversion reads.
struct Col {
    name: String,
    decl: String,
}

/// What became of one column.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ColumnReport {
    pub table: String,
    pub column: String,
    pub decl: String,
    pub converted: i64,
    pub invalid: i64,
}

/// What a file's conversion did.
#[derive(Debug, Default)]
pub struct Conversion {
    pub backup: Option<String>,
    pub columns: Vec<ColumnReport>,
    pub seconds: f64,
}

/// The backup a conversion to version takes of path.
#[must_use]
pub fn backup_path(path: &str, version: i64) -> String {
    format!("{path}.pre-v{version}.bak")
}

/// Brings the file at path to the current storage format, if it holds another: see the
/// module. A new file, `:memory:`, and one already in the format are left to the
/// connection setup, which marks a fresh one. Fails, and the server with it, if the
/// backup cannot be written: nothing is converted then.
///
/// # Errors
///
/// When the file cannot be opened, the backup cannot be written, or the conversion
/// fails as a whole.
pub fn ensure_storage_format(path: &str) -> Result<Option<Conversion>> {
    if path == ":memory:" || path.is_empty() || !std::path::Path::new(path).exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI)
        .map_err(sqlite_error)?;
    conn.busy_timeout(std::time::Duration::from_secs(30))
        .map_err(sqlite_error)?;
    crate::catalog::register_functions(&conn)?;
    if current_version(&conn) >= FORMAT_VERSION {
        retry_mixed(&conn)?;
        return Ok(None);
    }
    let tables = user_columns(&conn)?;
    let start = Instant::now();
    let mut report = Conversion::default();
    if !tables.is_empty() {
        let backup = backup_path(path, FORMAT_VERSION);
        log::info!("{path}: storage format v{FORMAT_VERSION}: backing up to {backup}");
        backup_to(&conn, &backup)?;
        report.backup = Some(backup);
    }
    conn.execute_batch("PRAGMA foreign_keys = OFF").map_err(sqlite_error)?;
    conn.execute_batch("BEGIN IMMEDIATE").map_err(sqlite_error)?;
    let converted = (|| -> Result<Vec<ColumnReport>> {
        if current_version(&conn) >= FORMAT_VERSION {
            return Ok(Vec::new()); // another process did it while we copied
        }
        let mut out = Vec::new();
        for (table, cols) in &tables {
            out.extend(convert_table(&conn, path, table, cols)?);
        }
        conn.execute_batch(MIXED_TABLE_DDL).map_err(sqlite_error)?;
        for c in out.iter().filter(|c| c.invalid > 0) {
            conn.execute(
                "INSERT OR REPLACE INTO _overlite_mixed_columns VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![c.table, c.column, c.decl, c.invalid],
            )
            .map_err(sqlite_error)?;
        }
        mark(&conn)?;
        Ok(out)
    })();
    match converted {
        Ok(cols) => {
            conn.execute_batch("COMMIT").map_err(sqlite_error)?;
            report.columns = cols;
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    report.seconds = start.elapsed().as_secs_f64();
    let converted: i64 = report.columns.iter().map(|c| c.converted).sum();
    for c in report.columns.iter().filter(|c| c.converted > 0 || c.invalid > 0) {
        log::info!(
            "{path}: {}.{} ({}): {} converted{}",
            c.table,
            c.column,
            c.decl,
            c.converted,
            if c.invalid > 0 {
                format!(", {} not of the type, left as they are: the column is mixed", c.invalid)
            } else {
                String::new()
            }
        );
    }
    log::info!(
        "{path}: storage format v{FORMAT_VERSION}: {converted} values converted in {} columns, {} mixed, {:.1}s{}",
        report.columns.iter().filter(|c| c.converted > 0).count(),
        report.columns.iter().filter(|c| c.invalid > 0).count(),
        report.seconds,
        report
            .backup
            .as_ref()
            .map(|b| format!("; backup {b}"))
            .unwrap_or_default()
    );
    Ok(Some(report))
}

/// The version a file is marked with, 0 for none.
pub(crate) fn current_version(conn: &Connection) -> i64 {
    conn.query_row("SELECT max(version) FROM _overlite_storage_format", [], |r| {
        r.get::<_, Option<i64>>(0)
    })
    .ok()
    .flatten()
    .unwrap_or(0)
}

/// Writes the current version.
fn mark(conn: &Connection) -> Result<()> {
    conn.execute_batch(FORMAT_TABLE_DDL).map_err(sqlite_error)?;
    conn.execute_batch(&format!(
        "DELETE FROM _overlite_storage_format; INSERT INTO _overlite_storage_format VALUES ({FORMAT_VERSION})"
    ))
    .map_err(sqlite_error)
}

/// Marks a file that holds no user table yet: there is nothing to convert. Run by the
/// connection setup, so a new database, `:memory:` included, starts in the format.
pub(crate) fn mark_if_fresh(conn: &Connection) -> Result<()> {
    if current_version(conn) >= FORMAT_VERSION || !user_columns(conn)?.is_empty() {
        return Ok(());
    }
    mark(conn)
}

/// The user tables, with their columns of a known kind.
fn user_columns(conn: &Connection) -> Result<Vec<(String, Vec<Col>)>> {
    let mut st = conn
        .prepare(
            r"SELECT m.name, p.name, p.type FROM sqlite_master AS m, pragma_table_info(m.name) AS p
              WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite\_%' ESCAPE '\'
                AND m.name NOT LIKE '\_overlite\_%' ESCAPE '\'
              ORDER BY m.name, p.cid",
        )
        .map_err(sqlite_error)?;
    let rows = st
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })
        .map_err(sqlite_error)?;
    let mut out: Vec<(String, Vec<Col>)> = Vec::new();
    let mut any_table = false;
    for row in rows {
        let (table, name, decl) = row.map_err(sqlite_error)?;
        any_table = true;
        if out.last().is_none_or(|(t, _)| *t != table) {
            out.push((table, Vec::new()));
        }
        if kind_of(&decl).is_some()
            && let Some((_, cols)) = out.last_mut()
        {
            cols.push(Col { name, decl });
        }
    }
    let _ = any_table;
    Ok(out)
}

/// Copies the database to dest with SQLite's backup API, WAL included, through a file
/// beside it renamed into place, so a backup that is there is whole.
fn backup_to(conn: &Connection, dest: &str) -> Result<()> {
    let tmp = format!("{dest}.tmp");
    let fail = |what: &str| {
        let _ = std::fs::remove_file(&tmp);
        errf!(
            SqlState::DISK_FULL,
            "cannot write the backup {dest}: {what}; nothing was converted",
        )
    };
    let _ = std::fs::remove_file(&tmp);
    let ctmp = CString::new(tmp.as_bytes()).map_err(|_| fail("bad path"))?;
    let main = c"main";
    // SAFETY: the pointers are valid C strings for the calls; every handle opened here
    // is closed before return, and conn's handle outlives the backup.
    unsafe {
        let mut db: *mut ffi::sqlite3 = std::ptr::null_mut();
        let flags = ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE;
        if ffi::sqlite3_open_v2(ctmp.as_ptr(), &raw mut db, flags, std::ptr::null()) != ffi::SQLITE_OK {
            let msg = if db.is_null() {
                "cannot open".to_string()
            } else {
                std::ffi::CStr::from_ptr(ffi::sqlite3_errmsg(db))
                    .to_string_lossy()
                    .into_owned()
            };
            ffi::sqlite3_close(db);
            return Err(fail(&msg));
        }
        let b = ffi::sqlite3_backup_init(db, main.as_ptr(), conn.handle(), main.as_ptr());
        if b.is_null() {
            let msg = std::ffi::CStr::from_ptr(ffi::sqlite3_errmsg(db))
                .to_string_lossy()
                .into_owned();
            ffi::sqlite3_close(db);
            return Err(fail(&msg));
        }
        let rc = ffi::sqlite3_backup_step(b, -1);
        ffi::sqlite3_backup_finish(b);
        let msg = std::ffi::CStr::from_ptr(ffi::sqlite3_errmsg(db))
            .to_string_lossy()
            .into_owned();
        let closed = ffi::sqlite3_close(db);
        if rc != ffi::SQLITE_DONE || closed != ffi::SQLITE_OK {
            return Err(fail(&msg));
        }
    }
    std::fs::rename(&tmp, dest).map_err(|e| fail(&e.to_string()))
}

/// Converts the columns of one table, by rowid range when it has rowids, so that a
/// large table reports progress. A column whose new values a constraint refuses (two
/// spellings of one key in a unique index) is left whole, and reported mixed.
fn convert_table(conn: &Connection, path: &str, table: &str, cols: &[Col]) -> Result<Vec<ColumnReport>> {
    let mut out: Vec<ColumnReport> = cols
        .iter()
        .map(|c| ColumnReport {
            table: table.to_string(),
            column: c.name.clone(),
            decl: c.decl.clone(),
            ..Default::default()
        })
        .collect();
    if cols.is_empty() {
        return Ok(out);
    }
    let qt = quote_ident(table);
    let bounds: Option<(i64, i64)> = conn
        .query_row(&format!("SELECT min(rowid), max(rowid) FROM {qt}"), [], |r| {
            Ok(match (r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?) {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            })
        })
        .unwrap_or(None);
    let has_rowid = conn.prepare(&format!("SELECT rowid FROM {qt} LIMIT 0")).is_ok();
    if has_rowid && bounds.is_none() {
        return Ok(out); // empty
    }
    // Triggers would fire on the rewrite: they go for its time, and come back as they were.
    let triggers: Vec<(String, String)> = {
        let mut st = conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1 AND sql IS NOT NULL")
            .map_err(sqlite_error)?;
        st.query_map([table], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(sqlite_error)?
            .filter_map(std::result::Result::ok)
            .collect()
    };
    for (name, _) in &triggers {
        conn.execute_batch(&format!("DROP TRIGGER {}", quote_ident(name)))
            .map_err(sqlite_error)?;
    }
    let total: i64 = conn
        .query_row(&format!("SELECT count(*) FROM {qt}"), [], |r| r.get(0))
        .map_err(sqlite_error)?;
    let state = |c: &Col| {
        format!(
            "overlite_storage_state({}, {})",
            quote_ident(&c.name),
            quote_literal(&c.decl)
        )
    };
    let mut refused = vec![false; cols.len()];
    let ranges: Vec<Option<(i64, i64)>> = match bounds.filter(|_| has_rowid) {
        Some((lo, hi)) => {
            let mut v = Vec::new();
            let mut a = lo;
            while a <= hi {
                v.push(Some((a, a.saturating_add(CHUNK - 1).min(hi))));
                a = a.saturating_add(CHUNK);
                if a == i64::MAX {
                    break;
                }
            }
            v
        }
        None => vec![None],
    };
    let started = Instant::now();
    let mut last_log = Instant::now();
    for (n, range) in ranges.iter().enumerate() {
        let filter = range.map_or(String::new(), |(a, b)| format!(" AND rowid BETWEEN {a} AND {b}"));
        let counts = cols
            .iter()
            .map(|c| format!("sum({} = 2)", state(c)))
            .collect::<Vec<_>>()
            .join(", ");
        conn.query_row(&format!("SELECT {counts} FROM {qt} WHERE 1{filter}"), [], |r| {
            for (i, o) in out.iter_mut().enumerate() {
                o.invalid += r.get::<_, Option<i64>>(i)?.unwrap_or(0);
            }
            Ok(())
        })
        .map_err(sqlite_error)?;
        for (i, c) in cols.iter().enumerate() {
            if refused[i] {
                continue;
            }
            let q = quote_ident(&c.name);
            let sql = format!(
                "UPDATE {qt} SET {q} = overlite_storage_canonical({q}, {d}) WHERE {s} = 1{filter}",
                d = quote_literal(&c.decl),
                s = state(c)
            );
            conn.execute_batch("SAVEPOINT overlite_storage_column")
                .map_err(sqlite_error)?;
            match conn.execute(&sql, []) {
                Ok(changed) => {
                    out[i].converted += changed as i64;
                    conn.execute_batch("RELEASE overlite_storage_column")
                        .map_err(sqlite_error)?;
                }
                Err(e) if is_constraint(&e) => {
                    conn.execute_batch("ROLLBACK TO overlite_storage_column; RELEASE overlite_storage_column")
                        .map_err(sqlite_error)?;
                    log::warn!(
                        "{path}: {table}.{}: its canonical values break a constraint ({e}); left as it is",
                        c.name
                    );
                    refused[i] = true;
                }
                Err(e) => return Err(sqlite_error(e)),
            }
        }
        if ranges.len() > 1 && (last_log.elapsed().as_secs() >= 5 || n + 1 == ranges.len()) {
            last_log = Instant::now();
            log::info!(
                "{path}: converting {table}: {}% of {total} rows, {:.0}s",
                (n + 1) * 100 / ranges.len(),
                started.elapsed().as_secs_f64()
            );
        }
    }
    // A refused column keeps every value as it was: what was converted in an earlier
    // range goes back with the rollback below only if it is undone here, so count it
    // as what it is, a column of two forms.
    for (i, r) in refused.iter().enumerate() {
        if *r {
            let s = state(&cols[i]);
            out[i].invalid = conn
                .query_row(&format!("SELECT count(*) FROM {qt} WHERE {s} <> 0"), [], |r| r.get(0))
                .map_err(sqlite_error)?;
            out[i].invalid = out[i].invalid.max(1);
        }
    }
    for (_, sql) in &triggers {
        conn.execute_batch(sql).map_err(sqlite_error)?;
    }
    Ok(out)
}

fn is_constraint(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation)
}

/// Tries the mixed columns again: one whose values now all read as its type (an
/// operator fixed them) is converted, and leaves the list.
fn retry_mixed(conn: &Connection) -> Result<()> {
    let mixed: Vec<(String, String, String)> =
        match conn.prepare("SELECT tablename, colname, decl FROM _overlite_mixed_columns") {
            Ok(mut st) => st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(sqlite_error)?
                .filter_map(std::result::Result::ok)
                .collect(),
            Err(_) => return Ok(()),
        };
    for (table, column, decl) in mixed {
        let (qt, qc, d) = (quote_ident(&table), quote_ident(&column), quote_literal(&decl));
        let invalid: rusqlite::Result<i64> = conn.query_row(
            &format!("SELECT count(*) FROM {qt} WHERE overlite_storage_state({qc}, {d}) = 2"),
            [],
            |r| r.get(0),
        );
        let gone = invalid.is_err();
        if !gone && invalid.as_ref().is_ok_and(|n| *n > 0) {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE").map_err(sqlite_error)?;
        let done = (|| -> rusqlite::Result<()> {
            if !gone {
                conn.execute(
                    &format!(
                        "UPDATE {qt} SET {qc} = overlite_storage_canonical({qc}, {d}) WHERE overlite_storage_state({qc}, {d}) = 1"
                    ),
                    [],
                )?;
            }
            conn.execute(
                "DELETE FROM _overlite_mixed_columns WHERE tablename = ?1 AND colname = ?2",
                [&table, &column],
            )?;
            Ok(())
        })();
        match done {
            Ok(()) => {
                conn.execute_batch("COMMIT").map_err(sqlite_error)?;
                log::info!("{table}.{column}: every value now reads as {decl}; the column is no longer mixed");
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                log::warn!("{table}.{column}: still mixed: {e}");
            }
        }
    }
    Ok(())
}

/// The mixed columns of a database, as (table, column), lowercase.
pub(crate) fn mixed_columns(conn: &Connection) -> Vec<(String, String)> {
    let Ok(mut st) = conn.prepare("SELECT lower(tablename), lower(colname) FROM _overlite_mixed_columns") else {
        return Vec::new();
    };
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|it| it.filter_map(std::result::Result::ok).collect())
        .unwrap_or_default()
}

/// The error a failed conversion stops the server with.
pub(crate) fn conversion_error(path: &str, e: &Error) -> Error {
    errf!(
        SqlState::IO_ERROR,
        "{path}: cannot bring it to storage format v{FORMAT_VERSION}: {e}",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    #[test]
    fn canonical_forms() {
        let cases: Vec<(&str, Value, Canon)> = vec![
            ("boolean", Value::Int(1), Canon::Same),
            ("BOOLEAN", text("true"), Canon::To(Value::Int(1))),
            ("bool", text("f"), Canon::To(Value::Int(0))),
            ("boolean", text("maybe"), Canon::Invalid),
            ("boolean", Value::Int(2), Canon::Invalid),
            (
                "TIMESTAMP(3)",
                text("2026-08-09 05:21:01.750"),
                Canon::To(text("2026-08-09 05:21:01.75")),
            ),
            (
                "timestamp",
                text("2024-01-02T03:04:05Z"),
                Canon::To(text("2024-01-02 03:04:05")),
            ),
            ("timestamp", text("2024-01-02 03:04:05"), Canon::Same),
            (
                "timestamptz",
                text("2024-01-02 03:04:05+02"),
                Canon::To(text("2024-01-02 01:04:05")),
            ),
            ("timestamp", Value::Int(2024), Canon::Invalid),
            ("timestamp", text("soon"), Canon::Invalid),
            ("date", text("2024-01-02"), Canon::Same),
            (
                "uuid",
                text("{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}"),
                Canon::To(text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11")),
            ),
            (
                "bytea",
                text("\\xdeadbeef"),
                Canon::To(Value::Blob(vec![0xde, 0xad, 0xbe, 0xef])),
            ),
            ("DECIMALTEXT(10,2)", text("1.555"), Canon::To(text("1.56"))),
            ("DECIMALTEXT(10,2)", text("1.50"), Canon::Same),
            ("text[]", text("{a,\"b c\"}"), Canon::To(text("[\"a\",\"b c\"]"))),
            ("text[]", text("[\"a\"]"), Canon::Same),
            ("jsonb", text("{\"a\": 1}"), Canon::Same),
            ("jsonb", text("{oops"), Canon::Invalid),
        ];
        for (decl, v, want) in cases {
            let kind = kind_of(decl).unwrap_or_else(|| panic!("{decl}"));
            assert_eq!(canonical(&v, &kind), want, "{decl} {v:?}");
        }
        for decl in ["integer", "text", "varchar", "double precision", "hstore"] {
            assert!(kind_of(decl).is_none(), "{decl}");
        }
    }
}
