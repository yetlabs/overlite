//! The Postgres-compatible functions and catalog views on top of SQLite.
//!
//! Two pieces:
//!   - scalar functions clients commonly call: version(), current_schema(),
//!     format_type(), pg_table_is_visible(), plus a REGEXP implementation so
//!     Postgres' ~ / !~ operators work.
//!   - the per-connection setup (schema.rs) that materializes information_schema
//!     and pg_catalog views, backed by SQLite's own sqlite_master /
//!     pragma_table_info.
//!
//! information_schema views keep their dotted name (quoted); pg_catalog objects
//! use bare names and the Postgres protocol strips the "pg_catalog." qualifier.
//! Either way the user's real database file stays free of emulation objects.

mod comments;
pub(crate) mod constraints;
pub(crate) mod enums;
pub(crate) mod grants;
pub(crate) mod rls;
pub(crate) mod rls_cache;
mod roles;
pub(crate) mod schema;
pub(crate) mod sequences;
mod triggers;
mod type_mod;
mod views;

use crate::builtins::arrays::{array_as_json, array_lower, is_json_array_text, json_array_len, json_arrays_overlap};
use crate::builtins::casts::{CastType, assign, cast};
use crate::builtins::dates::{age_fn, at_time_zone, date_trunc_fn, from_time_zone, to_char_fn};
use crate::builtins::decimal::{DecAgg, DecSum, bin_dec, dec_cmp, dec_cmp_str, dec_round, dec_string, parse_dec};
use crate::builtins::hstore::{hstore_get, hstore_in, hstore_keys, hstore_vals};
use crate::builtins::json::{json_quote, jsonb_contains, jsonb_exists, jsonb_exists_all, jsonb_exists_any};
use crate::builtins::ranges::{
    Range, RangeKind, arg_str, bound_value, is_range_text, parse_range, range_construct, range_contains,
    range_contains_range, range_op, range_overlaps,
};
use crate::builtins::regex::compile_regex;
use crate::builtins::srf::{
    json_elements, json_keys, json_pairs, regexp_split, register_srf_functions, series, subscripts, zip_arrays,
};
use crate::catalog::constraints::{
    attidentity, check_constraints, default_opclass_oid, fk_constraint_name, fk_deferral, identity_type, index_include,
    index_null_key_column, index_opclass, index_where, pk_constraint_name, unique_constraint_name, unique_constraints,
    unique_deferral,
};
use crate::catalog::enums::{enum_labels_from_ddl, enum_type_exists, lookup_enum_name};
use crate::catalog::schema::CATALOG_ROLE;
use crate::catalog::triggers::lookup_trigger_def;
use crate::catalog::type_mod::{
    element_decl, format_type_mod, is_array_decl, type_att_typmod, type_char_max_length, type_datetime_precision,
    type_numeric_precision, type_numeric_precision_radix, type_numeric_scale,
};
use crate::error::{Error, ErrorKind, Result, errf};
use crate::sql_funcs::parse::identity_args;
use crate::sql_funcs::{SqlFunc, func_by_oid_lookup};
use crate::sqlite::{set_function_state, sqlite_error};
use overlite_core::text::{has_raw_bytes, quote_literal, text_from_bytes, text_to_bytes, valid_utf8};
use overlite_core::{SqlState, Value, lazy_regex, parse_bool};
use rand::RngCore;
use rusqlite::Connection;
use rusqlite::ffi;
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::{ToSql, ToSqlOutput, Value as SqlValue, ValueRef};
use std::ffi::{CString, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

/// A boolean as SQLite stores one.
fn bool_int(b: bool) -> i64 {
    i64::from(b)
}

/// Lowers char by char, with simple case mappings.
pub(crate) fn lower_chars(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let s = valid_utf8(s);
    s.chars()
        .map(|c| {
            let mut m = c.to_lowercase();
            match (m.next(), m.next()) {
                (Some(l), None) => l,
                // Only U+0130 expands; its simple mapping is 'i'.
                (Some(l), Some(_)) => l,
                _ => c,
            }
        })
        .collect()
}

/// Uppers char by char, with simple case mappings; a char whose full mapping
/// expands has no simple one, except the Greek iota-subscript letters.
fn upper_chars(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_uppercase();
    }
    let s = valid_utf8(s);
    s.chars()
        .map(|c| {
            let mut m = c.to_uppercase();
            match (m.next(), m.next()) {
                (Some(u), None) => u,
                _ => match c as u32 {
                    x @ (0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7) => char::from_u32(x + 8).unwrap_or(c),
                    0x1FB3 => '\u{1FBC}',
                    0x1FC3 => '\u{1FCC}',
                    0x1FF3 => '\u{1FFC}',
                    _ => c,
                },
            }
        })
        .collect()
}

/// An aggregate: every one doubles as a window function, and xFinal answers with
/// WindowValue.
pub(crate) trait AggregateFunction {
    fn step(&mut self, args: &[Value]);
    fn window_inverse(&mut self, args: &[Value]);
    fn window_value(&mut self) -> Value;
}

/// ArrayAgg accumulates array_agg() values into a Postgres array literal.
#[derive(Debug, Default)]
struct ArrayAgg {
    vals: Vec<String>,
    /// How many leading vals the window dropped.
    head: usize,
}

impl AggregateFunction for ArrayAgg {
    fn step(&mut self, args: &[Value]) {
        if !args.is_empty() {
            if args[0].is_null() {
                self.vals.push("NULL".to_string());
            } else {
                self.vals.push(args[0].to_string());
            }
        }
    }

    fn window_inverse(&mut self, _args: &[Value]) {
        if self.head < self.vals.len() {
            self.head += 1;
        }
    }

    fn window_value(&mut self) -> Value {
        if self.head == self.vals.len() {
            return Value::Null; // array_agg over no rows is NULL in Postgres
        }
        Value::Text(format!("{{{}}}", self.vals[self.head..].join(",")))
    }
}

/// A random RFC-4122 version-4 UUID string, backing
/// gen_random_uuid()/uuid_generate_v4().
fn new_uuid_v4() -> Result<String> {
    let mut b = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut b)
        .map_err(|e| errf!(SqlState::INTERNAL_ERROR, "{e}"))?;
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    Ok(format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&b[0..4]),
        hex::encode(&b[4..6]),
        hex::encode(&b[6..8]),
        hex::encode(&b[8..10]),
        hex::encode(&b[10..16])
    ))
}

/// A function argument: text stops at the first NUL.
fn text_value(b: &[u8]) -> Value {
    let b = match b.iter().position(|&c| c == 0) {
        Some(n) => &b[..n],
        None => b,
    };
    Value::Text(text_from_bytes(b).into_owned())
}

fn arg_value(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(b) => text_value(b),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// A function's result: rusqlite's text is a String, which text that is not
/// UTF-8 cannot be.
#[derive(Debug)]
enum FnResult {
    Plain(SqlValue),
    RawText(Vec<u8>),
}

impl ToSql for FnResult {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            FnResult::Plain(v) => v.into(),
            FnResult::RawText(b) => ValueRef::Text(b),
        }))
    }
}

/// A function's return value, as SQLite takes it.
fn sql_value(v: Value) -> FnResult {
    FnResult::Plain(match v {
        Value::Null => SqlValue::Null,
        Value::Int(i) => SqlValue::Integer(i),
        Value::Float(f) => SqlValue::Real(f),
        Value::Text(s) if has_raw_bytes(s.as_bytes()) => {
            return FnResult::RawText(text_to_bytes(&s).into_owned());
        }
        Value::Text(s) => SqlValue::Text(s),
        Value::Blob(b) => SqlValue::Blob(b),
        Value::Bool(b) => SqlValue::Integer(b as i64),
        Value::Time(t) => SqlValue::Integer(t.timestamp()),
    })
}

/// Registers a function; failures surface with SQLITE_ERROR and the bare
/// message.
fn register<F>(conn: &Connection, name: &str, n_arg: i32, flags: FunctionFlags, f: F) -> Result<()>
where
    F: Fn(&[Value]) -> Result<Value> + Send + 'static,
{
    conn.create_scalar_function(name, n_arg, flags, move |ctx: &Context<'_>| {
        let mut args = Vec::with_capacity(ctx.len());
        for i in 0..ctx.len() {
            args.push(arg_value(ctx.get_raw(i)));
        }
        match f(&args) {
            Ok(v) => Ok(sql_value(v)),
            Err(e) => {
                if let ErrorKind::State(state) = e.kind() {
                    set_function_state(state);
                }
                Err(rusqlite::Error::SqliteFailure(
                    ffi::Error::new(ffi::SQLITE_ERROR),
                    Some(e.into_message()),
                ))
            }
        }
    })
    .map_err(sqlite_error)?;
    Ok(())
}

/// Registers a function that reads the database its statement runs in, which f gets
/// as its first argument. Never a constant: what it reads may change.
fn reading<F>(conn: &Connection, name: &str, f: F) -> Result<()>
where
    F: Fn(&Connection, &[Value]) -> Result<Value> + Send + 'static,
{
    conn.create_scalar_function(name, -1, FunctionFlags::SQLITE_UTF8, move |ctx: &Context<'_>| {
        // SAFETY: the handle is the connection stepping this statement, on this
        // thread; db is used within this call only and never leaves the thread.
        let db = unsafe { ctx.get_connection() }?;
        let mut args = Vec::with_capacity(ctx.len());
        for i in 0..ctx.len() {
            args.push(arg_value(ctx.get_raw(i)));
        }
        match f(&db, &args) {
            Ok(v) => Ok(sql_value(v)),
            Err(e) => {
                if let ErrorKind::State(state) = e.kind() {
                    set_function_state(state);
                }
                Err(rusqlite::Error::SqliteFailure(
                    ffi::Error::new(ffi::SQLITE_ERROR),
                    Some(e.into_message()),
                ))
            }
        }
    })
    .map_err(sqlite_error)?;
    Ok(())
}

/// Registers a function SQLite may treat as a constant of its arguments.
pub(crate) fn scalar<F>(conn: &Connection, name: &str, n_arg: i32, f: F) -> Result<()>
where
    F: Fn(&[Value]) -> Result<Value> + Send + 'static,
{
    register(
        conn,
        name,
        n_arg,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        f,
    )
}

/// scalar, for the function modules of builtins.
pub(crate) fn scalar_fn<F>(conn: &Connection, name: &str, n_arg: i32, f: F) -> Result<()>
where
    F: Fn(&[Value]) -> Result<Value> + Send + 'static,
{
    scalar(conn, name, n_arg, f)
}

/// aggregate, for the function modules of builtins.
pub(crate) fn aggregate_fn(conn: &Connection, name: &str, n_arg: i32, factory: AggFactory) -> Result<()> {
    aggregate(conn, name, n_arg, factory)
}

/// A non-deterministic function (e.g. random UUIDs): SQLite must call it once per
/// row, not fold it to a constant.
fn scalar_nd<F>(conn: &Connection, name: &str, n_arg: i32, f: F) -> Result<()>
where
    F: Fn(&[Value]) -> Result<Value> + Send + 'static,
{
    register(conn, name, n_arg, FunctionFlags::SQLITE_UTF8, f)
}

type AggFactory = fn() -> Box<dyn AggregateFunction>;
type AggSlot = *mut Box<dyn AggregateFunction>;

/// The instance of this evaluation, created on first use. The
/// rusqlite `window` feature is off, so window aggregates go through the C API.
///
/// # Safety
///
/// `ctx` is the context SQLite handed a callback of a function registered by
/// [`aggregate`], whose user data is an [`AggFactory`].
unsafe fn make_aggregate(ctx: *mut ffi::sqlite3_context) -> Option<AggSlot> {
    // SAFETY: ctx is live, per the contract above.
    let slot = unsafe { ffi::sqlite3_aggregate_context(ctx, size_of::<AggSlot>() as c_int) }.cast::<AggSlot>();
    if slot.is_null() {
        // SAFETY: as above.
        unsafe { result_error(ctx, "insufficient memory for aggregate") };
        return None;
    }
    // SAFETY: slot is SQLite's buffer for this evaluation, big enough, 8-byte aligned,
    // zeroed the first time and kept until agg_final. The user data is the AggFactory
    // that `aggregate` registered, so the transmute restores its type.
    unsafe {
        if (*slot).is_null() {
            let factory = std::mem::transmute::<*mut c_void, AggFactory>(ffi::sqlite3_user_data(ctx));
            *slot = Box::into_raw(Box::new(factory()));
        }
        Some(*slot)
    }
}

/// # Safety
///
/// `ctx` is the context of a running function.
unsafe fn result_error(ctx: *mut ffi::sqlite3_context, msg: &str) {
    if let Ok(c) = CString::new(msg) {
        // SAFETY: ctx is live, and SQLite copies the message before returning.
        unsafe { ffi::sqlite3_result_error(ctx, c.as_ptr(), -1) };
    }
    // SAFETY: ctx is live.
    unsafe { ffi::sqlite3_result_error_code(ctx, ffi::SQLITE_ERROR) };
}

