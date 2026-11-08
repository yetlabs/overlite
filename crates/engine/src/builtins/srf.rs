//! Set-returning functions. SQLite's json_each is lateral, which a subquery in FROM is
//! not, but its columns (`id`, `key`, `type`, `path`) collide with any table's. So each
//! function here is a scalar that builds its rows as a JSON array, and two eponymous
//! virtual tables walk such an array, with the columns Postgres gives.
//!
//! `overlite_elements(src)`: one column, `value`, and a hidden 1-based `ord`.
//! `overlite_pairs(src)`: `key` and `value`, from an array of pairs.

use crate::builtins::arrays::{array_as_json, array_lower};
use crate::builtins::json::{Json, json_quote, json_raw_object, json_unmarshal, json_unmarshal_raw_array};
use crate::builtins::regex::compile_regex;
use crate::error::{Error, Result, errf};
use crate::sqlite::sqlite_error;
use overlite_core::{SqlState, Value};
use rusqlite::types::Value as SqlValue;
use rusqlite::vtab::{
    Context, Filters, IndexConstraintOp, IndexInfo, Module, VTab, VTabConfig, VTabConnection, VTabCursor,
};
use rusqlite::{Connection, ffi};
use std::borrow::Cow;
use std::ffi::{CStr, c_int};
use std::marker::PhantomData;

/// The most rows a function may build. It builds them all at once, and a statement
/// that means more than this reads a table instead.
const MAX_SET_ROWS: i64 = 10_000_000;

/// What one of the two tables holds in its columns.
trait Shape: 'static {
    const SCHEMA: &'static CStr;
    /// The index of the hidden column the function's argument arrives through, which
    /// SQLite takes to be the first hidden one.
    const SOURCE: c_int;
    /// Column i of a row at position `ord`.
    fn cell(row: &Row, ord: i64, i: c_int) -> SqlValue;
    /// A row out of its JSON text.
    fn row(raw: &str) -> Result<Row>;
}

/// One row: its key, for a pair, and its value.
struct Row {
    key: SqlValue,
    value: SqlValue,
}

struct Elements;
struct Pairs;

impl Shape for Elements {
    const SCHEMA: &'static CStr = c"CREATE TABLE x(value, src HIDDEN, ord HIDDEN)";
    const SOURCE: c_int = 1;

    fn cell(row: &Row, ord: i64, i: c_int) -> SqlValue {
        match i {
            0 => row.value.clone(),
            _ => SqlValue::Integer(ord),
        }
    }

    fn row(raw: &str) -> Result<Row> {
        Ok(Row {
            key: SqlValue::Null,
            value: scalar_of(raw),
        })
    }
}

impl Shape for Pairs {
    const SCHEMA: &'static CStr = c"CREATE TABLE x(key, value, src HIDDEN, ord HIDDEN)";
    const SOURCE: c_int = 2;

    fn cell(row: &Row, ord: i64, i: c_int) -> SqlValue {
        match i {
            0 => row.key.clone(),
            1 => row.value.clone(),
            _ => SqlValue::Integer(ord),
        }
    }

    fn row(raw: &str) -> Result<Row> {
        let pair = json_unmarshal_raw_array(raw).unwrap_or_default();
        let [key, value] = pair[..] else {
            return Err(errf!(SqlState::INTERNAL_ERROR, "a row of overlite_pairs is not a pair"));
        };
        Ok(Row {
            key: scalar_of(key),
            value: scalar_of(value),
        })
    }
}

/// A JSON scalar as the SQL value it is, and anything else as its own text.
fn scalar_of(raw: &str) -> SqlValue {
    match raw.as_bytes().first() {
        Some(b'"') => match json_unmarshal(raw) {
            Some(Json::Str(s)) => SqlValue::Text(s),
            _ => SqlValue::Text(raw.to_string()),
        },
        Some(b'n') if raw == "null" => SqlValue::Null,
        Some(b'0'..=b'9' | b'-') => match raw.parse::<i64>() {
            Ok(n) => SqlValue::Integer(n),
            Err(_) => raw
                .parse()
                .map_or_else(|_| SqlValue::Text(raw.to_string()), SqlValue::Real),
        },
        _ => SqlValue::Text(raw.to_string()),
    }
}

/// The table: SQLite takes the first field to be its base.
#[repr(C)]
struct SetTab<S> {
    base: ffi::sqlite3_vtab,
    shape: PhantomData<S>,
}

