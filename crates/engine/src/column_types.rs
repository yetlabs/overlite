//! The type of a computed result column, which SQLite does not declare, read from
//! the select list: a cast decides it, and the rest speak for it only when there is
//! no row to sample.

use crate::blank::{blank_literals, code_only, is_word};
use crate::builtins::casts::CastType;
use crate::builtins::temporal::{Kind, result_kind};

/// What a statement's text says of a computed column's type, which SQLite does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TypeHint {
    /// Nothing.
    None,
    /// The type of the column whatever its values look like: a cast.
    Strong(&'static str),
    /// The type to report when there is no row to sample.
    Weak(&'static str),
}

impl TypeHint {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            TypeHint::None => "",
            TypeHint::Strong(name) | TypeHint::Weak(name) => name,
        }
    }
}

/// A type the protocol reads by name, for a cast's target.
fn cast_decl(name: &str) -> TypeHint {
    TypeHint::Strong(match CastType::parse(name) {
        Some(CastType::Int2 | CastType::Int4 | CastType::Int8) => "BIGINT",
        Some(CastType::Float4 | CastType::Float8) => "DOUBLE",
        Some(CastType::Numeric(_)) => "DECIMALTEXT",
        Some(CastType::Date) => "DATE",
        Some(CastType::Time(_)) => "TIME",
        Some(CastType::Timestamp(_)) => "TIMESTAMP",
        Some(CastType::TimestampTz(_)) => "TIMESTAMPTZ",
        Some(CastType::Interval) => "INTERVAL",
        Some(CastType::Uuid) => "UUID",
        Some(CastType::Json) => "JSON",
        Some(CastType::Jsonb) => "JSONB",
        Some(CastType::Bytea) => "BYTEA",
        Some(CastType::Varchar(_)) => "VARCHAR",
        Some(CastType::Char(_)) => "CHAR",
        Some(CastType::Range(kind)) => kind.name(),
        _ => return TypeHint::None,
    })
}

/// An array of elements of a type, as the protocol reads a declared type.
fn array_decl(elem: &str) -> TypeHint {
    TypeHint::Strong(match CastType::parse(elem) {
        Some(CastType::Int2 | CastType::Int4 | CastType::Int8) => "BIGINT[]",
        Some(CastType::Float4 | CastType::Float8) => "DOUBLE[]",
        Some(CastType::Numeric(_)) => "NUMERIC[]",
        Some(CastType::Date) => "DATE[]",
        Some(CastType::Timestamp(_)) => "TIMESTAMP[]",
        Some(CastType::TimestampTz(_)) => "TIMESTAMPTZ[]",
        Some(CastType::Uuid) => "UUID[]",
        Some(CastType::Jsonb) => "JSONB[]",
        Some(CastType::Bytea) => "BYTEA[]",
        Some(CastType::Varchar(_)) => "VARCHAR[]",
        _ if matches!(elem, "boolean" | "bool") => "BOOLEAN[]",
        _ => "TEXT[]",
    })
}