/// # Safety
///
/// `argv` holds the `argc` values SQLite passed a function, live for this call.
unsafe fn raw_args(argc: c_int, argv: *mut *mut ffi::sqlite3_value) -> Vec<Value> {
    let mut args = Vec::with_capacity(argc as usize);
    for i in 0..argc as usize {
        // SAFETY: i is below argc, and each value is live. A text or blob pointer is
        // valid for the byte count SQLite reports, and is copied before the next call.
        args.push(unsafe {
            let v = *argv.add(i);
            match ffi::sqlite3_value_type(v) {
                ffi::SQLITE_INTEGER => Value::Int(ffi::sqlite3_value_int64(v)),
                ffi::SQLITE_FLOAT => Value::Float(ffi::sqlite3_value_double(v)),
                ffi::SQLITE_TEXT => {
                    let p = ffi::sqlite3_value_text(v);
                    let n = ffi::sqlite3_value_bytes(v) as usize;
                    if p.is_null() || n == 0 {
                        Value::Text(String::new())
                    } else {
                        text_value(std::slice::from_raw_parts(p, n))
                    }
                }
                ffi::SQLITE_BLOB => {
                    let n = ffi::sqlite3_value_bytes(v) as usize;
                    let p = ffi::sqlite3_value_blob(v).cast::<u8>();
                    if p.is_null() || n == 0 {
                        Value::Blob(Vec::new())
                    } else {
                        Value::Blob(std::slice::from_raw_parts(p, n).to_vec())
                    }
                }
                _ => Value::Null,
            }
        });
    }
    args
}

/// # Safety
///
/// `ctx` is the context of a running function.
unsafe fn result_value(ctx: *mut ffi::sqlite3_context, v: Value) {
    // SAFETY: ctx is live, and SQLITE_TRANSIENT makes SQLite copy the text.
    unsafe {
        match v {
            Value::Int(i) => ffi::sqlite3_result_int64(ctx, i),
            Value::Float(f) => ffi::sqlite3_result_double(ctx, f),
            Value::Text(s) => ffi::sqlite3_result_text(
                ctx,
                s.as_ptr().cast::<c_char>(),
                s.len() as c_int,
                ffi::SQLITE_TRANSIENT(),
            ),
            _ => ffi::sqlite3_result_null(ctx),
        }
    }
}

/// Runs an aggregate's callback, turning a panic into an error of its statement:
/// unwinding out of an `extern "C"` function aborts the process, which would end every
/// session and not the one connection a panic is allowed to end.
///
/// # Safety
///
/// `ctx` is the context of a running function.
unsafe fn guarded(ctx: *mut ffi::sqlite3_context, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        // SAFETY: per this function's contract.
        unsafe { result_error(ctx, "aggregate failed") };
    }
}

// The four callbacks below are what `aggregate` registers, so SQLite calls them with a
// live context and, for the first two, argv and argc as raw_args wants them. The slot
// holds a Box made by make_aggregate, and SQLite never runs two callbacks of one
// evaluation at once. agg_final is the last call of the evaluation, so it may free it.

unsafe extern "C" fn agg_step(ctx: *mut ffi::sqlite3_context, argc: c_int, argv: *mut *mut ffi::sqlite3_value) {
    // SAFETY: see above.
    unsafe {
        guarded(ctx, || {
            if let Some(slot) = make_aggregate(ctx) {
                (*slot).step(&raw_args(argc, argv));
            }
        });
    }
}

unsafe extern "C" fn agg_inverse(ctx: *mut ffi::sqlite3_context, argc: c_int, argv: *mut *mut ffi::sqlite3_value) {
    // SAFETY: see above.
    unsafe {
        guarded(ctx, || {
            if let Some(slot) = make_aggregate(ctx) {
                (*slot).window_inverse(&raw_args(argc, argv));
            }
        });
    }
}

unsafe extern "C" fn agg_value(ctx: *mut ffi::sqlite3_context) {
    // SAFETY: see above.
    unsafe {
        guarded(ctx, || {
            if let Some(slot) = make_aggregate(ctx) {
                result_value(ctx, (*slot).window_value());
            }
        });
    }
}

unsafe extern "C" fn agg_final(ctx: *mut ffi::sqlite3_context) {
    // SAFETY: see above.
    unsafe {
        guarded(ctx, || {
            if let Some(slot) = make_aggregate(ctx) {
                result_value(ctx, (*slot).window_value());
                drop(Box::from_raw(slot));
            }
        });
    }
}

/// Registers a window-capable aggregate.
fn aggregate(conn: &Connection, name: &str, n_arg: i32, factory: AggFactory) -> Result<()> {
    let c_name = CString::new(name).map_err(|e| errf!(SqlState::INTERNAL_ERROR, "{e}"))?;
    // SAFETY: the handle is the open conn, SQLite copies the name, and the callbacks
    // match the signatures it expects. A plain fn pointer as user data needs no destructor.
    let rc = unsafe {
        ffi::sqlite3_create_window_function(
            conn.handle(),
            c_name.as_ptr(),
            n_arg,
            ffi::SQLITE_UTF8,
            factory as *mut c_void,
            Some(agg_step),
            Some(agg_final),
            Some(agg_value),
            Some(agg_inverse),
            None,
        )
    };
    if rc != ffi::SQLITE_OK {
        return Err(errf!(
            SqlState::INTERNAL_ERROR,
            "sqlite3_create_window_function {name}: {rc}"
        ));
    }
    Ok(())
}

fn text(s: &str) -> Result<Value> {
    Ok(Value::Text(s.to_string()))
}

fn null(_: &[Value]) -> Result<Value> {
    Ok(Value::Null)
}

fn empty(_: &[Value]) -> Result<Value> {
    text("")
}

/// Function-definition renderers for user LANGUAGE sql functions. Unknown oids
/// (catalog stubs) return "".
fn fn_field(pick: fn(&SqlFunc) -> String) -> impl Fn(&[Value]) -> Result<Value> + Send + Copy + 'static {
    move |a: &[Value]| {
        if a.is_empty() || a[0].is_null() {
            return text("");
        }
        if let Some(f) = func_by_oid_lookup(as_int64(&a[0])) {
            return Ok(Value::Text(pick(&f)));
        }
        text("")
    }
}

fn range_acc(conn: &Connection, name: &str, f: fn(&Range) -> Value) -> Result<()> {
    scalar(conn, name, 1, move |a| {
        if a.is_empty() || a[0].is_null() {
            return Ok(Value::Null);
        }
        match parse_range(&a[0].to_string()) {
            Some(r) => Ok(f(&r)),
            None => Ok(Value::Null),
        }
    })
}

/// Length over the JSON-array storage. array_length/array_upper of an empty array
/// are NULL (Postgres semantics); cardinality is 0.
fn arr_len(args: &[Value]) -> Result<Value> {
    if args.is_empty() {
        return Ok(Value::Null);
    }
    match json_array_len(&args[0]) {
        Some(n) if n > 0 => Ok(Value::Int(n)),
        _ => Ok(Value::Null),
    }
}

/// What a value stored in a boolean column means: an integer is true when not 0, and
/// text is read as Postgres' boolin reads it.
pub(crate) fn stored_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Null | Value::Time(_) => None,
        Value::Bool(b) => Some(*b),
        Value::Int(n) => Some(*n != 0),
        Value::Float(f) => Some(*f != 0.0),
        Value::Text(s) => parse_bool(s),
        Value::Blob(b) => parse_bool(&String::from_utf8_lossy(b)),
    }
}

fn int(b: bool) -> Result<Value> {
    Ok(Value::Int(bool_int(b)))
}