// SAFETY: `SetTab` is repr(C) with the base first, and `connect` declares a schema of
// which `best_index` and the cursor read only what the shape says it holds.
unsafe impl<'vtab, S: Shape> VTab<'vtab> for SetTab<S> {
    type Aux = ();
    type Cursor = SetCursor<'vtab, S>;

    fn connect(
        db: &mut VTabConnection,
        _aux: Option<&()>,
        _module: &[u8],
        _database: &[u8],
        _table: &[u8],
        _args: &[&[u8]],
    ) -> rusqlite::Result<(Cow<'static, CStr>, Self)> {
        db.config(VTabConfig::Innocuous)?;
        let tab = SetTab {
            base: ffi::sqlite3_vtab::default(),
            shape: PhantomData,
        };
        Ok((Cow::Borrowed(S::SCHEMA), tab))
    }

    fn best_index(&self, info: &mut IndexInfo) -> rusqlite::Result<bool> {
        let mut source = None;
        let mut waiting = false;
        for (i, c) in info.constraints().enumerate() {
            if c.column() != S::SOURCE || c.operator() != IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ {
                continue;
            }
            if c.is_usable() {
                source = Some(i);
            } else {
                waiting = true;
            }
        }
        if let Some(i) = source {
            let mut usage = info.constraint_usage(i);
            usage.set_argv_index(1);
            usage.set_omit(true);
            info.set_idx_num(1);
            info.set_estimated_cost(10.0);
            info.set_estimated_rows(16);
            return Ok(true);
        }
        // A column of a table to its left: SQLite has to order it first.
        Ok(!waiting)
    }

    fn open(&'vtab mut self) -> rusqlite::Result<SetCursor<'vtab, S>> {
        Ok(SetCursor {
            base: ffi::sqlite3_vtab_cursor::default(),
            rows: Vec::new(),
            at: 0,
            shape: PhantomData,
        })
    }
}

#[repr(C)]
struct SetCursor<'vtab, S> {
    base: ffi::sqlite3_vtab_cursor,
    rows: Vec<Row>,
    at: usize,
    shape: PhantomData<&'vtab S>,
}

// SAFETY: the cursor is repr(C) with the base first, and it holds nothing of SQLite's
// past a call: each row is copied out of the argument in `filter`.
unsafe impl<S: Shape> VTabCursor for SetCursor<'_, S> {
    fn filter(&mut self, idx_num: c_int, _idx_str: Option<&str>, args: &Filters<'_>) -> rusqlite::Result<()> {
        self.rows.clear();
        self.at = 0;
        if idx_num == 0 {
            return Ok(());
        }
        let Some(src) = args.get::<Option<String>>(0)? else {
            return Ok(()); // NULL is no rows
        };
        let Some(raw) = json_unmarshal_raw_array(&src) else {
            return Err(rusqlite::Error::ModuleError(format!("malformed set: {src}")));
        };
        for item in raw {
            self.rows
                .push(S::row(item).map_err(|e| rusqlite::Error::ModuleError(e.into_message()))?);
        }
        Ok(())
    }

    fn next(&mut self) -> rusqlite::Result<()> {
        self.at += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.at >= self.rows.len()
    }

    fn column(&self, ctx: &mut Context, i: c_int) -> rusqlite::Result<()> {
        match self.rows.get(self.at) {
            Some(row) => ctx.set_result(&S::cell(row, self.at as i64 + 1, i)),
            None => Ok(()),
        }
    }

    fn rowid(&self) -> rusqlite::Result<i64> {
        Ok(self.at as i64 + 1)
    }
}

/// Registers the two tables on a connection.
pub(crate) fn register_srf_functions(conn: &Connection) -> Result<()> {
    const ELEMENTS: Module<SetTab<Elements>> = Module::eponymous_only_module();
    const PAIRS: Module<SetTab<Pairs>> = Module::eponymous_only_module();
    conn.create_module(c"overlite_elements", &ELEMENTS, None::<()>)
        .map_err(sqlite_error)?;
    conn.create_module(c"overlite_pairs", &PAIRS, None::<()>)
        .map_err(sqlite_error)
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        other => other.to_string(),
    }
}

fn bad_json(text: &str) -> Error {
    errf!(
        SqlState::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type json: \"{text}\""
    )
}

/// A JSON array of the rows, each a string or null.
fn string_array(items: impl Iterator<Item = Option<String>>) -> Value {
    let cells: Vec<String> = items
        .map(|s| s.map_or("null".to_string(), |s| json_quote(&s)))
        .collect();
    Value::Text(["[", &cells.join(","), "]"].concat())
}

