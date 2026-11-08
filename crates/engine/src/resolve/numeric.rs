//! numeric values in expressions. A numeric is stored as decimal text, which a column
//! declared `DECIMALTEXT COLLATE DECIMAL` compares as a number: the column's TEXT
//! affinity makes the other side text, and the collation compares the two as decimals.
//! An expression has no affinity and no collation, so the decimal text a cast, an exact
//! sum or SUM gives compared with a number as text with a number, which SQLite orders
//! by class: `23.09::numeric > 40` was true.
//!
//! Each expression of numeric type that is not a column is therefore written in the
//! form a column has, `(CAST(overlite_numeric(e) AS DECIMALTEXT) COLLATE DECIMAL)`: a CAST has the affinity of its type, TEXT, so a number or a parameter on
//! the other side is read as text and the DECIMAL collation compares them as decimals,
//! exactly, and an integer column on the other side makes it a number. ORDER BY, GROUP
//! BY, MIN and MAX go by the collation too. A column of an integer type keeps its index:
//! nothing is written around a column.
//!
//! A literal with more digits than a double holds is the same form over its text, so
//! that it is neither rounded on its way into a column nor in a comparison.

use super::{Kind, Tk, Tok, Typer, text, tokens};
use crate::column_types::select_list;

/// The form a numeric expression is written in, around e. overlite_numeric gives the
/// decimal text of what e gives, a double's shortest digits too.
fn numeric_form(e: &str) -> String {
    ["(CAST(overlite_numeric(", e, ") AS DECIMALTEXT) COLLATE DECIMAL)"].concat()
}

/// Whether t is written in the numeric form, with or without its parentheses.
pub(crate) fn is_numeric_form(t: &str) -> bool {
    let t = t.trim();
    let t = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(t);
    t.get(..5).is_some_and(|head| head.eq_ignore_ascii_case("CAST(")) && t.ends_with(" AS DECIMALTEXT) COLLATE DECIMAL")
}

/// Whether a statement reads or writes rows, where expressions are typed. A table's DDL
/// keeps what SQLite runs as it is.
pub(crate) fn reads_or_writes_rows(sql: &str) -> bool {
    let first = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    ["select", "with", "insert", "update", "delete", "values", "replace"]
        .iter()
        .any(|w| first.eq_ignore_ascii_case(w))
}

/// A cheap look for what numeric_affinity may change: a call that can give a numeric,
/// a CASE, or a long number. low is the statement in lower case.
pub(crate) fn might_hold_numerics(low: &str) -> bool {
    let b = low.as_bytes();
    let mut run = 0;
    for &c in b {
        if c.is_ascii_digit() {
            run += 1;
            if run > 15 {
                return true;
            }
        } else if c != b'.' {
            run = 0;
        }
    }
    [
        "overlite_cast(",
        "dec_",
        "case",
        "sum(",
        "avg(",
        "min(",
        "max(",
        "coalesce(",
        "greatest(",
        "least(",
        "nullif(",
        "ifnull(",
        "abs(",
        "round(",
        "ceil",
        "floor(",
        "trunc(",
        "_value(",
        "lag(",
        "lead(",
    ]
    .iter()
    .any(|w| low.contains(w))
}

/// Significant digits of a number literal: more than a double keeps is a numeric's.
fn is_long_number(lit: &str) -> bool {
    let mantissa = lit.split(['e', 'E']).next().unwrap_or(lit);
    if mantissa.len() != lit.len() {
        return false;
    }
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    digits.trim_start_matches('0').trim_end_matches('0').len() > 15
        || (!mantissa.contains('.') && digits.trim_start_matches('0').len() > 18)
}

/// The index of the token closing the parenthesis at open.
fn closing(sql: &str, toks: &[Tok], open: usize) -> Option<usize> {
    let d = toks[open].depth;
    (open + 1..toks.len()).find(|&k| toks[k].depth == d && text(sql, &toks[k]) == ")")
}