/// What an item of a select list is, by its outermost call.
fn item_type(item: &str) -> TypeHint {
    // Past a label, which rewrites and clients both write.
    let code = blank_literals(item);
    let end = last_top_level_as(&code).unwrap_or(item.len());
    let mut item = item[..end].trim();
    // Parentheses around the whole of it.
    while item.starts_with('(') && matching_close(item, 0) == Some(item.len() - 1) {
        item = item[1..item.len() - 1].trim();
    }
    let closed = |call: &str| {
        item.len() > call.len()
            && item.as_bytes()[..call.len()].eq_ignore_ascii_case(call.as_bytes())
            && matching_close(item, call.len() - 1) == Some(item.len() - 1)
    };
    // A numeric in the form resolve/numeric.rs writes.
    if is_numeric_form(item) {
        return TypeHint::Strong("DECIMALTEXT");
    }
    if closed("overlite_cast(") {
        let name = item[..item.len() - 1]
            .rsplit(", '")
            .next()
            .unwrap_or("")
            .trim_end_matches('\'');
        return cast_decl(name);
    }
    // What `->`, `#>` and a cast to jsonb give: rewrite/json.rs gives each the JSONB
    // collation, in parentheses of its own or around its chain.
    if item.ends_with("COLLATE JSONB)") || item.ends_with(") COLLATE JSONB") {
        return TypeHint::Strong("JSONB");
    }
    // A range's constructor makes one of its type.
    for range in [
        "int4range(",
        "int8range(",
        "numrange(",
        "daterange(",
        "tsrange(",
        "tstzrange(",
    ] {
        if closed(range) {
            return TypeHint::Strong(&range[..range.len() - 1]);
        }
    }
    if closed("overlite_now(") || closed("overlite_clock_timestamp(") {
        return TypeHint::Strong("TIMESTAMPTZ");
    }
    if closed("overlite_extract(") {
        return TypeHint::Strong("DECIMALTEXT");
    }
    // A sum of dates, times or intervals: resolve/temporal.rs.
    if closed("overlite_tadd(") || closed("overlite_tsub(") || closed("overlite_tmul(") {
        return sum_decl(item).map_or(TypeHint::None, TypeHint::Strong);
    }
    // A cast to an array, as the rewrite writes one.
    if closed("overlite_assign(") {
        let name = item[..item.len() - 1]
            .rsplit(", '")
            .next()
            .unwrap_or("")
            .trim_end_matches('\'');
        if let Some(elem) = name.strip_suffix("[]") {
            return array_decl(elem);
        }
        return cast_decl(name);
    }
    if closed("pg_bool(") || is_boolean_expr(item) {
        return TypeHint::Strong("BOOL");
    }
    if closed("overlite_from_time_zone(") {
        return TypeHint::Strong("TIMESTAMPTZ");
    }
    if closed("overlite_instant_at_time_zone(") || closed("overlite_column_at_time_zone(") {
        return TypeHint::Strong("TIMESTAMP");
    }
    if closed("regexp_split_to_array(") {
        return TypeHint::Strong("TEXT[]");
    }
    // An array whose elements' type its values say.
    if closed("array_agg(") || closed("overlite_array(") {
        return TypeHint::Strong("ANYARRAY");
    }
    if closed("count(") || closed("length(") || closed("char_length(") {
        return TypeHint::Weak("BIGINT");
    }
    if [
        "lower(",
        "upper(",
        "substr(",
        "substring(",
        "trim(",
        "replace(",
        "concat(",
        "coalesce(''",
    ]
    .iter()
    .any(|c| closed(c))
    {
        return TypeHint::Weak("TEXT");
    }
    if closed("datetime(") {
        return TypeHint::Weak("TIMESTAMP");
    }
    if item.starts_with('\'') && code[..end].trim_end().ends_with('\'') {
        return TypeHint::Weak("TEXT");
    }
    if item.parse::<i64>().is_ok() {
        return TypeHint::Weak("BIGINT");
    }
    if item.parse::<f64>().is_ok() {
        return TypeHint::Weak("DECIMALTEXT");
    }
    TypeHint::None
}

/// Functions whose value is a boolean, as this server writes them: the readings of a
/// boolean, the boolean aggregates, and what @>, ?, ?|, ?& and && become.
const BOOLEAN_FUNCTIONS: &[&str] = &[
    "pg_bool",
    "overlite_bool",
    "bool_and",
    "bool_or",
    "every",
    "json_contains",
    "jsonb_exists",
    "jsonb_exists_all",
    "jsonb_exists_any",
    "overlite_overlaps",
    "ts_match",
    "starts_with",
    "regexp_like",
    "inet_same_family",
    "overlite_trgm_match",
];

/// Functions whose value is one of their arguments: a boolean if one is.
const PASSING_FUNCTIONS: &[&str] = &["coalesce", "ifnull", "nullif", "greatest", "least"];