/// What a value of JSON is as text: a string its contents, null nothing, and anything
/// else as written.
fn text_form(raw: &str) -> Option<String> {
    match raw {
        "null" => None,
        _ if raw.starts_with('"') => match json_unmarshal(raw) {
            Some(Json::Str(s)) => Some(s),
            _ => Some(raw.to_string()),
        },
        _ => Some(raw.to_string()),
    }
}

/// The elements of a JSON array for json_array_elements and its `_text`: each as the
/// text of its JSON, or of its value.
pub(crate) fn json_elements(v: &Value, as_text: bool) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let text = text_of(v);
    let what = match text.trim_start().as_bytes().first() {
        Some(b'[') => None,
        Some(b'{') => Some("an object"),
        Some(_) => Some("a scalar"),
        None => return Err(bad_json(&text)),
    };
    if let Some(what) = what {
        return Err(errf!(
            SqlState::INVALID_PARAMETER_VALUE,
            "cannot extract elements from {what}"
        ));
    }
    let Some(raw) = json_unmarshal_raw_array(&text) else {
        return Err(bad_json(&text));
    };
    Ok(string_array(raw.into_iter().map(|item| {
        if as_text {
            text_form(item)
        } else {
            Some(item.to_string())
        }
    })))
}

/// An object's members in the order jsonb keeps them: by length, then by bytes, the
/// last of a repeated key winning. json keeps them as written.
fn sorted_members(mut members: Vec<(String, &str)>) -> Vec<(String, &str)> {
    members.reverse();
    members.sort_by(|a, b| (a.0.len(), &a.0).cmp(&(b.0.len(), &b.0)));
    members.dedup_by(|later, first| later.0 == first.0);
    members
}

fn members_of<'a>(text: &'a str, function: &str, sorted: bool) -> Result<Vec<(String, &'a str)>> {
    match json_raw_object(text) {
        Some(m) if sorted => Ok(sorted_members(m)),
        Some(m) => Ok(m),
        None if json_unmarshal(text).is_none() => Err(bad_json(text)),
        None => Err(errf!(
            SqlState::INVALID_PARAMETER_VALUE,
            "cannot call {function} on a non-object"
        )),
    }
}

/// The members of a JSON object for json_each and its `_text`, as pairs.
pub(crate) fn json_pairs(v: &Value, as_text: bool, sorted: bool) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let text = text_of(v);
    let members = members_of(&text, "json_each", sorted)?;
    let pairs: Vec<String> = members
        .into_iter()
        .map(|(key, raw)| {
            let value = if as_text { text_form(raw) } else { Some(raw.to_string()) };
            let value = value.map_or("null".to_string(), |s| json_quote(&s));
            ["[", &json_quote(&key), ",", &value, "]"].concat()
        })
        .collect();
    Ok(Value::Text(["[", &pairs.join(","), "]"].concat()))
}

/// The keys of a JSON object, for json_object_keys under the name it was called by.
pub(crate) fn json_keys(v: &Value, function: &str) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let text = text_of(v);
    let sorted = function.starts_with("jsonb");
    let members = members_of(&text, function, sorted)?;
    Ok(string_array(members.into_iter().map(|(key, _)| Some(key))))
}

/// regexp_split_to_table and regexp_split_to_array: the pieces of a text between the
/// matches of a pattern. A match of nothing at the start or at the end splits nothing.
pub(crate) fn regexp_split(a: &[Value]) -> Result<Value> {
    if a.len() < 2 || a.len() > 3 {
        return Err(errf!(
            SqlState::UNDEFINED_FUNCTION,
            "function regexp_split_to_table does not exist"
        ));
    }
    if a[0].is_null() || a[1].is_null() {
        return Ok(Value::Null);
    }
    let (text, mut pattern) = (text_of(&a[0]), text_of(&a[1]));
    if let Some(flags) = a.get(2).map(text_of) {
        for flag in flags.chars() {
            match flag {
                'i' => pattern.insert_str(0, "(?i)"),
                'c' => {}
                'g' => {
                    return Err(errf!(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "regexp_split_to_table() does not support the \"global\" option"
                    ));
                }
                other => {
                    return Err(errf!(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "invalid regular expression option: \"{other}\""
                    ));
                }
            }
        }
    }
    let matcher = compile_regex(&pattern)?;
    let mut pieces = Vec::new();
    let mut from = 0;
    for (start, end) in matcher.find_spans(&text)? {
        if start == end && (start == from || start == text.len()) {
            continue;
        }
        pieces.push(Some(text[from..start].to_string()));
        from = end;
    }
    pieces.push(Some(text[from..].to_string()));
    Ok(string_array(pieces.into_iter()))
}