/// Registers everything on a new connection: SQLite functions are per connection.
pub(crate) fn register_functions(conn: &Connection) -> Result<()> {
    for name in ["gen_random_uuid", "uuid_generate_v4"] {
        scalar_nd(conn, name, 0, |_| Ok(Value::Text(new_uuid_v4()?)))?;
    }

    // Exact numeric: a collation that compares decimal text numerically, plus
    // big-rational arithmetic and an exact aggregate sum (see decimal.rs).
    conn.create_collation("DECIMAL", |l, r| dec_cmp_str(l, r).cmp(&0))
        .map_err(sqlite_error)?;
    // An interval's order, by its span, which resolve/temporal.rs gives an ORDER BY key
    // of an interval; text that is no interval goes after, in byte order.
    conn.create_collation("INTERVAL", |l, r| {
        let read = crate::builtins::casts::parse_interval;
        match (read(l), read(r)) {
            (Some(x), Some(y)) => crate::builtins::temporal::interval_cmp(&x, &y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => l.cmp(r),
        }
    })
    .map_err(sqlite_error)?;
    // jsonb's equality and order, for what `->`, `#>` and a cast to jsonb give: see
    // rewrite/json.rs in the postgres crate.
    conn.create_collation("JSONB", crate::builtins::json::jsonb_cmp)
        .map_err(sqlite_error)?;
    // `->>` is a function to SQLite, called by the operator's name: Postgres' gives text.
    scalar(conn, "->>", 2, |a| {
        crate::builtins::json::jsonb_arrow_text(&a[0], &a[1])
    })?;
    scalar(conn, "overlite_jsonb_path", 3, |a| {
        crate::builtins::json::jsonb_path(&a[0], &a[1], matches!(a[2], Value::Int(1)))
    })?;
    scalar(conn, "dec_add", 2, |a| {
        Ok(bin_dec(&a[0], &a[1], super::builtins::decimal::Rat::add))
    })?;
    scalar(conn, "dec_sub", 2, |a| {
        Ok(bin_dec(&a[0], &a[1], super::builtins::decimal::Rat::sub))
    })?;
    scalar(conn, "dec_mul", 2, |a| {
        Ok(bin_dec(&a[0], &a[1], super::builtins::decimal::Rat::mul))
    })?;
    scalar(conn, "dec_div", 2, |a| {
        if let Some(y) = parse_dec(&a[1])
            && y.sign() == 0
        {
            return Err(Error::with_state(
                SqlState::DIVISION_BY_ZERO,
                "division by zero".to_string(),
            ));
        }
        Ok(bin_dec(&a[0], &a[1], super::builtins::decimal::Rat::quo))
    })?;
    // numeric arithmetic as resolve/temporal.rs writes it: exact, at the scale Postgres
    // gives the result. overlite_numeric_op(a, b, '+').
    scalar(conn, "overlite_numeric_op", 3, |a| {
        let op = a[2].to_string().chars().next().unwrap_or('+');
        if op == '/'
            && let Some(y) = parse_dec(&a[1])
            && y.sign() == 0
        {
            return Err(Error::with_state(
                SqlState::DIVISION_BY_ZERO,
                "division by zero".to_string(),
            ));
        }
        Ok(crate::builtins::decimal::dec_op(&a[0], &a[1], op))
    })?;
    scalar(conn, "dec_cmp", 2, |a| Ok(Value::Int(dec_cmp(&a[0], &a[1]) as i64)))?;
    scalar(conn, "dec_round", -1, |a| {
        if a.is_empty() {
            return Ok(Value::Null);
        }
        let Some(r) = parse_dec(&a[0]) else {
            return Ok(Value::Null);
        };
        let mut scale = 0;
        if a.len() > 1 {
            scale = as_int64(&a[1]);
        }
        Ok(Value::Text(dec_string(&dec_round(&r, scale))))
    })?;
    aggregate(conn, "dec_sum", 1, || Box::<DecSum>::default())?;
    // Postgres' round, abs and random over SQLite's: decimal.rs.
    scalar(conn, "round", 1, crate::builtins::decimal::pg_round)?;
    scalar(conn, "round", 2, crate::builtins::decimal::pg_round)?;
    // Their exact forms, which resolve/temporal.rs writes where a numeric is sent.
    scalar(conn, "overlite_round_exact", -1, crate::builtins::decimal::round_exact)?;
    scalar(conn, "overlite_abs_exact", 1, |a| {
        crate::builtins::decimal::abs_exact(&a[0])
    })?;
    // A double in [0, 1), where SQLite's random() is any 64-bit integer.
    scalar_nd(conn, "random", 0, |_| Ok(Value::Float(rand::random::<f64>())))?;
    // Override the built-in sum/avg so they are exact over numeric (decimal-text)
    // columns; int/real inputs keep their native return type.
    aggregate(conn, "sum", 1, || Box::<DecAgg>::default())?;
    aggregate(conn, "avg", 1, || {
        Box::new(DecAgg {
            avg: true,
            ..Default::default()
        })
    })?;

    register_srf_functions(conn)?;
    crate::builtins::pgfuncs::register(conn)?;
    crate::builtins::contrib::register(conn)?;
    for (name, n_arg) in [("overlite_json_elements", 2), ("overlite_json_pairs", 3)] {
        scalar(conn, name, n_arg, move |a| {
            let as_text = a[1].to_string() != "0";
            if n_arg == 2 {
                json_elements(&a[0], as_text)
            } else {
                json_pairs(&a[0], as_text, a[2].to_string() != "0")
            }
        })?;
    }
    scalar(conn, "overlite_json_keys", 2, |a| json_keys(&a[0], &a[1].to_string()))?;
    scalar(conn, "overlite_regexp_split", -1, regexp_split)?;
    scalar(conn, "regexp_split_to_array", -1, regexp_split)?;
    scalar(conn, "overlite_zip", -1, zip_arrays)?;
    scalar(conn, "overlite_series", -1, series)?;
    scalar(conn, "overlite_subscripts", -1, |a| Ok(subscripts(&a[0])))?;
    scalar(conn, "overlite_unsupported", 1, |a| {
        Err(Error::with_state(SqlState::FEATURE_NOT_SUPPORTED, a[0].to_string()))
    })?;

    crate::builtins::fts::register(conn)?;

    // What a value becomes on its way into a column of a type with a modifier.
    scalar(conn, "overlite_assign", 2, |a| assign(&a[0], &a[1].to_string()))?;
    // The patterns of LIKE and ILIKE: rewrite/like.rs in the postgres crate.
    scalar(conn, "overlite_like_glob", 2, |a| {
        crate::builtins::like::like_glob(&a[0], &a[1])
    })?;
    scalar(conn, "overlite_ilike_pattern", 2, |a| {
        crate::builtins::like::ilike_pattern(&a[0], &a[1])
    })?;
    // A key SQLite would fill in for a NULL: ddl/assign.rs in the postgres crate.
    scalar(conn, "overlite_not_null", 3, |a| {
        if a[0].is_null() {
            return Err(Error::with_state(
                SqlState::NOT_NULL_VIOLATION,
                format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    a[2], a[1]
                ),
            ));
        }
        Ok(a[0].clone())
    })?;
    // The storage format's conversion (storage.rs) reads and writes through these.
    scalar(conn, "overlite_storage_state", 2, |a| {
        Ok(Value::Int(crate::storage::storage_state(&a[0], &a[1].to_string())))
    })?;
    scalar(conn, "overlite_storage_canonical", 2, |a| {
        Ok(crate::storage::storage_canonical(&a[0], &a[1].to_string()))
    })?;

    // overlite_cast is what any other cast but one to text becomes: see builtins/casts.
    // A json or jsonb it gives is JSON to the function it is passed to, as Postgres'
    // is: json_array(overlite_cast(x, 'jsonb')) nests x, where text would be quoted.
    conn.create_scalar_function(
        "overlite_cast",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_RESULT_SUBTYPE,
        |ctx: &Context<'_>| {
            let args: Vec<Value> = (0..ctx.len()).map(|i| arg_value(ctx.get_raw(i))).collect();
            let name = args[1].to_string();
            let result = match CastType::parse(&name) {
                Some(ty) => cast(&args[0], &ty).map(|v| {
                    let json = matches!(ty, CastType::Json | CastType::Jsonb) && matches!(v, Value::Text(_));
                    (v, json)
                }),
                None if enum_type_exists(&name) => Ok((args[0].clone(), false)),
                None => Err(Error::with_state(
                    SqlState::UNDEFINED_OBJECT,
                    format!("type \"{name}\" does not exist"),
                )),
            };
            match result {
                // 'J', the subtype SQLite's JSON functions give their JSON.
                Ok((v, json)) => Ok((sql_value(v), json.then_some(74))),
                Err(e) => {
                    if let ErrorKind::State(state) = e.kind() {
                        set_function_state(state);
                    }
                    Err(rusqlite::Error::SqliteFailure(
                        ffi::Error::new(ffi::SQLITE_ERROR),
                        Some(e.into_message()),
                    ))
                }
            }
        },
    )
    .map_err(sqlite_error)?;

    // pg_bool is what a cast to boolean becomes: SQLite's CAST to the BOOLEAN
    // affinity reads every word as 0, so 'true'::boolean was false.
    scalar(conn, "pg_bool", 1, |a| {
        let s = match &a[0] {
            Value::Null => return Ok(Value::Null),
            Value::Bool(v) => return int(*v),
            Value::Int(v) => return int(*v != 0),
            Value::Float(v) => return int(*v != 0.0),
            Value::Blob(v) => String::from_utf8_lossy(v).into_owned(),
            Value::Text(v) => v.clone(),
            other @ Value::Time(_) => other.to_string(),
        };
        match parse_bool(&s) {
            Some(b) => int(b),
            None => Err(Error::with_state(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("invalid input syntax for type boolean: \"{s}\""),
            )),
        }
    })?;

    // A boolean column's value whatever it holds: a stored 'true' or 't' as 1, as an
    // integer is (resolve/booleans.rs). What no boolean reads as is NULL, unknown.
    scalar(conn, "overlite_bool", 1, |a| {
        Ok(match stored_bool(&a[0]) {
            Some(b) => Value::Int(bool_int(b)),
            None => Value::Null,
        })
    })?;
    // The values a boolean column may hold for the truth value of a cast to boolean,
    // as a JSON array for `col IN (SELECT value FROM json_each(...))`.
    scalar(conn, "overlite_bool_forms", 1, |a| {
        let b = match &a[0] {
            Value::Null => None,
            Value::Text(s) => Some(parse_bool(s).ok_or_else(|| {
                Error::with_state(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    format!("invalid input syntax for type boolean: \"{s}\""),
                )
            })?),
            v => stored_bool(v),
        };
        text(&bool_forms_json(b))
    })?;

    scalar(conn, "version", 0, |_| {
        text("PostgreSQL 16.0 (overlite) on overlite, compiled by overlite")
    })?;
    scalar(conn, "current_schema", 0, |_| text("public"))?;
    scalar(conn, "current_schemas", -1, |_| text("{pg_catalog,public}"))?;
    // current_database() is resolved per statement (resolve_current_database): a
    // process serves many databases, and a registered function cannot tell which
    // one is asking. This is the fallback for a caller that reaches SQLite without
    // going through the rewrite.
    scalar(conn, "current_database", 0, |_| text("main"))?;
    scalar(conn, "current_user", 0, |_| text(&CATALOG_ROLE))?;
    scalar(conn, "session_user", 0, |_| text(&CATALOG_ROLE))?;
    scalar(conn, "pg_get_userbyid", 1, |_| text(&CATALOG_ROLE))?;
    scalar(conn, "pg_table_is_visible", 1, |_| Ok(Value::Int(1)))?;
    // pg_get_expr(expr, relation[, pretty]) renders a stored expression. Our
    // pg_attrdef keeps the default's SQL text as adbin, so returning the first
    // argument yields the column default for pg_dump; other callers pass NULL.
    scalar(conn, "pg_get_expr", -1, |args| {
        if args.is_empty() || args[0].is_null() {
            return Ok(Value::Null); // NULL in -> NULL out (e.g. a policy with no WITH CHECK)
        }
        Ok(Value::Text(args[0].to_string()))
    })?;

    // Definition-rendering helpers psql calls in \d; we don't reconstruct DDL yet,
    // so they return empty strings (variadic arg counts via nArg -1).
    for name in [
        "pg_get_indexdef",
        "pg_get_constraintdef",
        "pg_get_viewdef",
        "pg_get_partkeydef",
        "pg_get_ruledef",
    ] {
        scalar(conn, name, -1, empty)?;
    }
    // pg_get_function_sqlbody returns NULL so pg_dump emits the classic AS $$ prosrc
    // $$ form (SQL-standard bodies aren't stored).
    scalar(conn, "pg_get_function_sqlbody", -1, null)?;
    // pg_get_triggerdef renders a trigger's CREATE statement (from the registry
    // refreshed per connection); the first argument is the trigger oid.
    scalar(conn, "pg_get_triggerdef", -1, |args| {
        if args.is_empty() || args[0].is_null() {
            return text("");
        }
        Ok(Value::Text(lookup_trigger_def(as_int64(&args[0]))))
    })?;
    scalar(conn, "pg_relation_is_publishable", -1, |_| Ok(Value::Int(0)))?;
    scalar(conn, "pg_function_is_visible", 1, |_| Ok(Value::Int(1)))?;
    scalar(conn, "pg_type_is_visible", 1, |_| Ok(Value::Int(1)))?;
    // obj_description/col_description look up comments; we store none, so NULL.
    for name in ["obj_description", "col_description", "shobj_description"] {
        scalar(conn, name, -1, null)?;
    }
    scalar(conn, "pg_encoding_to_char", 1, |_| text("UTF8"))?;
    scalar(conn, "overlite_array_text", 1, |a| {
        Ok(crate::builtins::arrays::array_text(&a[0]))
    })?;
    scalar(conn, "array_to_string", -1, |a| {
        Ok(crate::builtins::arrays::array_to_string(a))
    })?;
    scalar(conn, "pg_get_functiondef", -1, fn_field(|f| f.src.clone()))?;
    scalar(conn, "pg_get_function_arguments", -1, fn_field(|f| f.args.clone()))?;
    scalar(conn, "pg_get_function_result", -1, fn_field(|f| f.rettype.clone()))?;
    scalar(
        conn,
        "pg_get_function_identity_arguments",
        -1,
        fn_field(|f| identity_args(&f.args)),
    )?;
    {
        use crate::builtins::arrays as a;
        scalar(conn, "array_cat", 2, |v| Ok(a::array_cat(&v[0], &v[1], "aa")))?;
        scalar(conn, "array_append", 2, |v| Ok(a::array_cat(&v[0], &v[1], "ae")))?;
        scalar(conn, "array_prepend", 2, |v| Ok(a::array_cat(&v[0], &v[1], "ea")))?;
        scalar(conn, "array_remove", 2, |v| Ok(a::array_remove(v)))?;
        scalar(conn, "array_replace", 3, |v| Ok(a::array_replace(v)))?;
        scalar(conn, "array_position", -1, |v| Ok(a::array_position(v)))?;
        scalar(conn, "array_positions", 2, |v| Ok(a::array_positions(v)))?;
        scalar(conn, "array_ndims", 1, |v| Ok(a::array_ndims(v)))?;
        scalar(conn, "array_sort", -1, |v| Ok(a::array_sort(v)))?;
        scalar(conn, "array_reverse", 1, |v| Ok(a::array_reverse(v)))?;
    }
    scalar(conn, "array_length", -1, arr_len)?;
    scalar(conn, "array_upper", -1, arr_len)?;
    scalar(conn, "array_lower", -1, |args| {
        if !args.is_empty()
            && let Some(n) = json_array_len(&args[0])
            && n > 0
        {
            return Ok(Value::Int(1));
        }
        Ok(Value::Null)
    })?;
    // AT TIME ZONE: convert a stored (UTC) instant to a wall clock in the zone. The
    // names are for what is known to be one and for a column the engine may find out
    // is a timestamp.
    for name in [
        "overlite_at_time_zone",
        "overlite_instant_at_time_zone",
        "overlite_column_at_time_zone",
    ] {
        scalar(conn, name, 2, |args| {
            if args.len() != 2 || args[0].is_null() || args[1].is_null() {
                return Ok(Value::Null);
            }
            match at_time_zone(&args[0].to_string(), &args[1].to_string()) {
                Some(w) => Ok(Value::Text(w)),
                None => Ok(Value::Null),
            }
        })?;
    }
    // AT TIME ZONE on a timestamp: read a wall clock in the zone as an instant.
    scalar(conn, "overlite_from_time_zone", 2, |args| {
        if args.len() != 2 || args[0].is_null() || args[1].is_null() {
            return Ok(Value::Null);
        }
        match from_time_zone(&args[0].to_string(), &args[1].to_string()) {
            Some(t) => Ok(Value::Text(t)),
            None => Ok(Value::Null),
        }
    })?;
    scalar(conn, "cardinality", 1, |args| {
        if !args.is_empty()
            && let Some(n) = json_array_len(&args[0])
        {
            return Ok(Value::Int(n));
        }
        Ok(Value::Int(0))
    })?;
    // The array that unnest and generate_subscripts in a select list step through
    // (whatever form it is stored in), and its first subscript.
    scalar(conn, "overlite_array_json", 1, |a| Ok(array_as_json(&a[0])))?;
    // Two arrays in Postgres' order, -1, 0 or 1, which the AST path writes for a
    // comparison of arrays; NULL over NULL and over what is no array.
    scalar(conn, "overlite_array_cmp", 2, |a| {
        if a[0].is_null() || a[1].is_null() {
            return Ok(Value::Null);
        }
        Ok(crate::builtins::arrays::array_cmp(&a[0], &a[1]).map_or(Value::Null, |o| Value::Int(i64::from(o as i8))))
    })?;
    // ARRAY(subquery), whose value is the JSON array the rewrite builds: the call
    // marks the result column an array.
    scalar(conn, "overlite_array", 1, |a| Ok(a[0].clone()))?;
    scalar(conn, "overlite_array_slice", 3, |a| {
        Ok(crate::builtins::arrays::array_slice(a))
    })?;
    // `a || b` of arrays, or of an array and an element, by which side is which.
    scalar(conn, "overlite_array_cat", 3, |a| {
        Ok(crate::builtins::arrays::array_cat(&a[0], &a[1], &a[2].to_string()))
    })?;
    // A covering index's INCLUDE columns: 0 the list as pg_get_indexdef writes it, 1 a
    // JSON array of the names.
    scalar(conn, "overlite_index_include", 2, |a| {
        let Some((_, names)) = index_include(&arg_str(&a[0])) else {
            return Ok(Value::Null);
        };
        if matches!(a[1], Value::Int(0)) {
            let plain = |n: &str| {
                n.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
                    && n.bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
            };
            let list: Vec<String> = names
                .iter()
                .map(|n| {
                    if plain(n) {
                        n.clone()
                    } else {
                        ["\"", &n.replace('"', "\"\""), "\""].concat()
                    }
                })
                .collect();
            return Ok(Value::Text(list.join(", ")));
        }
        let quoted: Vec<String> = names
            .iter()
            .map(|n| ["\"", &n.replace('\\', "\\\\").replace('"', "\\\""), "\""].concat())
            .collect();
        Ok(Value::Text(["[", &quoted.join(","), "]"].concat()))
    })?;
    scalar(conn, "overlite_index_null_key", 2, |a| {
        let Value::Int(seqno) = a[1] else {
            return Ok(Value::Null);
        };
        Ok(index_null_key_column(&arg_str(&a[0]), seqno).map_or(Value::Null, Value::Text))
    })?;
    scalar(conn, "overlite_index_where", 1, |a| {
        Ok(index_where(&arg_str(&a[0])).map_or(Value::Null, Value::Text))
    })?;
    scalar(conn, "overlite_array_lower", 1, |a| Ok(array_lower(&a[0])))?;
    // array_agg is a real aggregate (pg_dump collects index/constraint columns
    // with it); it builds a Postgres array literal {a,b,c}.
    aggregate(conn, "array_agg", -1, || Box::<ArrayAgg>::default())?;

    // Table and column privileges answer from the grants, for the session's role.
    reading(conn, "has_table_privilege", grants::has_table_privilege)?;
    reading(conn, "has_column_privilege", grants::has_column_privilege)?;
    reading(conn, "has_any_column_privilege", grants::has_any_column_privilege)?;
    reading(conn, "overlite_role_visible", grants::role_visible)?;
    scalar(conn, "overlite_aclitem", 3, grants::aclitem)?;
    // The other access-privilege checks: DBeaver filters the object tree with these;
    // objects other than tables have no grants here, so everything is granted.
    for name in [
        "has_schema_privilege",
        "has_database_privilege",
        "has_sequence_privilege",
        "has_function_privilege",
        "has_type_privilege",
        "has_language_privilege",
        "pg_has_role",
    ] {
        scalar(conn, name, -1, |_| Ok(Value::Int(1)))?;
    }
    // Partition helpers: we have no partitioned tables, so these return NULL/empty
    // (partition sections of \d then render as absent).
    for name in ["pg_partition_ancestors", "pg_partition_root", "pg_partition_tree"] {
        scalar(conn, name, -1, null)?;
    }
    scalar(conn, "pg_get_partition_constraintdef", -1, empty)?;
    for name in ["pg_get_statisticsobjdef", "pg_get_statisticsobjdef_columns"] {
        scalar(conn, name, -1, empty)?;
    }
    // ACL helpers pg_dump calls; we don't model privileges, so return NULL/empty.
    for name in ["acldefault", "aclexplode", "aclitemin", "aclitemout"] {
        scalar(conn, name, -1, null)?;
    }
    scalar(conn, "pg_is_in_recovery", 0, |_| Ok(Value::Int(0)))?;
    scalar(conn, "pg_backend_pid", 0, |_| Ok(Value::Int(1)))?;
    scalar(conn, "current_setting", -1, |args| {
        if args.is_empty() {
            return text("");
        }
        let name = args[0].to_string();
        if name.eq_ignore_ascii_case("timezone") {
            return Ok(Value::Text(crate::builtins::dates::session_zone()));
        }
        text(current_setting(&name))
    })?;
    // set_config: TimeZone is applied, as SET TIME ZONE is (Django sets it so); the
    // other settings are echoed and not applied.
    scalar_nd(conn, "set_config", 3, |args| {
        if args.len() < 2 {
            return text("");
        }
        if args[0].to_string().eq_ignore_ascii_case("timezone") {
            let local = matches!(&args[2], Value::Int(1)) || crate::catalog::truthy(&args[2]);
            return Ok(Value::Text(crate::builtins::dates::set_zone_from_sql(
                &args[1].to_string(),
                local,
            )?));
        }
        Ok(Value::Text(args[1].to_string()))
    })?;

    scalar(conn, "quote_ident", 1, |args| {
        if args.is_empty() || args[0].is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Text(quote_ident_if_needed(&args[0].to_string())))
    })?;
    scalar(conn, "quote_literal", 1, |args| {
        if args.is_empty() || args[0].is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Text(format!("'{}'", args[0].to_string().replace('\'', "''"))))
    })?;

    scalar(conn, "format_type", 2, |args| {
        if args.is_empty() {
            return Ok(Value::Null);
        }
        let oid = as_int64(&args[0]);
        if let Some(name) = lookup_enum_name(oid) {
            return Ok(Value::Text(name));
        }
        let mut typmod = -1;
        if args.len() > 1 && !args[1].is_null() {
            typmod = as_int64(&args[1]);
        }
        Ok(Value::Text(format_type_mod(oid, typmod)))
    })?;
    scalar(conn, "overlite_type_oid", 1, |args| {
        if args.is_empty() {
            return Ok(Value::Int(TYPE_OID_TEXT));
        }
        Ok(Value::Int(sqlite_type_oid(&args[0].to_string())))
    })?;
    // The enum a column was declared with, recovered from its CHECK; see
    // enum_labels_from_ddl. NULL when the column is not an enum.
    scalar(conn, "overlite_enum_labels", 2, |a| {
        if a.len() < 2 || a[0].is_null() || a[1].is_null() {
            return Ok(Value::Null);
        }
        let l = enum_labels_from_ddl(&arg_str(&a[0]), &arg_str(&a[1]));
        if !l.is_empty() {
            return Ok(Value::Text(l));
        }
        Ok(Value::Null)
    })?;
    // Declared type modifiers for information_schema.columns.
    scalar(conn, "overlite_char_max_length", 1, |a| {
        Ok(type_char_max_length(&arg_str(&a[0])))
    })?;
    scalar(conn, "overlite_numeric_precision", 1, |a| {
        Ok(type_numeric_precision(&arg_str(&a[0])))
    })?;
    scalar(conn, "overlite_numeric_scale", 1, |a| {
        Ok(type_numeric_scale(&arg_str(&a[0])))
    })?;
    scalar(conn, "overlite_numeric_precision_radix", 1, |a| {
        Ok(type_numeric_precision_radix(&arg_str(&a[0])))
    })?;
    scalar(conn, "overlite_datetime_precision", 1, |a| {
        Ok(type_datetime_precision(&arg_str(&a[0])))
    })?;
    scalar(conn, "overlite_typmod", 1, |a| {
        Ok(Value::Int(type_att_typmod(&arg_str(&a[0]))))
    })?;
    // Constraint names and index opclasses as Postgres reports them; see
    // constraints.rs. The column list is comma-separated, in key order.
    scalar(conn, "overlite_fk_name", 5, |a| {
        let cols: Vec<String> = arg_str(&a[2]).split(',').map(str::to_string).collect();
        Ok(Value::Text(fk_constraint_name(
            &arg_str(&a[0]),
            &arg_str(&a[1]),
            &cols,
            &arg_str(&a[3]),
            as_int64(&a[4]),
        )))
    })?;
    // overlite_fk_deferral(sql, cols, ref_table, id): 0, 1 deferrable, 2 initially deferred.
    scalar(conn, "overlite_fk_deferral", 4, |a| {
        let cols: Vec<String> = arg_str(&a[1]).split(',').map(str::to_string).collect();
        Ok(Value::Int(fk_deferral(
            &arg_str(&a[0]),
            &cols,
            &arg_str(&a[2]),
            as_int64(&a[3]),
        )))
    })?;
    scalar(conn, "overlite_checks", 2, |a| {
        Ok(Value::Text(check_constraints(&arg_str(&a[0]), &arg_str(&a[1]))))
    })?;
    scalar(conn, "overlite_unique_name", 3, |a| {
        let cols: Vec<String> = arg_str(&a[2]).split(',').map(str::to_string).collect();
        Ok(Value::Text(unique_constraint_name(
            &arg_str(&a[0]),
            &arg_str(&a[1]),
            &cols,
        )))
    })?;
    scalar(conn, "overlite_unique_constraints", 3, |a| {
        let cols: Vec<String> = arg_str(&a[2]).split(',').map(str::to_string).collect();
        Ok(Value::Text(unique_constraints(&arg_str(&a[0]), &arg_str(&a[1]), &cols)))
    })?;
    scalar(conn, "overlite_unique_deferral", 2, |a| {
        let cols: Vec<String> = arg_str(&a[1]).split(',').map(str::to_string).collect();
        Ok(Value::Int(unique_deferral(&arg_str(&a[0]), &cols)))
    })?;
    scalar(conn, "overlite_column_def", 2, |a| {
        Ok(
            crate::builtins::collation::column_item(&arg_str(&a[0]), &arg_str(&a[1]))
                .map_or(Value::Null, |i| Value::Text(i.to_string())),
        )
    })?;
    scalar(conn, "overlite_column_collation", 2, |a| {
        Ok(
            crate::builtins::collation::column_collation(&arg_str(&a[0]), &arg_str(&a[1]))
                .map_or(Value::Null, Value::Text),
        )
    })?;
    scalar(conn, "overlite_pk_name", 2, |a| {
        Ok(Value::Text(pk_constraint_name(&arg_str(&a[0]), &arg_str(&a[1]))))
    })?;
    scalar(conn, "overlite_attidentity", 2, |a| {
        Ok(Value::Text(attidentity(&arg_str(&a[0]), &arg_str(&a[1])).to_string()))
    })?;
    // The EXCLUDE constraint a trigger of ddl/exclude.rs carries, as JSON.
    scalar(conn, "overlite_exclusion", 1, |a| {
        let sql = arg_str(&a[0]);
        let Some(at) = sql.find("/*overlite exclude ") else {
            return Ok(Value::Null);
        };
        let rest = &sql[at + "/*overlite exclude ".len()..];
        Ok(rest
            .find("*/")
            .map_or(Value::Null, |end| Value::Text(rest[..end].to_string())))
    })?;
    scalar(conn, "overlite_identity_type", 2, |a| {
        Ok(identity_type(&arg_str(&a[0]), &arg_str(&a[1])).map_or(Value::Null, Value::Text))
    })?;
    scalar(conn, "overlite_index_opclass", 2, |a| {
        Ok(index_opclass(&arg_str(&a[0]), as_int64(&a[1])).map_or(Value::Null, Value::Text))
    })?;
    scalar(conn, "overlite_opclass", 1, |a| {
        Ok(Value::Int(default_opclass_oid(&arg_str(&a[0]))))
    })?;

    // jsonb containment (@> / <@, rewritten to json_contains in the dialect); the
    // same operator on a range checks range containment.
    // GREATEST and LEAST skip NULLs, where SQLite's max(a, b) is NULL with one.
    scalar(conn, "greatest", -1, |args| {
        Ok(extreme(args, std::cmp::Ordering::Greater))
    })?;
    scalar(conn, "least", -1, |args| Ok(extreme(args, std::cmp::Ordering::Less)))?;
    // The decimal text of a numeric, which resolve/numeric.rs puts in a CAST: a double
    // by its shortest digits, as Postgres casts one.
    scalar(conn, "overlite_numeric", 1, |a| {
        Ok(match &a[0] {
            Value::Int(n) => Value::Text(n.to_string()),
            Value::Float(f) if f.is_finite() => Value::Text(format!("{f}")),
            Value::Float(f) if f.is_nan() => Value::Text("NaN".to_string()),
            Value::Float(f) => Value::Text(if *f > 0.0 { "Infinity" } else { "-Infinity" }.to_string()),
            other => other.clone(),
        })
    })?;
    // Over numerics, which resolve/numeric.rs names: the arguments compare as decimals.
    for (name, want) in [("dec_greatest", 1), ("dec_least", -1)] {
        scalar(conn, name, -1, move |args| {
            let mut best: Option<&Value> = None;
            for v in args.iter().filter(|v| !v.is_null()) {
                if best.is_none_or(|b| crate::builtins::decimal::dec_cmp_str(&v.to_string(), &b.to_string()) == want) {
                    best = Some(v);
                }
            }
            Ok(best.cloned().unwrap_or(Value::Null))
        })?;
    }
    scalar(conn, "json_contains", 2, |args| {
        if args.len() < 2 || args[0].is_null() || args[1].is_null() {
            return Ok(Value::Int(0));
        }
        let (a, b) = (args[0].to_string(), args[1].to_string());
        let mut ok = jsonb_contains(&a, &b);
        // A range is range-text that isn't valid JSON ("[1,10)"); a JSON array
        // ("[1,2,3]") stays jsonb containment.
        if is_range_text(&a) && !is_json_array_text(&a) {
            if is_range_text(&b) && !is_json_array_text(&b) {
                ok = range_contains_range(&a, &b); // range @> range
            } else {
                ok = range_contains(&a, &b); // range @> scalar element
            }
        }
        int(ok)
    })?;
    // Overlap operator "&&": ranges that share a point, or arrays with a common
    // element.
    scalar(conn, "overlite_overlaps", 2, |args| {
        if args.len() < 2 || args[0].is_null() || args[1].is_null() {
            return Ok(Value::Int(0));
        }
        let (a, b) = (args[0].to_string(), args[1].to_string());
        // Two JSON arrays -> array overlap; otherwise two ranges -> range overlap.
        if is_json_array_text(&a) && is_json_array_text(&b) {
            return int(json_arrays_overlap(&a, &b).0);
        }
        int(is_range_text(&a) && is_range_text(&b) && range_overlaps(&a, &b))
    })?;

    // Range constructors (2 or 3 args), each giving its type's canonical form.
    for kind in [
        RangeKind::Int4,
        RangeKind::Int8,
        RangeKind::Num,
        RangeKind::Date,
        RangeKind::Ts,
        RangeKind::Tstz,
    ] {
        scalar(conn, kind.name(), -1, move |a| {
            if a.len() < 2 {
                return Ok(Value::Null);
            }
            let flags = a
                .get(2)
                .filter(|f| !f.is_null())
                .map_or("[)".to_string(), ToString::to_string);
            Ok(Value::Text(range_construct(kind, &a[0], &a[1], &flags)?))
        })?;
    }
    // A range operator: overlite_range_op('-|-', a, b). NULL over NULL, and over what
    // is no range.
    scalar(conn, "overlite_range_op", 3, |a| {
        if a.len() < 3 || a.iter().any(Value::is_null) {
            return Ok(Value::Null);
        }
        Ok(range_op(&a[0].to_string(), &a[1].to_string(), &a[2].to_string())
            .map_or(Value::Null, |b| Value::Int(bool_int(b))))
    })?;
    range_acc(conn, "isempty", |r| Value::Int(bool_int(r.empty)))?;
    range_acc(conn, "lower_inc", |r| Value::Int(bool_int(!r.empty && r.lo_inc)))?;
    range_acc(conn, "upper_inc", |r| Value::Int(bool_int(!r.empty && r.hi_inc)))?;
    range_acc(conn, "lower_inf", |r| Value::Int(bool_int(!r.empty && r.lo.is_none())))?;
    range_acc(conn, "upper_inf", |r| Value::Int(bool_int(!r.empty && r.hi.is_none())))?;
    // A tstzrange's text in the session's zone, as a cast to text writes it.
    scalar(conn, "overlite_tstzrange_text", 1, |a| {
        Ok(match &a[0] {
            Value::Null => Value::Null,
            v => crate::builtins::ranges::tstzrange_session_text(&v.to_string()).map_or_else(|| v.clone(), Value::Text),
        })
    })?;
    // A range's lower (0) or upper (1) bound, as the AST path writes lower and upper of
    // a range: NULL for the empty range and an infinite bound.
    scalar(conn, "overlite_range_bound", 2, |a| {
        if a[0].is_null() {
            return Ok(Value::Null);
        }
        let Some(r) = parse_range(&a[0].to_string()) else {
            return Ok(Value::Null);
        };
        if r.empty {
            return Ok(Value::Null);
        }
        Ok(bound_value(if matches!(a[1], Value::Int(1)) {
            r.hi.as_ref()
        } else {
            r.lo.as_ref()
        }))
    })?;
    // lower/upper are overloaded: a range yields its bound, otherwise the string
    // case functions (which SQLite provides). The word `empty` stays a string: as text
    // it is far likelier than the empty range.
    scalar(conn, "lower", 1, |a| {
        if a.is_empty() || a[0].is_null() {
            return Ok(Value::Null);
        }
        let s = a[0].to_string();
        if let Some(r) = parse_range(&s).filter(|r| !r.empty) {
            return Ok(bound_value(r.lo.as_ref()));
        }
        Ok(Value::Text(lower_chars(&s)))
    })?;
    scalar(conn, "upper", 1, |a| {
        if a.is_empty() || a[0].is_null() {
            return Ok(Value::Null);
        }
        let s = a[0].to_string();
        if let Some(r) = parse_range(&s).filter(|r| !r.empty) {
            return Ok(bound_value(r.hi.as_ref()));
        }
        Ok(Value::Text(upper_chars(&s)))
    })?;
    // jsonb key existence (? / ?| / ?&, rewritten from those operators), NULL over
    // NULL as a strict operator is, so a CHECK lets a NULL through.
    scalar(conn, "jsonb_exists", 2, |a| {
        if a.len() < 2 || a[0].is_null() || a[1].is_null() {
            return Ok(Value::Null);
        }
        int(jsonb_exists(&a[0].to_string(), &a[1].to_string()))
    })?;
    scalar(conn, "jsonb_exists_any", 2, |a| {
        if a.len() < 2 || a[0].is_null() || a[1].is_null() {
            return Ok(Value::Null);
        }
        int(jsonb_exists_any(&a[0].to_string(), &a[1].to_string()))
    })?;
    scalar(conn, "jsonb_exists_all", 2, |a| {
        if a.len() < 2 || a[0].is_null() || a[1].is_null() {
            return Ok(Value::Null);
        }
        int(jsonb_exists_all(&a[0].to_string(), &a[1].to_string()))
    })?;

    // hstore (stored as a JSON object; see hstore.rs).
    scalar(conn, "hstore_in", 1, |a| {
        if a.is_empty() || a[0].is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Text(hstore_in(&a[0].to_string())))
    })?;
    scalar(conn, "hstore", 2, |a| {
        if a.len() < 2 || a[0].is_null() {
            return Ok(Value::Null);
        }
        let v = if a[1].is_null() {
            "null".to_string()
        } else {
            json_quote(&a[1].to_string())
        };
        Ok(Value::Text(format!("{{{}:{}}}", json_quote(&a[0].to_string()), v)))
    })?;
    // hstore's `->` (ast/emit.rs, hstore_op).
    scalar(conn, "overlite_hstore_get", 2, |a| {
        if a.len() < 2 || a[0].is_null() || a[1].is_null() {
            return Ok(Value::Null);
        }
        Ok(hstore_get(&a[0].to_string(), &a[1].to_string()).map_or(Value::Null, Value::Text))
    })?;
    scalar(conn, "akeys", 1, |a| {
        if a.is_empty() || a[0].is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Text(hstore_keys(&a[0].to_string())))
    })?;
    scalar(conn, "avals", 1, |a| {
        if a.is_empty() || a[0].is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Text(hstore_vals(&a[0].to_string())))
    })?;
    // hstore is already JSON, so hstore_to_json[b] is the identity.
    for name in ["hstore_to_json", "hstore_to_jsonb"] {
        scalar(conn, name, 1, |a| {
            if a.is_empty() {
                return Ok(Value::Null);
            }
            Ok(a[0].clone())
        })?;
    }

    // Date/time functions (now()/extract are handled in the dialect layer).
    scalar(conn, "date_trunc", 2, date_trunc_fn)?;
    // + and - over dates, times, timestamps and intervals: resolve/temporal.rs.
    // A timestamptz as text in the session's zone, which resolve/temporal.rs writes for
    // a cast of one to text.
    scalar(conn, "overlite_tz_text", 1, |a| {
        Ok(match &a[0] {
            Value::Null => Value::Null,
            v => crate::builtins::dates::session_tz_text(&v.to_string()).map_or_else(|| v.clone(), Value::Text),
        })
    })?;
    // A timestamptz's wall clock in the session's zone, which the AST path writes where
    // Postgres reads one there (a cast to a date or a time, EXTRACT, DATE_TRUNC): the
    // value itself in UTC.
    scalar(conn, "overlite_tz_wall", 1, |a| {
        if a[0].is_null() || crate::builtins::dates::session_is_utc() {
            return Ok(a[0].clone());
        }
        let zone = crate::builtins::dates::session_zone();
        Ok(at_time_zone(&a[0].to_string(), &zone).map_or_else(|| a[0].clone(), Value::Text))
    })?;
    // A timestamptz a client gives as text, read in the session's zone: resolve/temporal.rs
    // writes it around what a cast or a write to timestamptz takes, but a timestamptz.
    // An array's elements so too: a timestamp[] or an array's text meeting a
    // timestamptz[] (a JSON array or `{...}`, which no timestamp's text starts with).
    scalar(conn, "overlite_tz_in", 1, |a| match &a[0] {
        Value::Text(t) if t.trim_start().starts_with(['[', '{']) => {
            crate::builtins::canonical::zoned_timestamptz_array(&a[0])
        }
        v => crate::builtins::casts::timestamptz_in_session(v, None),
    })?;
    // CURRENT_DATE in the session's zone, which resolve/temporal.rs writes for it.
    scalar_nd(conn, "overlite_current_date", 0, |_| {
        Ok(Value::Text(crate::builtins::dates::session_today()))
    })?;
    scalar(conn, "overlite_tmul", 5, crate::builtins::temporal::temporal_mul)?;
    // Two intervals in Postgres' order: -1, 0 or 1, NULL over NULL.
    scalar(conn, "overlite_interval_cmp", 2, |a| {
        if a[0].is_null() || a[1].is_null() {
            return Ok(Value::Null);
        }
        let read = |v: &Value| crate::builtins::casts::parse_interval(&v.to_string());
        let (Some(x), Some(y)) = (read(&a[0]), read(&a[1])) else {
            return Err(Error::with_state(
                SqlState::INVALID_DATETIME_FORMAT,
                format!("invalid input syntax for type interval: \"{}\"", a[0]),
            ));
        };
        Ok(Value::Int(match crate::builtins::temporal::interval_cmp(&x, &y) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }))
    })?;
    scalar(conn, "overlite_tadd", 4, |a| {
        crate::builtins::temporal::temporal_op(a, false)
    })?;
    scalar(conn, "overlite_tsub", 4, |a| {
        crate::builtins::temporal::temporal_op(a, true)
    })?;
    scalar(conn, "date_part", 2, |a| {
        crate::builtins::temporal::extract(&a[0], &a[1], false)
    })?;
    // now() and its kin: rewrite/datetime.rs in the postgres crate.
    // now(): the transaction's start in one, else the statement's; with a second
    // argument, statement_timestamp(), the statement's always.
    scalar(conn, "overlite_now", -1, |a| {
        if a.len() == 1
            && let Some(start) = crate::builtins::temporal::transaction_start()
        {
            return Ok(start);
        }
        Ok(crate::builtins::temporal::now_from_julian(&a[0]))
    })?;
    scalar_nd(conn, "pg_sleep", 1, |a| {
        let secs = a[0].to_string().trim().parse::<f64>().unwrap_or(0.0).clamp(0.0, 3600.0);
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
        Ok(Value::Text(String::new()))
    })?;
    scalar_nd(conn, "overlite_clock_timestamp", 0, |_| {
        Ok(crate::builtins::temporal::clock_timestamp())
    })?;
    // EXTRACT, which gives a numeric since Postgres 14.
    scalar(conn, "overlite_extract", 2, |a| {
        crate::builtins::temporal::extract(&a[0], &a[1], true)
    })?;
    scalar(conn, "to_char", 2, to_char_fn)?;
    scalar(conn, "age", -1, age_fn)?;

    scalar(conn, "regexp", 2, |args| {
        if args.len() != 2 {
            return Err(errf!(SqlState::INVALID_REGULAR_EXPRESSION, "regexp: want 2 args"));
        }
        let m = compile_regex(&args[0].to_string())?;
        let ok = m.is_match(&args[1].to_string())?;
        int(ok)
    })?;
    Ok(())
}