/// Whether a select-list item is a boolean, read from its text as Postgres types it: a
/// comparison, a logical operator, a test (IS, IN, LIKE, BETWEEN, EXISTS...), a boolean
/// literal or function, and COALESCE, NULLIF, GREATEST, LEAST or CASE over one. SQLite
/// gives each an integer, which went out as int8 1 where Postgres sends t.
pub(crate) fn is_boolean_expr(item: &str) -> bool {
    let code = blank_literals(item);
    let mut s = code.trim();
    let mut off = item.len() - item.trim_start().len();
    // Parentheses around the whole of it.
    while s.starts_with('(') && matching_close(s, 0) == Some(s.len() - 1) {
        let inner = &s[1..s.len() - 1];
        off += 1 + (inner.len() - inner.trim_start().len());
        s = inner.trim();
    }
    let lower = s.to_ascii_lowercase();
    if lower == "true" || lower == "false" {
        return true;
    }
    if lower.starts_with("select") && !lower.as_bytes().get(6).is_some_and(|c| is_word(*c)) {
        return false;
    }
    let b = lower.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    let mut words: Vec<(usize, usize, i32)> = Vec::new();
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' => {
                // Blank already; skip to the closing quote.
                i += 1;
                while i < b.len() && b[i] != c {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < b.len() && is_word(b[i]) {
                    i += 1;
                }
                match &lower[start..i] {
                    "case" => depth += 1,
                    "end" => depth -= 1,
                    _ => words.push((start, i, depth)),
                }
                continue;
            }
            _ if depth == 0 && b"<>=!".contains(&c) && (i == 0 || !b"<>=!~|&+-*/%@#^".contains(&b[i - 1])) => {
                let start = i;
                while i < b.len() && b"<>=!~|&+-*/%@#^".contains(&b[i]) {
                    i += 1;
                }
                if matches!(&lower[start..i], "=" | "==" | "<>" | "!=" | "<" | ">" | "<=" | ">=") {
                    return true;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    let top = |w: &str| words.iter().any(|&(a, e, d)| d == 0 && &lower[a..e] == w);
    if [
        "and", "or", "not", "is", "in", "like", "glob", "regexp", "match", "between", "exists",
    ]
    .iter()
    .any(|w| top(w))
    {
        return true;
    }
    // A call that is the whole item.
    if let Some(&(a, e, 0)) = words.first()
        && a == 0
        && s[e..].trim_start().starts_with('(')
    {
        let open = e + (s[e..].len() - s[e..].trim_start().len());
        if matching_close(s, open) == Some(s.len() - 1) {
            let name = &lower[a..e];
            if BOOLEAN_FUNCTIONS.contains(&name) {
                return true;
            }
            if PASSING_FUNCTIONS.contains(&name) {
                let args = &item[off + open + 1..off + s.len() - 1];
                return split_args(args).iter().any(|arg| is_boolean_expr(arg));
            }
        }
        return false;
    }
    // CASE ... END: a boolean if a branch is.
    if lower.starts_with("case") && lower.ends_with("end") {
        return case_results(&item[off..off + s.len()])
            .iter()
            .any(|r| is_boolean_expr(r));
    }
    false
}

/// The arguments of a call, at its top-level commas.
fn split_args(args: &str) -> Vec<&str> {
    let code = blank_literals(args);
    let mut out = Vec::new();
    let (mut depth, mut from) = (0i32, 0);
    for (i, c) in code.bytes().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&args[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(&args[from..]);
    out
}

/// What the THEN and ELSE branches of a CASE ... END say, at its own level.
fn case_results(case: &str) -> Vec<&str> {
    let code = blank_literals(case).to_ascii_lowercase();
    let b = code.as_bytes();
    let mut marks: Vec<(usize, usize, bool)> = Vec::new(); // (start, end, opens a result)
    let (mut depth, mut nest, mut i) = (0i32, 0i32, 0);
    while i < b.len() {
        let c = b[i];
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
        } else if c.is_ascii_alphabetic() && (i == 0 || !is_word(b[i - 1])) {
            let start = i;
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
            let w = &code[start..i];
            match w {
                "case" => nest += 1,
                "end" => {
                    nest -= 1;
                    if depth == 0 && nest == 0 {
                        marks.push((start, i, false));
                    }
                }
                "when" | "then" | "else" if depth == 0 && nest == 1 => marks.push((start, i, w != "when")),
                _ => {}
            }
            continue;
        }
        i += 1;
    }
    marks
        .windows(2)
        .filter(|w| w[0].2)
        .map(|w| &case[w[0].1..w[1].0])
        .collect()
}

/// Where the select list of the outermost SELECT lies. Blanked text, so a keyword in a
/// literal is no keyword.
pub(crate) fn select_list(sql: &str) -> Option<(usize, usize)> {
    let code = code_only(sql).to_ascii_lowercase();
    let b = code.as_bytes();
    let start = code.find("select")? + "select".len();
    if !code[..start - "select".len()].trim().is_empty() {
        return None;
    }
    let mut depth = 0;
    for i in start..b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b';' if depth == 0 => return Some((start, i)),
            _ if depth != 0 || (is_word(b[i - 1])) => {}
            _ => {
                for word in [
                    "from",
                    "where",
                    "group",
                    "order",
                    "limit",
                    "union",
                    "except",
                    "intersect",
                ] {
                    if b[i..].starts_with(word.as_bytes()) && !b.get(i + word.len()).copied().is_some_and(is_word) {
                        return Some((start, i));
                    }
                }
            }
        }
    }
    Some((start, b.len()))
}

