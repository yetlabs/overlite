//! Exact numeric: a `numeric`/`decimal[(p,s)]` column is stored as its exact
//! decimal string in a TEXT cell that carries the DECIMAL collation, so storage
//! is lossless and comparison/ordering are numeric (see engine/decimal). We
//! re-type the column to `DECIMALTEXT COLLATE DECIMAL`: DECIMALTEXT gives it TEXT
//! affinity and lets overlite advertise OID 1700; the collation makes
//! `<`/`>`/`ORDER BY` compare as numbers.

use crate::rewrite::tokens::{CodeTok, code_tokens};

/// The types that take a modifier, and could have one before array brackets.
const MODIFIED_TYPES: [&str; 12] = [
    "numeric",
    "decimal",
    "varchar",
    "varying",
    "char",
    "character",
    "bpchar",
    "timestamp",
    "timestamptz",
    "time",
    "timetz",
    "bit",
];

/// Quotes a type with a modifier before its brackets: `numeric(10,2)[]` is no type to
/// SQLite, which takes parentheses last, but a quoted name is one, and SQLite gives
/// the column that name, unquoted, as its declared type. So the modifier reaches the
/// catalogs, while the value stays a JSON array in a cell of no special affinity.
fn quote_modified_arrays(sql: &str) -> String {
    let toks = code_tokens(sql);
    let is_punct = |i: usize, p: &str| toks.get(i).is_some_and(|t| t.is_punct(sql, p));
    let is_word = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
    let mut out = String::with_capacity(sql.len() + 8);
    let mut at = 0;
    let mut i = 0;
    while i < toks.len() {
        let word = i;
        i += 1;
        if !MODIFIED_TYPES.iter().any(|w| toks[word].is_word(sql, w)) {
            continue;
        }
        if !is_punct(word + 1, "(") || !is_precision(sql, &toks, word + 1) {
            continue;
        }
        let Some(close) = (word + 1..toks.len()).find(|&j| is_punct(j, ")")) else {
            continue;
        };
        let mut type_end = close;
        if (is_word(close + 1, "with") || is_word(close + 1, "without"))
            && is_word(close + 2, "time")
            && is_word(close + 3, "zone")
        {
            type_end = close + 3;
        }
        let mut end = type_end + 1;
        let mut brackets = 0;
        while is_punct(end, "[") {
            match (end..toks.len()).find(|&j| is_punct(j, "]")) {
                Some(j) => end = j + 1,
                None => break,
            }
            brackets += 1;
        }
        if brackets == 0 {
            continue;
        }
        let start = if toks[word].is_word(sql, "varying") && word > 0 && is_word(word - 1, "character") {
            word - 1
        } else {
            word
        };
        out.push_str(&sql[at..toks[start].span.start]);
        out.push('"');
        out.push_str(&sql[toks[start].span.start..toks[type_end].span.end]);
        out.push_str(&"[]".repeat(brackets));
        out.push('"');
        at = toks[end - 1].span.end;
        i = end;
    }
    out.push_str(&sql[at..]);
    out
}

/// Writes each numeric or decimal type in sql as the marker. An array keeps its type:
/// its value is a JSON array, not a decimal. A literal or a quoted name that spells the
/// type is not one.
pub(crate) fn retype_numeric(sql: &str) -> String {
    let sql = &quote_modified_arrays(sql);
    let toks = code_tokens(sql);
    let is_punct = |i: usize, p: &str| toks.get(i).is_some_and(|t| t.is_punct(sql, p));
    let mut out = String::with_capacity(sql.len() + 32);
    let mut at = 0;
    let mut i = 0;
    while i < toks.len() {
        if !(toks[i].is_word(sql, "numeric") || toks[i].is_word(sql, "decimal")) {
            i += 1;
            continue;
        }
        // Where a column's name goes it is one: `numeric numeric(4,1)`, `UNIQUE (numeric)`.
        let names = i > 0
            && (is_punct(i - 1, "(")
                || is_punct(i - 1, ",")
                || ["add", "column", "exists"].iter().any(|w| toks[i - 1].is_word(sql, w)));
        if names {
            i += 1;
            continue;
        }
        let word = &sql[toks[i].span.clone()];
        let mut end = i + 1;
        if is_punct(end, "(") && is_precision(sql, &toks, end) {
            end = (end..toks.len()).find(|&j| is_punct(j, ")")).map_or(end, |j| j + 1);
        }
        let modifier = &sql[toks[i].span.end..toks[end - 1].span.end];
        let mut brackets = 0;
        while is_punct(end, "[") {
            let close = (end..toks.len()).find(|&j| is_punct(j, "]"));
            end = close.map_or(end + 1, |j| j + 1);
            brackets += 1;
        }
        out.push_str(&sql[at..toks[i].span.start]);
        if brackets > 0 {
            out.push_str(word);
            out.push_str(&"[]".repeat(brackets));
        } else {
            out.push_str("DECIMALTEXT");
            out.push_str(modifier);
            out.push_str(" COLLATE DECIMAL");
        }
        at = toks[end - 1].span.end;
        i = end;
    }
    out.push_str(&sql[at..]);
    out
}

