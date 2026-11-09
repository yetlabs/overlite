//! Arrays are stored as JSON arrays in TEXT cells (see `types::array` in the postgres crate).
//! These back the Postgres array functions over that representation.

use crate::builtins::json::{Json, json_decode_first, json_quote, json_unmarshal_raw_array};
use crate::builtins::ranges::arg_str;
use overlite_core::text::{eq_fold, slice, split_words, trim_chars};
use overlite_core::{Value, lazy_regex};
use std::collections::HashMap;

/// Whether s parses as a JSON array. Range text like "[1,10)" is not valid JSON,
/// so this distinguishes a stored array from a range (the ambiguous closed form
/// "[1,5]" is treated as an array).
pub(crate) fn is_json_array_text(s: &str) -> bool {
    json_unmarshal_raw_array(s).is_some()
}

/// Whether two stored JSON arrays share an element (the Postgres array "&&"
/// operator). The second value is false when either isn't a JSON array.
pub(crate) fn json_arrays_overlap(a: &str, b: &str) -> (bool, bool) {
    let (Some(ax), Some(bx)) = (json_unmarshal_raw_array(a), json_unmarshal_raw_array(b)) else {
        return (false, false);
    };
    let mut seen: HashMap<&str, bool> = HashMap::with_capacity(ax.len());
    for e in &ax {
        seen.insert(e, true);
    }
    for e in &bx {
        if seen.contains_key(e) {
            return (true, true);
        }
    }
    (false, true)
}

/// `a || b` of Postgres' arrays, kinds saying which side is an array (`a`) and which
/// an element (`e`): "aa" two arrays end to end, "ae" an element appended, "ea" one
/// prepended. A NULL array is no elements, a NULL element is appended as one; two
/// NULL arrays are NULL, as Postgres' array_cat gives.
pub(crate) fn array_cat(l: &Value, r: &Value, kinds: &str) -> Value {
    let items = |v: &Value, array: bool| -> Option<Option<String>> {
        if array {
            if v.is_null() {
                return Some(None);
            }
            let Value::Text(j) = array_as_json(v) else { return None };
            let inner = j.trim().strip_prefix('[')?.strip_suffix(']')?.trim().to_string();
            Some(Some(inner))
        } else {
            Some(Some(match v {
                Value::Null => "null".to_string(),
                Value::Int(n) => n.to_string(),
                Value::Float(f) if f.is_finite() => overlite_core::format_float(*f),
                Value::Bool(b) => b.to_string(),
                other => json_quote(&arg_str(other)),
            }))
        }
    };
    let (la, ra) = match kinds {
        "ae" => (true, false),
        "ea" => (false, true),
        _ => (true, true),
    };
    let (Some(a), Some(b)) = (items(l, la), items(r, ra)) else {
        return Value::Null;
    };
    match (a, b) {
        (None, None) => Value::Null,
        (Some(x), None) | (None, Some(x)) => Value::Text(["[", &x, "]"].concat()),
        (Some(x), Some(y)) if x.is_empty() => Value::Text(["[", &y, "]"].concat()),
        (Some(x), Some(y)) if y.is_empty() => Value::Text(["[", &x, "]"].concat()),
        (Some(x), Some(y)) => Value::Text(["[", &x, ",", &y, "]"].concat()),
    }
}

/// Parses a stored JSON array value and returns its length.
pub(crate) fn json_array_len(v: &Value) -> Option<i64> {
    let s = match v {
        Value::Text(x) => std::borrow::Cow::Borrowed(x.as_str()),
        Value::Blob(x) => String::from_utf8_lossy(x),
        _ => return None,
    };
    json_unmarshal_raw_array(&s).map(|arr| arr.len() as i64)
}

// The text form of int2vector/oidvector ("1 2 3"), the type of the catalog's
// pg_index.indkey/indclass/indoption.
lazy_regex!(RE_INT_VECTOR, r"^-?\d+(?: -?\d+)*$");