// Postgres type OIDs used by the emulated pg_type / atttypid mapping.
const TYPE_OID_BOOL: i64 = 16;
const TYPE_OID_BYTEA: i64 = 17;
const TYPE_OID_INT8: i64 = 20;
const TYPE_OID_INT2: i64 = 21;
const TYPE_OID_INT4: i64 = 23;
const TYPE_OID_TEXT: i64 = 25;
const TYPE_OID_FLOAT8: i64 = 701;
const TYPE_OID_VARCHAR: i64 = 1043;
const TYPE_OID_NUMERIC: i64 = 1700;
const TYPE_OID_DATE: i64 = 1082;
const TYPE_OID_TIMESTAMP: i64 = 1114;
const TYPE_OID_TIMESTAMPTZ: i64 = 1184;
const TYPE_OID_JSON: i64 = 114;
const TYPE_OID_JSONB: i64 = 3802;
const TYPE_OID_UUID: i64 = 2950;

/// Array types: the element's OID, the array's, and the array's name in pg_type.
pub(crate) const ARRAY_TYPES: [(i64, i64, &str); 15] = [
    (TYPE_OID_BOOL, 1000, "_bool"),
    (TYPE_OID_BYTEA, 1001, "_bytea"),
    (TYPE_OID_INT8, 1016, "_int8"),
    (TYPE_OID_INT2, 1005, "_int2"),
    (TYPE_OID_INT4, 1007, "_int4"),
    (TYPE_OID_TEXT, 1009, "_text"),
    (TYPE_OID_FLOAT8, 1022, "_float8"),
    (TYPE_OID_VARCHAR, 1015, "_varchar"),
    (TYPE_OID_DATE, 1182, "_date"),
    (TYPE_OID_TIMESTAMP, 1115, "_timestamp"),
    (TYPE_OID_TIMESTAMPTZ, 1185, "_timestamptz"),
    (TYPE_OID_NUMERIC, 1231, "_numeric"),
    (TYPE_OID_JSON, 199, "_json"),
    (TYPE_OID_JSONB, 3807, "_jsonb"),
    (TYPE_OID_UUID, 2951, "_uuid"),
];