/// The end (exclusive) of the term that starts at i: a call with its FILTER and OVER,
/// or a CASE to its END. None for anything else.
fn term_end(sql: &str, toks: &[Tok], i: usize) -> Option<usize> {
    let t = &toks[i];
    if t.kind != Tk::Word {
        return None;
    }
    let word = text(sql, t);
    if word.eq_ignore_ascii_case("case") {
        let mut nested = 0;
        for (k, tk) in toks.iter().enumerate().skip(i + 1) {
            if tk.depth < t.depth {
                return None;
            }
            if tk.depth != t.depth || tk.kind != Tk::Word {
                continue;
            }
            let w = text(sql, tk);
            if w.eq_ignore_ascii_case("case") {
                nested += 1;
            } else if w.eq_ignore_ascii_case("end") {
                if nested == 0 {
                    return Some(k + 1);
                }
                nested -= 1;
            }
        }
        return None;
    }
    if toks.get(i + 1).is_none_or(|n| text(sql, n) != "(") {
        return None;
    }
    let mut end = closing(sql, toks, i + 1)? + 1;
    for word in ["filter", "over"] {
        if toks.get(end).is_some_and(|n| text(sql, n).eq_ignore_ascii_case(word))
            && toks.get(end + 1).is_some_and(|n| text(sql, n) == "(")
        {
            end = closing(sql, toks, end + 1)? + 1;
        }
    }
    Some(end)
}

/// The item of the select list a span is the whole expression of, and whether that
/// item has a label of its own.
fn whole_item(sql: &str, s: usize, e: usize) -> Option<bool> {
    let (a, b) = select_list(sql)?;
    if s < a || e > b {
        return None;
    }
    let list = crate::blank::blank_literals(&sql[a..b]);
    let (mut depth, mut from) = (0, 0);
    let mut items = Vec::new();
    for (i, c) in list.bytes().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                items.push((a + from, a + i));
                from = i + 1;
            }
            _ => {}
        }
    }
    items.push((a + from, b));
    let (x, y) = items.into_iter().find(|&(x, y)| x <= s && e <= y)?;
    let before = sql[x..s].trim().to_ascii_lowercase();
    if !(before.is_empty() || before == "distinct" || before == "all") {
        return None;
    }
    let after = sql[e..y].trim();
    if after.is_empty() {
        return Some(false);
    }
    let low = after.to_ascii_lowercase();
    (low.starts_with("as ") || low.starts_with('"') || after.chars().next().is_some_and(char::is_alphabetic))
        .then_some(true)
}

/// Writes each numeric expression that is not a column in the numeric form: a long
/// literal first, GREATEST and LEAST of numerics as theirs, then the outermost
/// numeric expressions.
pub(crate) fn numeric_affinity(typer: &mut Typer<'_>, sql: &str) -> String {
    let sql = long_literals(sql);
    let sql = numeric_extremes(typer, &sql);
    wrap_numerics(typer, &sql)
}

/// Each number literal with more digits than a double keeps, in the numeric form.
fn long_literals(sql: &str) -> String {
    let toks = tokens(sql);
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for t in toks
        .iter()
        .filter(|t| t.kind == Tk::Number && is_long_number(text(sql, t)))
    {
        out.push_str(&sql[at..t.start]);
        out.push_str(&numeric_form(&["'", text(sql, t), "'"].concat()));
        at = t.end;
    }
    out.push_str(&sql[at..]);
    out
}

/// GREATEST and LEAST over numerics as dec_greatest and dec_least, which compare their
/// arguments as numbers: SQLite's text order puts '9.5' after '10.25'.
fn numeric_extremes(typer: &mut Typer<'_>, sql: &str) -> String {
    let toks = tokens(sql);
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for (i, t) in toks.iter().enumerate() {
        let w = text(sql, t);
        if t.kind != Tk::Word || !(w.eq_ignore_ascii_case("greatest") || w.eq_ignore_ascii_case("least")) {
            continue;
        }
        let Some(end) = term_end(sql, &toks, i) else {
            continue;
        };
        if typer.kind(sql, &toks, (i, end)) == Some(Kind::Decimal) {
            out.push_str(&sql[at..t.start]);
            out.push_str("dec_");
            at = t.start;
        }
    }
    out.push_str(&sql[at..]);
    out
}