/// Whether the parenthesis at toks[open] holds a precision: one or two numbers.
fn is_precision(sql: &str, toks: &[CodeTok], open: usize) -> bool {
    let number = |i: usize| {
        toks.get(i)
            .is_some_and(|t| sql[t.span.clone()].bytes().all(|c| c.is_ascii_digit()))
    };
    let is_punct = |i: usize, p: &str| toks.get(i).is_some_and(|t| t.is_punct(sql, p));
    number(open + 1)
        && (is_punct(open + 2, ")") || (is_punct(open + 2, ",") && number(open + 3) && is_punct(open + 4, ")")))
}

const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;
/// The dscale numeric_send gives an infinity.
const NUMERIC_INF_DSCALE: u16 = 32;

/// numeric_send of a decimal text: ndigits, weight, sign and dscale, then the digits
/// in base 10000, aligned on the decimal point, without the zero groups at either
/// end. None for a text that is not a number, or too large for those fields.
pub(crate) fn numeric_binary(text: &str) -> Option<Vec<u8>> {
    let s = text.trim();
    match s.to_ascii_lowercase().as_str() {
        "nan" => return Some(numeric_fields(0, NUMERIC_NAN, 0, &[])),
        "infinity" | "+infinity" | "inf" | "+inf" => {
            return Some(numeric_fields(0, NUMERIC_PINF, NUMERIC_INF_DSCALE, &[]));
        }
        "-infinity" | "-inf" => return Some(numeric_fields(0, NUMERIC_NINF, NUMERIC_INF_DSCALE, &[])),
        _ => {}
    }
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int.len() + frac.len() == 0 || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let dscale = u16::try_from((frac.len() as i64).checked_sub(exp)?.max(0)).ok()?;

    // Where the decimal point falls in the digits; move it to a group boundary.
    let point = (int.len() as i64).checked_add(exp)?;
    let lead = (4 - point.rem_euclid(4)) % 4;
    let point = point + lead;
    let weight = point / 4 - 1;
    i16::try_from(weight).ok()?;
    let mut digits = vec![0u8; lead as usize];
    digits.extend(int.bytes().chain(frac.bytes()).map(|c| c - b'0'));
    let len = (digits.len() as i64).max(point);
    digits.resize((len + (4 - len.rem_euclid(4)) % 4) as usize, 0);

    let mut groups: Vec<u16> = digits
        .chunks(4)
        .map(|g| g.iter().fold(0, |n, d| n * 10 + *d as u16))
        .collect();
    while groups.last() == Some(&0) {
        groups.pop();
    }
    let zeros = groups.iter().take_while(|g| **g == 0).count();
    groups.drain(..zeros);
    if groups.is_empty() {
        return Some(numeric_fields(0, 0, dscale, &[]));
    }
    i16::try_from(groups.len()).ok()?;
    let sign = if negative { NUMERIC_NEG } else { 0 };
    Some(numeric_fields(
        i16::try_from(weight - zeros as i64).ok()?,
        sign,
        dscale,
        &groups,
    ))
}

fn numeric_fields(weight: i16, sign: u16, dscale: u16, groups: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + groups.len() * 2);
    out.extend_from_slice(&(groups.len() as u16).to_be_bytes());
    out.extend_from_slice(&weight.to_be_bytes());
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&dscale.to_be_bytes());
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
    out
}