/// The hints of a statement for the columns SQLite has no decltype for. Nothing to read
/// for a statement that names all its columns, which is what the hot path runs.
pub(crate) fn computed_types(sql: &str, decls: &[String]) -> Vec<TypeHint> {
    if decls.iter().all(|d| !d.is_empty()) {
        return Vec::new();
    }
    let Some((start, end)) = select_list(sql) else {
        return Vec::new();
    };
    let list = &sql[start..end];
    let code = blank_literals(list);
    let b = code.as_bytes();
    let mut items = Vec::with_capacity(decls.len());
    let (mut depth, mut from) = (0, 0);
    for i in 0..b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                items.push(&list[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    items.push(&list[from..]);
    // A star makes positions mean nothing.
    if items.len() != decls.len() || items.iter().any(|item| item.trim_end().ends_with('*')) {
        return Vec::new();
    }
    // The first item, past a DISTINCT or ALL.
    if let Some(first) = items.first_mut() {
        let t = first.trim_start();
        for word in ["distinct ", "all "] {
            if t.get(..word.len()).is_some_and(|w| w.eq_ignore_ascii_case(word)) {
                *first = &t[word.len()..];
            }
        }
    }
    items.iter().map(|item| item_type(item)).collect()
}

pub(crate) fn apply_strong_types(decls: &mut [String], hints: &[TypeHint]) {
    for (decl, hint) in decls.iter_mut().zip(hints) {
        if let (true, TypeHint::Strong(name)) = (decl.is_empty(), hint) {
            *decl = (*name).to_string();
        }
    }
}

/// The index of the parenthesis closing the one at open, literals skipped.
fn matching_close(sql: &str, open: usize) -> Option<usize> {
    let code = blank_literals(sql);
    let mut depth = 0;
    for (i, c) in code.bytes().enumerate().skip(open) {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The index of a label's AS, outside parentheses.
fn last_top_level_as(code: &str) -> Option<usize> {
    let b = code.as_bytes();
    let mut depth = 0;
    let mut found = None;
    for i in 0..b.len() {
        match b[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'a' | b'A' if depth == 0 && i > 0 && b[i - 1].is_ascii_whitespace() => {
                let word = b
                    .get(i..i + 3)
                    .is_some_and(|w| w[..2].eq_ignore_ascii_case(b"as") && w[2].is_ascii_whitespace());
                if word {
                    found = Some(i);
                }
            }
            _ => {}
        }
    }
    found
}

/// Whether t is written in the numeric form, with or without its parentheses.
pub(crate) fn is_numeric_form(t: &str) -> bool {
    let t = t.trim();
    let t = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(t);
    t.get(..5).is_some_and(|head| head.eq_ignore_ascii_case("CAST(")) && t.ends_with(" AS DECIMALTEXT) COLLATE DECIMAL")
}

/// What a call the printer writes gives, read from its text.
fn call_kind(item: &str) -> Option<Kind> {
    let t = item.trim();
    let closed = |name: &str| {
        t.len() > name.len() && t.get(..name.len()).is_some_and(|h| h.eq_ignore_ascii_case(name)) && t.ends_with(')')
    };
    let last_arg = || {
        t[..t.len() - 1]
            .rsplit(", '")
            .next()
            .map_or("", |a| a.trim_end_matches('\''))
    };
    if closed("overlite_cast(") {
        return Kind::of_decl(last_arg());
    }
    for (name, sub) in [("overlite_tadd(", false), ("overlite_tsub(", true)] {
        if closed(name) {
            let args: Vec<&str> = t[..t.len() - 1].rsplitn(3, ", '").collect();
            let kb = args.first().map(|a| a.trim_end_matches('\''))?;
            let ka = args.get(1).map(|a| a.trim_end_matches('\''))?;
            return result_kind(sub, Kind::of_name(ka)?, Kind::of_name(kb)?);
        }
    }
    if [
        "dec_add(",
        "dec_sub(",
        "dec_mul(",
        "dec_div(",
        "dec_round(",
        "dec_greatest(",
        "dec_least(",
        "overlite_numeric_op(",
    ]
    .iter()
    .any(|f| closed(f))
        || is_numeric_form(t)
    {
        return Some(Kind::Decimal);
    }
    if closed("overlite_tmul(") {
        return Some(Kind::Interval);
    }
    if closed("overlite_tz_in(") {
        return Some(Kind::TimestampTz);
    }
    if closed("overlite_now(") || closed("overlite_clock_timestamp(") || closed("datetime(") {
        return Some(Kind::TimestampTz);
    }
    if closed("date(") {
        return Some(Kind::Date);
    }
    // AT TIME ZONE, as rewrite and time_zone.rs write it.
    if closed("overlite_from_time_zone(") {
        return Some(Kind::TimestampTz);
    }
    if closed("overlite_instant_at_time_zone(") || closed("overlite_column_at_time_zone(") {
        return Some(Kind::Timestamp);
    }
    None
}

/// The declared type the protocol reads for a sum the printer wrote.
pub(crate) fn sum_decl(item: &str) -> Option<&'static str> {
    Some(match call_kind(item)? {
        Kind::Date => "DATE",
        Kind::Time => "TIME",
        Kind::Timestamp => "TIMESTAMP",
        Kind::TimestampTz => "TIMESTAMPTZ",
        Kind::Interval => "INTERVAL",
        Kind::Decimal => "DECIMALTEXT",
        Kind::Number => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boolean_items() {
        for item in [
            "1 = 1",
            "x IS NULL",
            "NOT x",
            "true",
            "(a < b)",
            "EXISTS (SELECT 1)",
            "COALESCE(pg_bool(max(overlite_bool(x = 1))), false)",
            "coalesce(a, false)",
            "nullif(a > 1, false)",
            "greatest(a = 1, b = 2)",
            "CASE WHEN a = 1 THEN true ELSE false END",
            "CASE WHEN a THEN x = 1 END",
            "a IN (1, 2)",
            "a LIKE 'x%'",
            "json_contains(a, '{}')",
            "a BETWEEN 1 AND 2",
        ] {
            assert!(is_boolean_expr(item), "{item}");
        }
        for item in [
            "count(*)",
            "a",
            "a + 1",
            "data -> 'k'",
            "data ->> 'k'",
            "'a = b'",
            "coalesce(a, 0)",
            "CASE WHEN a = 1 THEN 'x' ELSE 'y' END",
            "max(a = 1)",
            "(SELECT a = b FROM t)",
            "lower(x)",
            "a || '<>'",
        ] {
            assert!(!is_boolean_expr(item), "{item}");
        }
    }
}