fn wrap_numerics(typer: &mut Typer<'_>, sql: &str) -> String {
    let toks = tokens(sql);
    let mut out = String::with_capacity(sql.len() + 64);
    let mut at = 0;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        // One already in the form, which is written in parentheses: past it whole.
        if text(sql, t) == "("
            && let Some(close) = closing(sql, &toks, i)
            && is_numeric_form(&sql[t.start..toks[close].end])
        {
            i = close + 1;
            continue;
        }
        let Some(end) = term_end(sql, &toks, i) else {
            i += 1;
            continue;
        };
        let (s, e) = (t.start, toks[end - 1].end);
        // The exact operators are written inside a cast to numeric, which is wrapped;
        // one called by name keeps the text it gives.
        let raw_operator = ["dec_add", "dec_sub", "dec_mul", "dec_div", "dec_round", "dec_cmp"]
            .iter()
            .any(|f| text(sql, t).eq_ignore_ascii_case(f));
        if raw_operator || typer.kind(sql, &toks, (i, end)) != Some(Kind::Decimal) {
            i += 1;
            continue;
        }
        let span = &sql[s..e];
        out.push_str(&sql[at..s]);
        out.push_str(&numeric_form(span));
        if whole_item(sql, s, e) == Some(false) {
            // SQLite's name for the column was the expression's text.
            out.push_str(" AS \"");
            out.push_str(&span.replace('"', "\"\""));
            out.push('"');
        }
        at = e;
        i = end;
    }
    out.push_str(&sql[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::collections::HashMap;

    #[test]
    fn numerics_compare_as_numbers() {
        let q = Connection::open_in_memory().unwrap();
        q.execute_batch("CREATE TABLE n (id int, a DECIMALTEXT(6,2), b DECIMALTEXT(6,2), s text)")
            .unwrap();
        // The probe of a column's type prepares the statement, so its functions exist.
        for f in ["greatest", "overlite_cast", "overlite_assign", "overlite_numeric"] {
            q.create_scalar_function(f, -1, rusqlite::functions::FunctionFlags::SQLITE_UTF8, |_| Ok(0))
                .unwrap();
        }
        let mut typer = Typer {
            q: &q,
            probes: HashMap::new(),
            version: None,
        };
        for (sql, want) in [
            (
                "SELECT 1 WHERE overlite_cast('23.09', 'numeric(6,2)') > 40",
                "SELECT 1 WHERE (CAST(overlite_numeric(overlite_cast('23.09', 'numeric(6,2)')) AS DECIMALTEXT) COLLATE DECIMAL) > 40",
            ),
            (
                "SELECT id FROM n WHERE GREATEST(a, b) = 1.2",
                "SELECT id FROM n WHERE (CAST(overlite_numeric(dec_GREATEST(a, b)) AS DECIMALTEXT) COLLATE DECIMAL) = 1.2",
            ),
            (
                "SELECT sum(a) FROM n HAVING sum(a) > 40",
                "SELECT (CAST(overlite_numeric(sum(a)) AS DECIMALTEXT) COLLATE DECIMAL) AS \"sum(a)\" FROM n HAVING (CAST(overlite_numeric(sum(a)) AS DECIMALTEXT) COLLATE DECIMAL) > 40",
            ),
            (
                "SELECT id FROM n WHERE CASE WHEN id = 0 THEN a ELSE NULL END = 1",
                "SELECT id FROM n WHERE (CAST(overlite_numeric(CASE WHEN id = 0 THEN a ELSE NULL END) AS DECIMALTEXT) COLLATE DECIMAL) = 1",
            ),
            (
                "SELECT id, count(*), lower(s) FROM n WHERE a > 1",
                "SELECT id, count(*), lower(s) FROM n WHERE a > 1",
            ),
            (
                "INSERT INTO n (a) VALUES (overlite_assign(0.100000000000000000000000000005, 'numeric(40,30)'))",
                "INSERT INTO n (a) VALUES (overlite_assign((CAST(overlite_numeric('0.100000000000000000000000000005') AS DECIMALTEXT) COLLATE DECIMAL), 'numeric(40,30)'))",
            ),
        ] {
            assert_eq!(numeric_affinity(&mut typer, sql), want, "{sql}");
        }
    }
}