/// v as a JSON array whatever form the array arrived in: a stored JSON array as
/// is, Postgres' '{a,b}' text, or a vector's "1 2". It is what unnest and
/// generate_subscripts expand, so they work on the catalog's vectors as well as
/// on stored arrays. Anything else is NULL: no elements.
/// Postgres' order of two arrays, in either form an array is stored or given in:
/// element by element, numbers as numbers and anything else by its text, a NULL
/// element after every other, then the shorter first. None when either is not an array.
pub(crate) fn array_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    use crate::builtins::json::{Json, json_decode_first};
    use std::cmp::Ordering;
    let (Value::Text(x), Value::Text(y)) = (array_as_json(a), array_as_json(b)) else {
        return None;
    };
    let (Some(Json::Arr(x)), Some(Json::Arr(y))) = (json_decode_first(&x), json_decode_first(&y)) else {
        return None;
    };
    let number = |j: &Json| match j {
        Json::Number(n) => Some(n.clone()),
        Json::Num(f) => Some(f.to_string()),
        _ => None,
    };
    for (p, q) in x.iter().zip(&y) {
        let o = match (p, q) {
            (Json::Null, Json::Null) => Ordering::Equal,
            (Json::Null, _) => Ordering::Greater,
            (_, Json::Null) => Ordering::Less,
            (Json::Str(a), Json::Str(b)) => a.cmp(b),
            (Json::Bool(a), Json::Bool(b)) => a.cmp(b),
            _ => match (number(p), number(q)) {
                (Some(a), Some(b)) => crate::builtins::decimal::dec_cmp_str(&a, &b).cmp(&0),
                _ => crate::builtins::json::json_marshal(p).cmp(&crate::builtins::json::json_marshal(q)),
            },
        };
        if o != Ordering::Equal {
            return Some(o);
        }
    }
    Some(x.len().cmp(&y.len()))
}

pub(crate) fn array_as_json(v: &Value) -> Value {
    let arg = arg_str(v);
    let s = arg.trim();
    if s.is_empty() {
        return Value::Null;
    }
    if is_json_array_text(s) {
        return Value::Text(s.to_string());
    }
    if RE_INT_VECTOR.is_match(s) {
        return Value::Text(format!("[{}]", split_words(s).join(",")));
    }
    if s.starts_with('{') && s.ends_with('}') {
        let inner = slice(s, 1, s.len().saturating_sub(1)).trim();
        if inner.is_empty() {
            return Value::Text("[]".to_string());
        }
        if let Some(items) = array_items(inner) {
            let parts: Vec<String> = items
                .into_iter()
                .map(|(item, quoted)| match item {
                    None => "null".to_string(),
                    Some(p) if !quoted && RE_INT_VECTOR.is_match(&p) => p,
                    Some(p) => json_quote(&p),
                })
                .collect();
            return Value::Text(format!("[{}]", parts.join(",")));
        }
        let parts: Vec<String> = inner
            .split(',')
            .map(|p| {
                let p = trim_chars(p.trim(), "\"");
                if eq_fold(p, "null") {
                    "null".to_string()
                } else if RE_INT_VECTOR.is_match(p) {
                    p.to_string()
                } else {
                    json_quote(p)
                }
            })
            .collect();
        return Value::Text(format!("[{}]", parts.join(",")));
    }
    Value::Null
}

/// The elements of a one-dimensional array literal's inside, as array_in reads them:
/// a quoted element keeps its commas and braces and takes `\` escapes, an unquoted
/// one is trimmed and NULL is no value. Each comes with whether it was quoted. None
/// for a nested array, which the caller reads its own way.
fn array_items(inner: &str) -> Option<Vec<(Option<String>, bool)>> {
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut item = String::new();
        let quoted = chars.peek() == Some(&'"');
        if quoted {
            chars.next();
            loop {
                match chars.next()? {
                    '\\' => item.push(chars.next()?),
                    '"' => break,
                    c => item.push(c),
                }
            }
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                if c == '{' || c == '"' {
                    return None;
                }
                if c == '\\' {
                    chars.next();
                    item.push(chars.next()?);
                    continue;
                }
                item.push(c);
                chars.next();
            }
            item = item.trim().to_string();
        }
        let null = !quoted && eq_fold(&item, "null");
        out.push(((!null).then_some(item), quoted));
        match chars.next() {
            None => return Some(out),
            Some(',') => {}
            Some(_) => return None,
        }
    }
}

/// The subscript of an array's first element: 1, except for the catalog's
/// int2vector/oidvector columns, which Postgres numbers from 0
/// (generate_subscripts(indkey, 1) starts at 0).
pub(crate) fn array_lower(v: &Value) -> Value {
    if RE_INT_VECTOR.is_match(arg_str(v).trim()) && !is_json_array_text(&arg_str(v)) {
        return Value::Int(0);
    }
    Value::Int(1)
}

