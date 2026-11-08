//! Type modifiers reported by information_schema.columns are recovered from the
//! declared SQLite type: varchar(50) -> character_maximum_length 50, and the
//! exact-numeric marker DECIMALTEXT(10,2) -> numeric_precision 10 / scale 2 (the
//! (p,s) is kept on the marker precisely so it can be reported here).

use crate::catalog::{
    ARRAY_TYPES, TYPE_OID_DATE, TYPE_OID_FLOAT8, TYPE_OID_INT2, TYPE_OID_INT4, TYPE_OID_INT8, TYPE_OID_NUMERIC,
    TYPE_OID_TIMESTAMP, TYPE_OID_TIMESTAMPTZ, TYPE_OID_VARCHAR, format_type_name, lower_chars, sqlite_type_oid,
};
use overlite_core::Value;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Whether a declared type is an array: `text[]`, `numeric(10,2)[]`.
pub(crate) fn is_array_decl(decl: &str) -> bool {
    decl.trim_end().ends_with("[]")
}

/// The type of an array's elements, all dimensions off; another type as it is.
pub(crate) fn element_decl(decl: &str) -> &str {
    let mut d = decl.trim_end();
    while let Some(rest) = d.strip_suffix("[]") {
        d = rest.trim_end();
    }
    d
}

/// Splits a declared type into its base name (lowercased) and the integer
/// arguments inside its parentheses: "DECIMALTEXT(10,2)" -> "decimaltext",
/// [10,2].
fn type_paren(decl: &str) -> (String, Vec<i64>) {
    let decl = decl.trim();
    let Some(open) = decl.find('(') else {
        return (lower_chars(decl), Vec::new());
    };
    let base = lower_chars(decl[..open].trim());
    let close = match decl.find(')') {
        Some(c) if c >= open => c,
        _ => return (base, Vec::new()),
    };
    let mut nums = Vec::new();
    for p in decl[open + 1..close].split(',') {
        if let Ok(n) = p.trim().parse::<i64>() {
            nums.push(n);
        }
    }
    (base, nums)
}

static CHAR_TYPES: LazyLock<HashMap<&'static str, bool>> = LazyLock::new(|| {
    HashMap::from([
        ("varchar", true),
        ("character varying", true),
        ("char", true),
        ("character", true),
        ("bpchar", true),
    ])
});

static NUMERIC_TYPES: LazyLock<HashMap<&'static str, bool>> =
    LazyLock::new(|| HashMap::from([("decimaltext", true), ("numeric", true), ("decimal", true)]));

fn is_char_type(base: &str) -> bool {
    CHAR_TYPES.get(base).copied().unwrap_or(false)
}

fn is_numeric_type(base: &str) -> bool {
    NUMERIC_TYPES.get(base).copied().unwrap_or(false)
}

/// The declared length of a character type, or NULL.
pub(crate) fn type_char_max_length(decl: &str) -> Value {
    if is_array_decl(decl) {
        return Value::Null;
    }
    let (base, nums) = type_paren(decl);
    if is_char_type(&base) && !nums.is_empty() {
        return Value::Int(nums[0]);
    }
    Value::Null
}

/// The precision Postgres reports, in bits, for the integer and floating-point
/// types, whose precision is fixed by the type itself.
fn binary_precision(decl: &str) -> Option<i64> {
    match sqlite_type_oid(decl) {
        TYPE_OID_INT2 => Some(16),
        TYPE_OID_INT4 => Some(32),
        TYPE_OID_INT8 => Some(64),
        TYPE_OID_FLOAT8 => Some(53),
        _ => None,
    }
}

/// The declared precision of a numeric type, the fixed binary precision of an
/// integer or float, or NULL (plain unqualified numeric has no fixed precision).
pub(crate) fn type_numeric_precision(decl: &str) -> Value {
    if is_array_decl(decl) {
        return Value::Null;
    }
    let (base, nums) = type_paren(decl);
    if is_numeric_type(&base) && !nums.is_empty() {
        return Value::Int(nums[0]);
    }
    if let Some(p) = binary_precision(decl) {
        return Value::Int(p);
    }
    Value::Null
}