/// Maps a SQLite declared type to a Postgres type OID using SQLite's affinity
/// rules (substring matching).
fn sqlite_type_oid(decl: &str) -> i64 {
    if is_array_decl(decl) {
        let elem = sqlite_type_oid(element_decl(decl));
        let array = ARRAY_TYPES.iter().find(|(e, ..)| *e == elem);
        return array.map_or(1009, |(_, arr, _)| *arr);
    }
    let d = upper_chars(decl);
    let has = |s: &str| d.contains(s);
    if has("DECIMALTEXT") {
        // overlite's exact-numeric storage type
        TYPE_OID_NUMERIC
    } else if has("UUID") {
        TYPE_OID_UUID
    } else if has("JSONB") {
        TYPE_OID_JSONB
    } else if has("JSON") {
        TYPE_OID_JSON
    // The integer widths are separate types to a client: Prisma reads an int8
    // column as BigInt and an int4 as Int, so a BIGINT must not come back int4.
    } else if has("BIGINT") || has("INT8") || has("BIGSERIAL") {
        TYPE_OID_INT8
    } else if has("SMALLINT") || has("INT2") || has("SMALLSERIAL") {
        TYPE_OID_INT2
    } else if has("INT") {
        TYPE_OID_INT4
    } else if has("VARCHAR") || has("CHARACTER VARYING") {
        TYPE_OID_VARCHAR
    } else if has("CHAR") || has("CLOB") || has("TEXT") {
        TYPE_OID_TEXT
    } else if has("BLOB") || d.trim() == "BYTEA" {
        TYPE_OID_BYTEA
    } else if has("REAL") || has("FLOA") || has("DOUB") {
        TYPE_OID_FLOAT8
    } else if has("BOOL") {
        TYPE_OID_BOOL
    } else if has("NUMERIC") || has("DEC") {
        TYPE_OID_NUMERIC
    // TIMESTAMPTZ carries a zone; TIMESTAMP does not. They are different types
    // to a client, which reads the column's type to decide how to parse it.
    } else if has("TIMESTAMPTZ") || (has("TIMESTAMP") && has("WITH TIME ZONE")) {
        TYPE_OID_TIMESTAMPTZ
    } else if has("TIMESTAMP") || has("DATETIME") {
        TYPE_OID_TIMESTAMP
    } else if has("DATE") {
        TYPE_OID_DATE
    } else if has("TIME") {
        TYPE_OID_TIMESTAMP
    } else {
        TYPE_OID_TEXT
    }
}