/// Several arrays side by side, a row of each, the shorter padded with NULLs.
pub(crate) fn zip_arrays(a: &[Value]) -> Result<Value> {
    let arrays: Vec<Vec<String>> = a
        .iter()
        .map(|v| {
            let text = text_of(&array_as_json(v));
            json_unmarshal_raw_array(&text)
                .map(|items| items.into_iter().map(str::to_string).collect())
                .unwrap_or_default()
        })
        .collect();
    let rows = arrays.iter().map(Vec::len).max().unwrap_or(0);
    let cells: Vec<String> = (0..rows)
        .map(|row| {
            let cell = arrays.iter().map(|array| array.get(row).map_or("null", String::as_str));
            ["[", &cell.collect::<Vec<_>>().join(","), "]"].concat()
        })
        .collect();
    Ok(Value::Text(["[", &cells.join(","), "]"].concat()))
}

fn number_of(v: &Value) -> Result<f64> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(f) => Ok(*f),
        other => text_of(other).trim().parse().map_err(|_| {
            errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "generate_series in a select list takes numbers here"
            )
        }),
    }
}

/// generate_series in a select list.
pub(crate) fn series(a: &[Value]) -> Result<Value> {
    if a.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let (start, stop) = match a {
        [start, stop] | [start, stop, _] => (number_of(start)?, number_of(stop)?),
        _ => {
            return Err(errf!(
                SqlState::UNDEFINED_FUNCTION,
                "function generate_series() does not exist"
            ));
        }
    };
    let step = a.get(2).map_or(Ok(1.0), number_of)?;
    if step == 0.0 {
        return Err(errf!(SqlState::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
    }
    let count = ((stop - start) / step).floor();
    if count >= MAX_SET_ROWS as f64 {
        return Err(errf!(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "generate_series in a select list is held to {MAX_SET_ROWS} rows"
        ));
    }
    let whole = a.iter().all(|v| matches!(v, Value::Int(_)));
    let cells: Vec<String> = (0..=count.max(-1.0) as i64)
        .filter(|_| count >= 0.0)
        .map(|i| {
            let n = start + i as f64 * step;
            if whole { (n as i64).to_string() } else { n.to_string() }
        })
        .collect();
    Ok(Value::Text(["[", &cells.join(","), "]"].concat()))
}

/// The subscripts of an array, for generate_subscripts.
pub(crate) fn subscripts(v: &Value) -> Value {
    let lower = match array_lower(v) {
        Value::Int(n) => n,
        _ => 1,
    };
    let text = text_of(&array_as_json(v));
    let n = json_unmarshal_raw_array(&text).map_or(0, |items| items.len() as i64);
    let cells: Vec<String> = (lower..lower + n).map(|i| i.to_string()).collect();
    Value::Text(["[", &cells.join(","), "]"].concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str, pattern: &str) -> String {
        regexp_split(&[Value::from(text), Value::from(pattern)])
            .unwrap()
            .to_string()
    }

    // Each answer was read off Postgres 17.
    #[test]
    fn regexp_split_pieces() {
        assert_eq!(split("a,b;c", "[,;]"), r#"["a","b","c"]"#);
        assert_eq!(split("abc", ""), r#"["a","b","c"]"#);
        assert_eq!(split("", ","), r#"[""]"#);
        assert_eq!(split(",a", ","), r#"["","a"]"#);
        assert_eq!(split("a,", ","), r#"["a",""]"#);
    }

    #[test]
    fn jsonb_keeps_its_own_order() {
        let v = Value::from(r#"{"bb":1,"a":2,"a":3,"c":"x"}"#);
        assert_eq!(
            json_pairs(&v, true, true).unwrap().to_string(),
            r#"[["a","3"],["c","x"],["bb","1"]]"#
        );
        assert_eq!(
            json_keys(&Value::from(r#"{"bb":1,"a":2}"#), "json_object_keys")
                .unwrap()
                .to_string(),
            r#"["bb","a"]"#
        );
    }

    #[test]
    fn series_of_numbers() {
        let s = |a: &[Value]| series(a).unwrap().to_string();
        assert_eq!(s(&[Value::Int(1), Value::Int(3)]), "[1,2,3]");
        assert_eq!(s(&[Value::Int(3), Value::Int(1), Value::Int(-1)]), "[3,2,1]");
        assert_eq!(s(&[Value::Int(3), Value::Int(1)]), "[]");
        assert_eq!(
            s(&[Value::Float(1.0), Value::Float(2.0), Value::Float(0.5)]),
            "[1,1.5,2]"
        );
    }
}