/// The base numeric_precision is counted in: 10 for a declared numeric, 2 for the
/// binary types, NULL for everything else.
pub(crate) fn type_numeric_precision_radix(decl: &str) -> Value {
    if is_array_decl(decl) {
        return Value::Null;
    }
    let (base, nums) = type_paren(decl);
    if is_numeric_type(&base) && !nums.is_empty() {
        return Value::Int(10);
    }
    if binary_precision(decl).is_some() {
        return Value::Int(2);
    }
    Value::Null
}

/// The declared scale of a numeric type: the second paren argument, defaulting to
/// 0 when only a precision is given (as Postgres does), 0 for an integer, or NULL
/// when unqualified.
pub(crate) fn type_numeric_scale(decl: &str) -> Value {
    if is_array_decl(decl) {
        return Value::Null;
    }
    let (base, nums) = type_paren(decl);
    if !is_numeric_type(&base) || nums.is_empty() {
        return match sqlite_type_oid(decl) {
            TYPE_OID_INT2 | TYPE_OID_INT4 | TYPE_OID_INT8 => Value::Int(0),
            _ => Value::Null,
        };
    }
    if nums.len() >= 2 {
        return Value::Int(nums[1]);
    }
    Value::Int(0)
}

/// The fractional-second digits of a date/time type: the declared precision
/// (TIMESTAMP(3) -> 3), Postgres' default of 6 when none is given, 0 for a date,
/// NULL for any other type.
pub(crate) fn type_datetime_precision(decl: &str) -> Value {
    if is_array_decl(decl) {
        return Value::Null;
    }
    match sqlite_type_oid(decl) {
        TYPE_OID_DATE => Value::Int(0),
        TYPE_OID_TIMESTAMP | TYPE_OID_TIMESTAMPTZ => {
            let (_, nums) = type_paren(decl);
            if !nums.is_empty() {
                return Value::Int(nums[0]);
            }
            Value::Int(6)
        }
        _ => Value::Null,
    }
}

/// Encodes the declared modifiers the way pg_attribute.atttypmod holds them: the
/// precision for a timestamp, the length plus 4 for a character type,
/// ((precision << 16) | scale) + 4 for a numeric, and -1 for none. An array has its
/// element's.
pub(crate) fn type_att_typmod(decl: &str) -> i64 {
    let decl = element_decl(decl);
    let (base, nums) = type_paren(decl);
    if nums.is_empty() {
        return -1;
    }
    if is_char_type(&base) {
        return nums[0].wrapping_add(4);
    }
    if is_numeric_type(&base) {
        let scale = if nums.len() >= 2 { nums[1] } else { 0 };
        return ((nums[0] << 16) | scale).wrapping_add(4);
    }
    match sqlite_type_oid(decl) {
        TYPE_OID_TIMESTAMP | TYPE_OID_TIMESTAMPTZ => nums[0],
        _ => -1,
    }
}

/// Renders a type the way format_type(oid, typmod) does, with the modifiers
/// type_att_typmod encoded: timestamp(3) without time zone, character
/// varying(36), numeric(10,2). An array is its element's, with brackets.
pub(crate) fn format_type_mod(oid: i64, typmod: i64) -> String {
    if let Some((elem, ..)) = ARRAY_TYPES.iter().find(|(_, arr, _)| *arr == oid) {
        return format_type_mod(*elem, typmod) + "[]";
    }
    let name = format_type_name(oid);
    if typmod < 0 {
        return name.to_string();
    }
    match oid {
        TYPE_OID_TIMESTAMP => format!("timestamp({typmod}) without time zone"),
        TYPE_OID_TIMESTAMPTZ => format!("timestamp({typmod}) with time zone"),
        TYPE_OID_VARCHAR => format!("{}({})", name, typmod.wrapping_sub(4)),
        TYPE_OID_NUMERIC => {
            let t = typmod.wrapping_sub(4);
            format!("numeric({},{})", t >> 16, t & 0xffff)
        }
        _ => name.to_string(),
    }
}