/// Renders a type OID as Postgres' canonical display name, as format_type()
/// would.
fn format_type_name(oid: i64) -> &'static str {
    match oid {
        TYPE_OID_BOOL => "boolean",
        TYPE_OID_BYTEA => "bytea",
        TYPE_OID_INT8 => "bigint",
        TYPE_OID_INT2 => "smallint",
        TYPE_OID_INT4 => "integer",
        TYPE_OID_TEXT => "text",
        TYPE_OID_VARCHAR => "character varying",
        TYPE_OID_FLOAT8 => "double precision",
        TYPE_OID_NUMERIC => "numeric",
        TYPE_OID_DATE => "date",
        TYPE_OID_TIMESTAMP => "timestamp without time zone",
        TYPE_OID_TIMESTAMPTZ => "timestamp with time zone",
        TYPE_OID_JSON => "json",
        TYPE_OID_JSONB => "jsonb",
        TYPE_OID_UUID => "uuid",
        _ => "text",
    }
}

/// Backs the current_setting() function with the same values the SHOW command
/// reports.
/// A boolean argument in any of the forms SQLite may hand one over.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Int(n) => *n != 0,
        Value::Bool(b) => *b,
        Value::Text(t) => overlite_core::parse_bool(t).unwrap_or(false),
        _ => false,
    }
}

fn current_setting(name: &str) -> &'static str {
    match name {
        "server_version" => "16.0",
        "server_version_num" => "160000",
        "server_encoding" | "client_encoding" => "UTF8",
        "standard_conforming_strings" | "integer_datetimes" | "is_superuser" => "on",
        "search_path" => "\"$user\", public",
        "transaction_isolation" | "default_transaction_isolation" => "read committed",
        "max_index_keys" => "32",
        "max_identifier_length" => "63",
        "application_name" => "overlite",
        _ => "",
    }
}

// A lowercase identifier that needs no double-quoting.
lazy_regex!(RE_BARE_IDENT, r"^[a-z_][a-z0-9_]*$");

/// Quotes an SQL identifier the way Postgres' quote_ident() does: bare if it is
/// already a safe lowercase identifier, otherwise double-quoted (with embedded
/// quotes doubled).
pub(crate) fn quote_ident_if_needed(s: &str) -> String {
    if RE_BARE_IDENT.is_match(s) {
        return s.to_string();
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// The type modifier of a declared type, as pg_attribute.atttypmod and a
/// RowDescription carry it: varchar(100) is 104, -1 for none.
pub fn declared_typmod(decl: &str) -> i32 {
    i32::try_from(type_att_typmod(decl)).unwrap_or(-1)
}

fn as_int64(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::Float(n) => *n as i64,
        _ => 0,
    }
}

/// The functions overlite provides; they populate pg_proc and
/// information_schema.routines so \df and tools can list them.
const CATALOG_FUNCTION_NAMES: &[&str] = &[
    "version",
    "now",
    "age",
    "current_schema",
    "current_schemas",
    "current_database",
    "current_user",
    "session_user",
    "current_setting",
    "format_type",
    "quote_ident",
    "quote_literal",
    "date_trunc",
    "date_part",
    "to_char",
    "gen_random_uuid",
    "uuid_generate_v4",
    "array_to_string",
    "array_agg",
    "array_length",
    "json_contains",
    "pg_get_indexdef",
    "pg_get_constraintdef",
    "pg_get_triggerdef",
    "pg_get_expr",
    "has_table_privilege",
    "has_schema_privilege",
];

/// Builds pg_proc from the provided-function list (all in pg_catalog).
fn pg_proc_view() -> String {
    let parts: Vec<String> = CATALOG_FUNCTION_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            format!(
                concat!(
                    "SELECT {} AS oid, {} AS proname, 11 AS pronamespace, 10 AS proowner,",
                    " 12 AS prolang, 'f' AS prokind, 25 AS prorettype, '' AS proargtypes,",
                    " 0 AS pronargs, 0 AS proretset, NULL AS proacl, '' AS prosrc, NULL AS probin,",
                    " 'v' AS provolatile, 0 AS proisstrict, NULL AS proargmodes, NULL AS proargnames,",
                    " NULL AS proallargtypes, 0 AS prosecdef, NULL AS proconfig, 100 AS procost,",
                    " 0 AS prorows, 0 AS provariadic, 0 AS prosupport, 0 AS proleakproof,",
                    " 's' AS proparallel, NULL AS proargdefaults, 0 AS pronargdefaults, '' AS prosqlbody,",
                    " NULL AS protrftypes, 1255 AS tableoid"
                ),
                100000 + i,
                quote_literal(name)
            )
        })
        .collect();
    // User-defined LANGUAGE sql functions, read live from the registry (public
    // namespace 2200, `sql` language 14). \df/\sf and pg_dump reconstruct the
    // signature from pg_get_function_arguments/result and the body from prosrc.
    let user_row = concat!(
        "SELECT CAST(rowid + 500000 AS INTEGER) AS oid, name AS proname, 2200 AS pronamespace, 10 AS proowner,",
        " 14 AS prolang, 'f' AS prokind, 25 AS prorettype, '' AS proargtypes,",
        " arity AS pronargs, 0 AS proretset, NULL AS proacl, body AS prosrc, NULL AS probin,",
        " 'v' AS provolatile, 0 AS proisstrict, NULL AS proargmodes,",
        " CASE WHEN params='' THEN NULL ELSE '{'||replace(params,char(31),',')||'}' END AS proargnames,",
        " NULL AS proallargtypes, coalesce(secdef, 0) AS prosecdef, NULL AS proconfig, 100 AS procost,",
        " 0 AS prorows, 0 AS provariadic, 0 AS prosupport, 0 AS proleakproof,",
        " 's' AS proparallel, NULL AS proargdefaults, 0 AS pronargdefaults, NULL AS prosqlbody,",
        " NULL AS protrftypes, 1255 AS tableoid FROM _overlite_functions WHERE lang='sql'"
    );
    format!(
        "CREATE TEMP VIEW pg_proc AS {} UNION ALL {}",
        parts.join(" UNION ALL "),
        user_row
    )
}

/// Builds information_schema.routines from the same list.
fn info_routines_view(db_name: &str) -> String {
    let cat = quote_literal(db_name);
    let parts: Vec<String> = CATALOG_FUNCTION_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            format!(
                concat!(
                    "SELECT {cat} AS specific_catalog, 'pg_catalog' AS specific_schema,",
                    " {name} || '_' || {oid} AS specific_name, {cat} AS routine_catalog,",
                    " 'pg_catalog' AS routine_schema, {name} AS routine_name, 'FUNCTION' AS routine_type,",
                    " 'text' AS data_type, NULL AS routine_definition"
                ),
                cat = cat,
                name = quote_literal(name),
                oid = 100000 + i
            )
        })
        .collect();
    format!(
        r#"CREATE TEMP VIEW "information_schema.routines" AS {}"#,
        parts.join(" UNION ALL ")
    )
}