/// array_to_string(array, sep [, null]) over an array literal (`{a,"b c"}`, as
/// pg_attribute.attacl reads, which psql's \dp joins): its elements joined by sep, a
/// NULL left out or written as null when that is given. Anything else, the JSON the
/// engine stores an array as included, is '' as it always was here. An
/// array(SELECT ...) argument never gets here: the rewrite makes that a group_concat.
pub(crate) fn array_to_string(a: &[Value]) -> Value {
    let (Some(arr), Some(Value::Text(sep))) = (a.first(), a.get(1)) else {
        return Value::Text(String::new());
    };
    let null = match a.get(2) {
        Some(Value::Text(s)) => Some(s.as_str()),
        _ => None,
    };
    let s = arg_str(arr);
    let Some(inner) = s.trim().strip_prefix('{').and_then(|x| x.strip_suffix('}')) else {
        return Value::Text(String::new());
    };
    if inner.trim().is_empty() {
        return Value::Text(String::new());
    }
    let Some(items) = array_items(inner) else {
        return Value::Text(String::new());
    };
    let parts: Vec<&str> = items.iter().filter_map(|(i, _)| i.as_deref().or(null)).collect();
    Value::Text(parts.join(sep))
}

/// Postgres' text form of an array (array_out): `{a,"b c",NULL}`, each element quoted
/// where it would otherwise be read another way, a boolean `t` or `f`.
#[must_use]
pub fn pg_array_text(arr: &[Json]) -> String {
    let parts: Vec<String> = arr.iter().map(pg_array_elem).collect();
    ["{", &parts.join(","), "}"].concat()
}

fn pg_array_elem(e: &Json) -> String {
    match e {
        Json::Null => "NULL".to_string(),
        Json::Bool(x) => if *x { "t" } else { "f" }.to_string(),
        Json::Number(x) => x.clone(),
        Json::Num(x) => overlite_core::format_float(*x),
        Json::Str(x) => quote_pg_array_elem(x),
        Json::Arr(x) => pg_array_text(x),
        Json::Obj(_) => quote_pg_array_elem(""),
    }
}

/// Double-quotes an element when it would otherwise be ambiguous (empty, holding a
/// delimiter, a quote or a space, or spelled like NULL).
fn quote_pg_array_elem(s: &str) -> String {
    if s.is_empty() || s.eq_ignore_ascii_case("null") || s.contains(['{', '}', ',', '"', '\\', ' ']) {
        let s = s.replace('\\', "\\\\").replace('"', "\\\"");
        return ["\"", &s, "\""].concat();
    }
    s.to_string()
}

/// overlite_array_text: an array, stored as a JSON array, in Postgres' text form, as a
/// cast to text writes it; what is no JSON array as it is.
pub(crate) fn array_text(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Text(s) => match json_decode_first(s) {
            Some(Json::Arr(a)) => Value::Text(pg_array_text(&a)),
            _ => v.clone(),
        },
        other => other.clone(),
    }
}

/// overlite_array_slice(a, lo, hi): the elements of a JSON array from lo to hi, counted
/// from 1, as `a[lo:hi]` takes them (a bound NULL is open); NULL for what is no JSON
/// array.
pub(crate) fn array_slice(a: &[Value]) -> Value {
    // An array in Postgres' text form too (string_to_array's).
    let Some(Value::Text(s)) = a.first().map(array_as_json) else {
        return Value::Null;
    };
    let Some(Json::Arr(items)) = json_decode_first(&s) else {
        return Value::Null;
    };
    let bound = |v: Option<&Value>| match v {
        Some(Value::Int(i)) => Some(*i),
        Some(Value::Float(f)) => Some(f.round() as i64),
        Some(Value::Text(t)) => t.trim().parse().ok(),
        _ => None,
    };
    let lo = bound(a.get(1)).unwrap_or(1);
    let hi = bound(a.get(2)).unwrap_or(i64::MAX);
    let kept: Vec<Json> = (1i64..)
        .zip(items)
        .filter(|(p, _)| *p >= lo && *p <= hi)
        .map(|(_, j)| j)
        .collect();
    Value::Text(crate::builtins::json::json_marshal(&Json::Arr(kept)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_as_json_cases() {
        for (v, want) in [
            ("{1,2}", "[1,2]"),
            (
                r#"{a,"b,c","d\"e", f ,NULL,"NULL"}"#,
                r#"["a","b,c","d\"e","f",null,"NULL"]"#,
            ),
            (r#"{"{x}"}"#, r#"["{x}"]"#),
            ("[1, 2]", "[1, 2]"),
            ("1 2", "[1,2]"),
            ("{}", "[]"),
        ] {
            assert_eq!(
                array_as_json(&Value::Text(v.to_string())),
                Value::Text(want.to_string()),
                "{v}"
            );
        }
        assert_eq!(array_as_json(&Value::Text("abc".to_string())), Value::Null);
    }
}