// Schema-independent; created on every connection. @ENUMBASE@ is filled in per
// database: an enum type's oid is its rowid plus a base, and the registry
// format_type() reads is shared by the whole process, so two databases numbering
// from the same rowid would render each other's names. The schema-spanning views
// (pg_namespace/pg_class/pg_attribute/pg_index/pg_constraint/information_schema.*)
// are generated in views.rs.
static STATIC_CATALOG_VIEWS: &[&str] = &[
    // The outer SELECT adds the columns \dT+ and pg_dump read (typacl,
    // typispreferred, typisdefined, ...) without touching every UNION row. The
    // I/O functions (typinput, ...) are not modeled: 0, Postgres' "none", which
    // is enough for the introspection that filters on them (Prisma's
    // `typinput::regproc::text <> 'array_in'`).
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_type AS
	 SELECT ov_t.*, 1247 AS tableoid, NULL AS typacl, 0 AS typispreferred, 1 AS typisdefined,
	        '-' AS typalign, 'p' AS typstorage, 0 AS typrelid2,
	        0 AS typinput, 0 AS typoutput, 0 AS typreceive, 0 AS typsend,
	        0 AS typmodin, 0 AS typmodout, 0 AS typanalyze FROM (
	 SELECT 16 AS oid, 'bool' AS typname, 11 AS typnamespace, 10 AS typowner,
	        'b' AS typtype, 'B' AS typcategory, 1 AS typlen, 0 AS typbyval,
	        0 AS typrelid, 0 AS typelem, 1000 AS typarray, 0 AS typnotnull,
	        0 AS typbasetype, -1 AS typtypmod, 0 AS typndims, 0 AS typcollation,
	        NULL AS typdefault, ',' AS typdelim
	 UNION ALL SELECT 17,   'bytea',     11, 10, 'b', 'U', -1, 0, 0, 0, 1001, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 20,   'int8',      11, 10, 'b', 'N', 8,  1, 0, 0, 1016, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 21,   'int2',      11, 10, 'b', 'N', 2,  1, 0, 0, 1005, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 23,   'int4',      11, 10, 'b', 'N', 4,  1, 0, 0, 1007, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 25,   'text',      11, 10, 'b', 'S', -1, 0, 0, 0, 1009, 0, 0, -1, 0, 100, NULL, ','
	 UNION ALL SELECT 701,  'float8',    11, 10, 'b', 'N', 8,  1, 0, 0, 1022, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1043, 'varchar',   11, 10, 'b', 'S', -1, 0, 0, 0, 1015, 0, 0, -1, 0, 100, NULL, ','
	 UNION ALL SELECT 1082, 'date',      11, 10, 'b', 'D', 4,  1, 0, 0, 1182, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1114, 'timestamp', 11, 10, 'b', 'D', 8,  1, 0, 0, 1115, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1184, 'timestamptz',11, 10, 'b', 'D', 8,  1, 0, 0, 1185, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1700, 'numeric',   11, 10, 'b', 'N', -1, 0, 0, 0, 1231, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 114,  'json',      11, 10, 'b', 'U', -1, 0, 0, 0, 199,  0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 3802, 'jsonb',     11, 10, 'b', 'U', -1, 0, 0, 0, 3807, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 2950, 'uuid',      11, 10, 'b', 'U', 16, 0, 0, 0, 2951, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1000, '_bool',     11, 10, 'b', 'A', -1, 0, 0, 16,   0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1001, '_bytea',    11, 10, 'b', 'A', -1, 0, 0, 17,   0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1016, '_int8',     11, 10, 'b', 'A', -1, 0, 0, 20,   0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1005, '_int2',     11, 10, 'b', 'A', -1, 0, 0, 21,   0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1007, '_int4',     11, 10, 'b', 'A', -1, 0, 0, 23,   0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1009, '_text',     11, 10, 'b', 'A', -1, 0, 0, 25,   0, 0, 0, -1, 0, 100, NULL, ','
	 UNION ALL SELECT 1022, '_float8',   11, 10, 'b', 'A', -1, 0, 0, 701,  0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1015, '_varchar',  11, 10, 'b', 'A', -1, 0, 0, 1043, 0, 0, 0, -1, 0, 100, NULL, ','
	 UNION ALL SELECT 1182, '_date',     11, 10, 'b', 'A', -1, 0, 0, 1082, 0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1115, '_timestamp',11, 10, 'b', 'A', -1, 0, 0, 1114, 0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1185, '_timestamptz',11, 10, 'b', 'A', -1, 0, 0, 1184, 0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 1231, '_numeric',  11, 10, 'b', 'A', -1, 0, 0, 1700, 0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 199,  '_json',     11, 10, 'b', 'A', -1, 0, 0, 114,  0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 3807, '_jsonb',    11, 10, 'b', 'A', -1, 0, 0, 3802, 0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 2951, '_uuid',     11, 10, 'b', 'A', -1, 0, 0, 2950, 0, 0, 0, -1, 0, 0, NULL, ','
	 -- An extension's types, in pg_catalog: pg_dump leaves them to CREATE EXTENSION.
	 UNION ALL SELECT 16400, 'hstore',   11,   10, 'b', 'U', -1, 0, 0, 0, 16401, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 16401, '_hstore',  11,   10, 'b', 'A', -1, 0, 0, 16400, 0, 0, 0, -1, 0, 0, NULL, ','
	 UNION ALL SELECT 16410, 'citext',   11,   10, 'b', 'S', -1, 0, 0, 0, 16411, 0, 0, -1, 0, 100, NULL, ','
	           WHERE EXISTS (SELECT 1 FROM _overlite_extensions WHERE extname = 'citext')
	 UNION ALL SELECT 16411, '_citext',  11,   10, 'b', 'A', -1, 0, 0, 16410, 0, 0, 0, -1, 0, 100, NULL, ','
	           WHERE EXISTS (SELECT 1 FROM _overlite_extensions WHERE extname = 'citext')
	 UNION ALL SELECT CAST(rowid + @ENUMBASE@ AS INTEGER), typname, 2200, 10, 'e', 'E', 4, 1, 0, 0, 0, 0, 0, -1, 0, 0, NULL, ','
	           FROM _overlite_enum_types
	 UNION ALL SELECT CAST(rowid + 95000000 AS INTEGER), typname, 2200, 10, 'c', 'C', -1, 0, 0, 0, 0, 0, 0, -1, 0, 0, NULL, ','
	           FROM _overlite_composite_types
	 ) ov_t"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_enum AS
	 SELECT CAST(e.rowid AS INTEGER) AS oid,
	        CAST((SELECT t.rowid + @ENUMBASE@ FROM _overlite_enum_types t WHERE t.typname = e.typname) AS INTEGER) AS enumtypid,
	        CAST(e.sortorder AS REAL) AS enumsortorder, e.label AS enumlabel
	 FROM _overlite_enums e"#,
    // pg_auth_members: real role membership from _overlite_memberships, with oids
    // resolved through pg_roles (so psql's \du "Member of" works).
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_auth_members AS
	 SELECT m.rowid AS oid, ro.oid AS roleid, mo.oid AS member,
	        10 AS grantor, m.admin_option AS admin_option
	 FROM _overlite_memberships m
	 JOIN pg_roles ro ON lower(ro.rolname) = lower(m.roleof)
	 JOIN pg_roles mo ON lower(mo.rolname) = lower(m.member)"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_tablespace AS
	 SELECT 1663 AS oid, 'pg_default' AS spcname, 10 AS spcowner, NULL AS spcacl, NULL AS spcoptions
	 UNION ALL SELECT 1664, 'pg_global', 10, NULL, NULL"#,
    // The collations a column can name: default, C and POSIX, the ICU collations of
    // the common languages under Postgres' oids' scheme, and those CREATE COLLATION made.
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_collation AS
	 SELECT 100 AS oid, 'default' AS collname, 11 AS collnamespace, 10 AS collowner,
	        'd' AS collprovider, 1 AS collisdeterministic, -1 AS collencoding,
	        NULL AS collcollate, NULL AS collctype, NULL AS colllocale, NULL AS collicurules, NULL AS collversion
	 UNION ALL SELECT 950, 'C', 11, 10, 'c', 1, -1, 'C', 'C', NULL, NULL, NULL
	 UNION ALL SELECT 951, 'POSIX', 11, 10, 'c', 1, -1, 'POSIX', 'POSIX', NULL, NULL, NULL
	 UNION ALL SELECT 962, 'ucs_basic', 11, 10, 'b', 1, 6, NULL, NULL, 'C', NULL, NULL
	 UNION ALL SELECT 963, 'unicode', 11, 10, 'i', 1, -1, NULL, NULL, 'und', NULL, NULL
	 UNION ALL SELECT 12000 + k, l || '-x-icu', 11, 10, 'i', 1, -1, NULL, NULL, l, NULL, NULL
	   FROM (SELECT key AS k, value AS l FROM json_each('["und","en","sv","fi","de","fr","es","it","pt","nl","da","nb","pl","cs","ru","tr","ja","zh"]'))
	 UNION ALL SELECT CAST(rowid + 900000 AS INTEGER), collname, 2200, 10,
	        CASE provider WHEN 'libc' THEN 'c' ELSE 'i' END, deterministic, -1,
	        CASE provider WHEN 'libc' THEN locale END, CASE provider WHEN 'libc' THEN locale END,
	        CASE provider WHEN 'libc' THEN NULL ELSE locale END, NULL, NULL
	   FROM _overlite_collations"#,
    // Access methods: \dt LEFT JOINs pg_am on c.relam.
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_am AS
	 SELECT 2   AS oid, 'heap'  AS amname, 'heap_tableam_handler' AS amhandler, 't' AS amtype
	 UNION ALL SELECT 403, 'btree', 'bthandler',   'i'
	 UNION ALL SELECT 405, 'hash',  'hashhandler', 'i'
	 UNION ALL SELECT 783, 'gist',  'gisthandler', 'i'
	 UNION ALL SELECT 2742, 'gin',  'ginhandler', 'i'
	 UNION ALL SELECT 4000, 'spgist', 'spghandler', 'i'
	 UNION ALL SELECT 3580, 'brin', 'brinhandler', 'i'
	 UNION ALL SELECT 16400 + 500, 'bloom', 'blhandler', 'i'"#,
    // pg_description is populated from _overlite_comments: relation-level
    // comments join to pg_class for the oid (objsubid 0); column comments join
    // through pg_attribute for the attnum (objsubid = attnum).
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_description AS
	 SELECT c.oid AS objoid, 1259 AS classoid, 0 AS objsubid, cm.comment AS description
	   FROM _overlite_comments cm
	   JOIN pg_class c ON lower(c.relname) = lower(cm.objname)
	  WHERE cm.objkind <> 'column' AND cm.comment IS NOT NULL
	 UNION ALL
	 SELECT a.attrelid AS objoid, 1259 AS classoid, a.attnum AS objsubid, cm.comment AS description
	   FROM _overlite_comments cm
	   JOIN pg_class c ON lower(c.relname) = lower(cm.objname)
	   JOIN pg_attribute a ON a.attrelid = c.oid AND lower(a.attname) = lower(cm.subname)
	  WHERE cm.objkind = 'column' AND cm.comment IS NOT NULL"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_inherits AS
	 SELECT 0 AS inhrelid, 0 AS inhparent, 0 AS inhseqno, 0 AS inhdetachpending
	 WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_rewrite AS
	 SELECT 0 AS oid, '' AS rulename, 0 AS ev_class, '1' AS ev_type,
	        'O' AS ev_enabled, 0 AS is_instead, NULL AS ev_qual, NULL AS ev_action
	 WHERE 0"#,
    // pg_policy: real RLS policies from _overlite_policies. polcmd maps the
    // command to Postgres' single-char code; polqual/polwithcheck carry the
    // expression text (what psql renders for \d). polroles NULL means "to all".
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_policy AS
	 SELECT p.rowid AS oid, p.polname AS polname,
	        (SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
	          WHERE lower(p.tablename) = lower(CASE n.nspname WHEN 'public' THEN c.relname
	                ELSE n.nspname || '.' || c.relname END)
	          AND c.relkind IN ('r','p') LIMIT 1) AS polrelid,
	        CASE upper(p.command) WHEN 'SELECT' THEN 'r' WHEN 'INSERT' THEN 'a'
	          WHEN 'UPDATE' THEN 'w' WHEN 'DELETE' THEN 'd' ELSE '*' END AS polcmd,
	        p.permissive AS polpermissive, NULL AS polroles,
	        CASE WHEN p.using_expr = '' THEN NULL ELSE p.using_expr END AS polqual,
	        CASE WHEN p.check_expr = '' THEN NULL ELSE p.check_expr END AS polwithcheck
	 FROM _overlite_policies p"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_statistic_ext AS
	 SELECT 0 AS oid, 0 AS stxrelid, '' AS stxname, 2200 AS stxnamespace,
	        10 AS stxowner, '' AS stxkeys, '' AS stxkind, -1 AS stxstattarget
	 WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_partitioned_table AS
	 SELECT 0 AS partrelid, 'r' AS partstrat, 0 AS partnatts, 0 AS partdefid,
	        '' AS partattrs, '' AS partclass, '' AS partcollation, NULL AS partexprs
	 WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_publication AS
	 SELECT 0 AS oid, '' AS pubname, 10 AS pubowner, 0 AS puballtables,
	        0 AS pubinsert, 0 AS pubupdate, 0 AS pubdelete, 0 AS pubtruncate,
	        0 AS pubviaroot
	 WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_publication_rel AS
	 SELECT 0 AS oid, 0 AS prpubid, 0 AS prrelid, NULL AS prqual, NULL AS prattrs
	 WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_publication_namespace AS
	 SELECT 0 AS oid, 0 AS pnpubid, 0 AS pnnspid
	 WHERE 0"#,
    // Empty system catalogs pg_dump probes early (extensions, dependencies,
    // default privileges, comments). Kept empty so its queries run and return
    // nothing.
    // plpgsql, and the extensions CREATE EXTENSION has made.
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_extension AS
	 SELECT 13000 AS oid, 'plpgsql' AS extname, 10 AS extowner, 11 AS extnamespace,
	        0 AS extrelocatable, '1.0' AS extversion, NULL AS extconfig, NULL AS extcondition
	 UNION ALL
	 SELECT CAST(rowid + 13000 AS INTEGER), extname, 10, 2200, 0, extversion, NULL, NULL
	 FROM _overlite_extensions"#,
    // pg_depend: real structural dependencies, assembled from the other catalog
    // views so the oids line up by construction. Each index / constraint /
    // trigger / policy auto-depends ('a') on the table it belongs to.
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_depend AS
	 SELECT 1259 AS classid, i.indexrelid AS objid, 0 AS objsubid,
	        1259 AS refclassid, i.indrelid AS refobjid, 0 AS refobjsubid, 'a' AS deptype
	 FROM pg_index i
	 UNION ALL SELECT 2606, c.oid, 0, 1259, c.conrelid, 0, 'a' FROM pg_constraint c
	 UNION ALL SELECT 2620, t.oid, 0, 1259, t.tgrelid, 0, 'a' FROM pg_trigger t
	 UNION ALL SELECT 3256, p.oid, 0, 1259, p.polrelid, 0, 'a' FROM pg_policy p
	 UNION ALL SELECT 1259, s.oid, 0, 1259, t.oid, a.attnum, CASE a.attidentity WHEN '' THEN 'a' ELSE 'i' END
	 FROM _overlite_serial_sequences s
	 JOIN pg_class t ON t.relnamespace = s.ns AND t.relkind = 'r'
	      AND t.relname = substr(s.tablename, instr(s.tablename, '.') + 1)
	 JOIN pg_attribute a ON a.attrelid = t.oid AND s.seqname = t.relname || '_' || a.attname || '_seq'
	 UNION ALL SELECT 1259, q.oid, 0, 1259, t.oid, a.attnum, CASE a.attidentity WHEN '' THEN 'a' ELSE 'i' END
	 FROM _overlite_defaults d
	 JOIN pg_class q ON q.relkind = 'S' AND q.relname = d.seqname
	 JOIN pg_class t ON t.relkind = 'r' AND t.relname = substr(d.tablename, instr(d.tablename, '.') + 1)
	 JOIN pg_attribute a ON a.attrelid = t.oid AND lower(a.attname) = lower(d.colname)
	      AND (a.attidentity <> '' OR instr(overlite_column_def((SELECT sql FROM sqlite_master
	           WHERE type = 'table' AND name = d.tablename), a.attname), '/*overlite owned sequence*/'))"#,
    // pg_shdepend: shared dependency of each table on its owning role.
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_shdepend AS
	 SELECT 0 AS dbid, 1259 AS classid, c.oid AS objid, 0 AS objsubid,
	        1260 AS refclassid, r.oid AS refobjid, 'o' AS deptype
	 FROM _overlite_owners o
	 JOIN pg_class c ON c.relkind IN ('r','p')
	 JOIN pg_namespace n ON n.oid = c.relnamespace AND lower(o.tablename) =
	      lower(CASE n.nspname WHEN 'public' THEN c.relname ELSE n.nspname || '.' || c.relname END)
	 JOIN pg_roles r ON lower(r.rolname) = lower(o.owner)"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_default_acl AS
	 SELECT 0 AS oid, 0 AS defaclrole, 2200 AS defaclnamespace, 'r' AS defaclobjtype, NULL AS defaclacl
	 WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_init_privs AS
	 SELECT 0 AS objoid, 0 AS classoid, 0 AS objsubid, 'i' AS privtype, NULL AS initprivs
	 WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_shdescription AS
	 SELECT 0 AS objoid, 0 AS classoid, '' AS description WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_largeobject_metadata AS
	 SELECT 0 AS oid, 0 AS lomowner, NULL AS lomacl WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_subscription AS
	 SELECT 0 AS oid, 0 AS subdbid, '' AS subname WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_event_trigger AS
	 SELECT 0 AS oid, '' AS evtname, '' AS evtevent, 10 AS evtowner, 'O' AS evtenabled,
	        0 AS evtfoid, NULL AS evttags WHERE 0"#,
    // Foreign-data catalogs: we have none, but pg_dump joins them per relation.
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_foreign_table AS
	 SELECT 0 AS ftrelid, 0 AS ftserver, NULL AS ftoptions WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_foreign_server AS
	 SELECT 0 AS oid, '' AS srvname, 10 AS srvowner, 0 AS srvfdw, NULL AS srvtype,
	        NULL AS srvversion, NULL AS srvacl, NULL AS srvoptions WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_foreign_data_wrapper AS
	 SELECT 0 AS oid, '' AS fdwname, 10 AS fdwowner, 0 AS fdwhandler, 0 AS fdwvalidator,
	        NULL AS fdwacl, NULL AS fdwoptions WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_transform AS
	 SELECT 0 AS oid, 0 AS trftype, 0 AS trflang, 0 AS trffromsql, 0 AS trftosql WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_operator AS
	 SELECT 0 AS oid, '' AS oprname, 11 AS oprnamespace, 10 AS oprowner, 'b' AS oprkind,
	        0 AS oprleft, 0 AS oprright, 0 AS oprresult, 0 AS oprcom, 0 AS oprnegate,
	        0 AS oprcode WHERE 0"#,
    // The default btree operator classes of the types overlite reports, with
    // Postgres' own oids: pg_index.indclass names them (default_opclass_oid), and
    // introspection reads opcname/opcdefault to tell a default class from one
    // an index asked for.
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_opclass AS
	 SELECT 10003 AS oid, 403 AS opcmethod, 'bool_ops' AS opcname, 11 AS opcnamespace, 10 AS opcowner,
	        0 AS opcfamily, 16 AS opcintype, 1 AS opcdefault, 0 AS opckeytype
	 UNION ALL SELECT 10006, 403, 'bytea_ops',       11, 10, 0, 17,   1, 0
	 UNION ALL SELECT 3122,  403, 'date_ops',        11, 10, 0, 1082, 1, 0
	 UNION ALL SELECT 3123,  403, 'float8_ops',      11, 10, 0, 701,  1, 0
	 UNION ALL SELECT 1979,  403, 'int2_ops',        11, 10, 0, 21,   1, 0
	 UNION ALL SELECT 1978,  403, 'int4_ops',        11, 10, 0, 23,   1, 0
	 UNION ALL SELECT 3124,  403, 'int8_ops',        11, 10, 0, 20,   1, 0
	 UNION ALL SELECT 3125,  403, 'numeric_ops',     11, 10, 0, 1700, 1, 0
	 UNION ALL SELECT 3126,  403, 'text_ops',        11, 10, 0, 25,   1, 0
	 UNION ALL SELECT 3127,  403, 'timestamptz_ops', 11, 10, 0, 1184, 1, 0
	 UNION ALL SELECT 3128,  403, 'timestamp_ops',   11, 10, 0, 1114, 1, 0
	 UNION ALL SELECT 10065, 403, 'uuid_ops',        11, 10, 0, 2950, 1, 0
	 UNION ALL SELECT 10088, 403, 'jsonb_ops',       11, 10, 0, 3802, 1, 0
	 UNION ALL SELECT 4217,  403, 'text_pattern_ops',    11, 10, 0, 25,   0, 0
	 UNION ALL SELECT 4218,  403, 'varchar_pattern_ops', 11, 10, 0, 25,   0, 0
	 UNION ALL SELECT 4219,  403, 'bpchar_pattern_ops',  11, 10, 0, 1042, 0, 0
	 UNION ALL SELECT 10044, 403, 'varchar_ops',         11, 10, 0, 25,   0, 0
	 UNION ALL SELECT 10091, 2742, 'jsonb_path_ops',     11, 10, 0, 3802, 0, 0
	 UNION ALL SELECT 16616, 2742, 'gin_trgm_ops',       2200, 10, 0, 25, 0, 0
	 UNION ALL SELECT 16595, 783, 'gist_trgm_ops',       2200, 10, 0, 25, 0, 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_opfamily AS
	 SELECT 0 AS oid, 0 AS opfmethod, '' AS opfname, 11 AS opfnamespace, 10 AS opfowner WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_conversion AS
	 SELECT 0 AS oid, '' AS conname, 11 AS connamespace, 10 AS conowner,
	        0 AS conforencoding, 0 AS contoencoding, 0 AS conproc, 0 AS condefault WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_aggregate AS
	 SELECT 0 AS aggfnoid, 'n' AS aggkind, 0 AS aggnumdirectargs, 0 AS aggtransfn,
	        0 AS aggfinalfn, 0 AS aggtranstype WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_range AS
	 SELECT 0 AS rngtypid, 0 AS rngsubtype, 0 AS rngmultitypid, 0 AS rngcollation,
	        0 AS rngsubopc, 0 AS rngcanonical, 0 AS rngsubdiff WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_ts_config AS
	 SELECT 0 AS oid, '' AS cfgname, 11 AS cfgnamespace, 10 AS cfgowner, 0 AS cfgparser WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_ts_dict AS
	 SELECT 0 AS oid, '' AS dictname, 11 AS dictnamespace, 10 AS dictowner,
	        0 AS dicttemplate, NULL AS dictinitoption WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_ts_parser AS
	 SELECT 0 AS oid, '' AS prsname, 11 AS prsnamespace, 0 AS prsstart, 0 AS prstoken,
	        0 AS prsend, 0 AS prsheadline, 0 AS prslextype WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_ts_template AS
	 SELECT 0 AS oid, '' AS tmplname, 11 AS tmplnamespace, 0 AS tmplinit, 0 AS tmpllexize WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_seclabels AS
	 SELECT 0 AS objoid, 0 AS classoid, 0 AS objsubid, '' AS objtype, 0 AS objnamespace,
	        '' AS objname, '' AS provider, '' AS label WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_seclabel AS
	 SELECT 0 AS objoid, 0 AS classoid, 0 AS objsubid, '' AS provider, '' AS label WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_shseclabel AS
	 SELECT 0 AS objoid, 0 AS classoid, '' AS provider, '' AS label WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_amproc AS
	 SELECT 0 AS oid, 0 AS amprocfamily, 0 AS amproclefttype, 0 AS amprocrighttype,
	        0 AS amprocnum, 0 AS amproc WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_amop AS
	 SELECT 0 AS oid, 0 AS amopfamily, 0 AS amoplefttype, 0 AS amoprighttype, 0 AS amopstrategy,
	        'a' AS amoppurpose, 0 AS amopopr, 0 AS amopmethod, 0 AS amopsortfamily WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_cast AS
	 SELECT 0 AS oid, 0 AS castsource, 0 AS casttarget, 0 AS castfunc,
	        'e' AS castcontext, 'f' AS castmethod WHERE 0"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_language AS
	 SELECT 12 AS oid, 'internal' AS lanname, 10 AS lanowner, 0 AS lanispl, 0 AS lanpltrusted,
	        0 AS lanplcallfoid, 0 AS laninline, 0 AS lanvalidator, NULL AS lanacl
	 UNION ALL SELECT 13,'c',10,0,0,0,0,0,NULL
	 UNION ALL SELECT 14,'sql',10,0,1,0,0,0,NULL
	 UNION ALL SELECT 13545,'plpgsql',10,1,1,0,0,0,NULL"#,
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_settings AS
	 SELECT '' AS name, '' AS setting, '' AS unit, '' AS category, '' AS short_desc,
	        '' AS context, '' AS vartype, '' AS source, NULL AS min_val, NULL AS max_val,
	        NULL AS enumvals, '' AS boot_val, '' AS reset_val
	 WHERE 0"#,
    // Sequence metadata for \d <seq> and pg_dump (rows come from
    // _overlite_sequences; oids match the pg_class 'S' rows).
    r#"CREATE TEMP VIEW IF NOT EXISTS pg_sequence AS
	 SELECT CAST(rowid + 80000000 AS INTEGER) AS seqrelid, 20 AS seqtypid,
	        start_value AS seqstart, increment AS seqincrement, max_value AS seqmax,
	        min_value AS seqmin, cache_size AS seqcache, is_cycled AS seqcycle
	 FROM _overlite_sequences
	 UNION ALL SELECT oid, 20, 1, 1, 9223372036854775807, 1, 1, 0 FROM _overlite_serial_sequences"#,
];

/// The argument furthest toward want, NULLs aside, in SQLite's order: numbers, then
/// text, then blobs.
fn extreme(args: &[Value], want: std::cmp::Ordering) -> Value {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Int(_) | Value::Float(_) | Value::Bool(_) => 0,
            Value::Text(_) | Value::Time(_) => 1,
            _ => 2,
        }
    }
    fn number(v: &Value) -> f64 {
        match v {
            Value::Int(n) => *n as f64,
            Value::Float(f) => *f,
            Value::Bool(b) => f64::from(u8::from(*b)),
            _ => 0.0,
        }
    }
    fn compare(a: &Value, b: &Value) -> Ordering {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(y),
            (Value::Text(x), Value::Text(y)) => x.cmp(y),
            (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
            _ if rank(a) != rank(b) => rank(a).cmp(&rank(b)),
            _ if rank(a) == 0 => number(a).partial_cmp(&number(b)).unwrap_or(Ordering::Equal),
            _ => a.to_string().cmp(&b.to_string()),
        }
    }
    let mut best: Option<&Value> = None;
    for v in args.iter().filter(|v| !v.is_null()) {
        if best.is_none_or(|b| compare(v, b) == want) {
            best = Some(v);
        }
    }
    best.cloned().unwrap_or(Value::Null)
}

/// The text a boolean column may hold for true, besides 1: what Postgres' boolin
/// reads as true, in lower case, upper case and capitalized. Integers aside, these
/// are what a client sends, so what the Go server stored.
pub(crate) const TRUE_FORMS: &[&str] = &[
    "t", "true", "y", "yes", "on", "T", "TRUE", "Y", "YES", "ON", "True", "Yes", "On",
];
/// The same for false, besides 0.
pub(crate) const FALSE_FORMS: &[&str] = &[
    "f", "false", "n", "no", "off", "F", "FALSE", "N", "NO", "OFF", "False", "No", "Off",
];

/// What overlite_bool_forms answers: the forms of v's truth value as a JSON array,
/// `[null]` for NULL, so that `col IN` it is NULL as `col = NULL` is.
pub(crate) fn bool_forms_json(b: Option<bool>) -> String {
    let Some(b) = b else {
        return "[null]".to_string();
    };
    let (n, words) = if b { ("1", TRUE_FORMS) } else { ("0", FALSE_FORMS) };
    let mut out = format!("[{n}");
    for w in words {
        out.push_str(",\"");
        out.push_str(w);
        out.push('"');
    }
    out.push(']');
    out
}
