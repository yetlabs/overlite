//! What Postgres calls a result column the statement does not name.
//!
//! SQLite labels an unnamed expression with its text, and the text it sees is the
//! rewritten one: `count(*)`, `datetime('now')` for `now()`, `'postgres'` for
//! `current_user`. Postgres labels a function call with the function's name, a column
//! with the column's, a cast with its operand's or else the type's, CASE, EXISTS and
//! ARRAY with the keyword, and anything else `?column?` (FigureColname in its parser).
//! The labels are read from the statement as the client wrote it, before any pass, and
//! given as AS to the rewritten list once every pass has run, if it still has as many
//! items: the outermost select list, or a write's RETURNING list.

use overlite_core::text::quote_ident;
use overlite_engine::code_only;

/// One label per item of the result list, None where the item names itself (a column,
/// a star, an AS of its own).
pub(crate) type Labels = Option<Vec<Option<String>>>;

/// Niladic functions Postgres labels by their name, which a bare word otherwise is not.
const NILADIC: &[&str] = &[
    "current_date",
    "current_time",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    "current_user",
    "current_role",
    "session_user",
    "user",
    "current_catalog",
    "current_schema",
];

/// Words that end a select list at depth zero.
const LIST_END: &[&str] = &[
    "from",
    "into",
    "where",
    "group",
    "having",
    "window",
    "order",
    "limit",
    "offset",
    "fetch",
    "for",
    "union",
    "except",
    "intersect",
];

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

/// A word at depth zero, lowered, with its place.
struct Word {
    start: usize,
    end: usize,
    text: String,
}

fn top_words(code: &str) -> Vec<Word> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut i) = (0i32, 0);
    while i < b.len() {
        let c = b[i];
        if c == b'(' || c == b'[' {
            depth += 1;
        } else if c == b')' || c == b']' {
            depth -= 1;
        } else if (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !is_word(b[i - 1])) {
            let start = i;
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
            if depth == 0 {
                out.push(Word {
                    start,
                    end: i,
                    text: code[start..i].to_ascii_lowercase(),
                });
            }
            continue;
        }
        i += 1;
    }
    out
}

/// Where the result list lies: the outermost select list of a SELECT (past a WITH),
/// or the RETURNING list of a write.
fn result_list(sql: &str) -> Option<(usize, usize)> {
    let code = code_only(sql);
    let words = top_words(&code);
    let first = words.first()?;
    let end_of_text = sql.trim_end().trim_end_matches(';').trim_end().len();
    let verb = if first.text == "with" {
        words
            .iter()
            .find(|w| matches!(w.text.as_str(), "select" | "insert" | "update" | "delete" | "values"))?
    } else {
        first
    };
    match verb.text.as_str() {
        "select" => {
            let mut start = verb.end;
            let rest: Vec<&Word> = words.iter().filter(|w| w.start >= verb.end).collect();
            let mut k = 0;
            if let Some(w) = rest.first()
                && (w.text == "all" || w.text == "distinct")
            {
                start = w.end;
                k = 1;
                if w.text == "distinct" && rest.get(1).is_some_and(|w| w.text == "on") {
                    // DISTINCT ON (...): the list starts past its parenthesis.
                    let open = rest[1].end + code[rest[1].end..].find('(')?;
                    start = open + closing(&code, open)? + 1;
                    k = 2;
                }
            }
            let end = rest[k..]
                .iter()
                .find(|w| w.start >= start && LIST_END.contains(&w.text.as_str()))
                .map_or(end_of_text, |w| w.start);
            (start < end).then_some((start, end))
        }
        "insert" | "update" | "delete" => {
            let ret = words.iter().find(|w| w.start > verb.start && w.text == "returning")?;
            Some((ret.end, end_of_text))
        }
        _ => None,
    }
}

/// The offset of the parenthesis closing the one at open, from open.
fn closing(code: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in code[open..].bytes().enumerate() {
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

/// The items of a list, at its top-level commas, as ranges.
fn items(code: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut from) = (0i32, start);
    for (i, &c) in b.iter().enumerate().take(end).skip(start) {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth == 0 => {
                out.push((from, i));
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push((from, end));
    out
}

/// Where a label of an item begins: its AS, or a bare alias after the expression.
fn alias_at(code: &str) -> Option<usize> {
    let words = top_words(code);
    if let Some(w) = words.iter().rev().find(|w| w.text == "as") {
        return Some(w.start);
    }
    let trimmed = code.trim_end();
    // A bare quoted alias: `x "Total"`, but not `x COLLATE "C"` or `t."col"`.
    if trimmed.ends_with('"') && trimmed.len() > 1 {
        let open = trimmed[..trimmed.len() - 1].rfind('"')?;
        let before = trimmed[..open].trim_end();
        let spaced = before.len() < open;
        let after_collate = words
            .last()
            .is_some_and(|w| w.end == before.len() && w.text == "collate");
        let ends_expr = before
            .bytes()
            .last()
            .is_some_and(|c| is_word(c) || matches!(c, b')' | b']' | b'\'' | b'"'));
        return (spaced && ends_expr && !after_collate).then_some(open);
    }
    // A bare alias: a last word after something that cannot end in a name's middle.
    let last = words.last()?;
    if last.end != trimmed.len() || words.len() < 2 && code[..last.start].trim().is_empty() {
        return None;
    }
    let before = code[..last.start].trim_end();
    let prev = before.as_bytes().last().copied()?;
    let after_word = words.iter().rev().nth(1).is_some_and(|w| w.end == before.len());
    let keyword_before = words.iter().rev().nth(1).is_some_and(|w| {
        w.end == before.len()
            && matches!(
                w.text.as_str(),
                "and" | "or" | "not" | "is" | "in" | "like" | "between" | "then" | "else" | "when" | "case" | "collate"
            )
    });
    let ends_expr = matches!(prev, b')' | b']' | b'\'' | b'"') || prev.is_ascii_digit() || after_word;
    let ends_with_keyword = matches!(last.text.as_str(), "end" | "null" | "true" | "false" | "unknown");
    (ends_expr && !keyword_before && !ends_with_keyword).then_some(last.start)
}

/// The labels of the result list of sql, as the client wrote it.
pub(crate) fn result_labels(sql: &str) -> Labels {
    // Most statements name every column, or return none: told without allocating.
    let head = sql.trim_start().as_bytes();
    let starts = |w: &[u8]| head.len() >= w.len() && head[..w.len()].eq_ignore_ascii_case(w);
    if starts(b"select") {
        if crate::rewrite::fastpath::select_list_names_itself(sql) {
            return None;
        }
    } else if starts(b"insert") || starts(b"update") || starts(b"delete") {
        if !sql.as_bytes().windows(9).any(|w| w.eq_ignore_ascii_case(b"returning")) {
            return None;
        }
    } else if !starts(b"with") {
        return None;
    }
    let (start, end) = result_list(sql)?;
    let code = code_only(sql);
    let labels: Vec<Option<String>> = items(&code, start, end)
        .into_iter()
        .map(|(a, b)| {
            let item_code = &code[a..b];
            if item_code.trim().is_empty() || alias_at(item_code).is_some() {
                return None;
            }
            label(&sql[a..b])
        })
        .collect();
    labels.iter().any(Option::is_some).then_some(labels)
}

/// Gives each item of the rewritten statement's result list its label, replacing an AS
/// a pass wrote. Nothing changes if the list no longer has as many items.
pub(crate) fn apply_labels(sql: &str, labels: &Labels) -> String {
    let Some(labels) = labels else {
        return sql.to_string();
    };
    let Some((start, end)) = result_list(sql) else {
        return sql.to_string();
    };
    let code = code_only(sql);
    let spans = items(&code, start, end);
    if spans.len() != labels.len() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + 16 * labels.len());
    let mut at = 0;
    for ((a, b), label) in spans.into_iter().zip(labels) {
        let Some(label) = label else {
            continue;
        };
        let item = code[a..b].trim_end();
        if item.ends_with('*') || item.trim().is_empty() {
            return sql.to_string();
        }
        let cut = match alias_at(&code[a..a + item.len()]) {
            Some(p) => a + code[a..a + p].trim_end().len(),
            None => a + item.len(),
        };
        out.push_str(&sql[at..cut]);
        out.push_str(" AS ");
        out.push_str(&quote_ident(label));
        at = a + item.len();
    }
    out.push_str(&sql[at..]);
    out
}

/// Words Postgres reads as more than a name where one could stand: a select-list item of
/// bare names and none of these is a column or garbage, and either way SQLite's label
/// stands. needs_no_rewrite takes a list of names only without these.
pub(crate) fn is_label_word(w: &[u8]) -> bool {
    matches!(
        w,
        b"true"
            | b"false"
            | b"null"
            | b"user"
            | b"distinct"
            | b"all"
            | b"case"
            | b"is"
            | b"isnull"
            | b"notnull"
            | b"not"
            | b"and"
            | b"or"
            | b"in"
            | b"like"
            | b"between"
            | b"exists"
            | b"collate"
            | b"similar"
            | b"glob"
            | b"regexp"
            | b"match"
            | b"escape"
    ) || w.starts_with(b"current_")
        || w.starts_with(b"local")
        || w.starts_with(b"session_")
}

/// Postgres' label for an expression, None for a column or a star, which SQLite
/// already names as Postgres does.
fn label(item: &str) -> Option<String> {
    let plain = item
        .bytes()
        .all(|c| is_word(c) || matches!(c, b'.' | b'*' | b' ' | b'\t' | b'\n' | b'\r'));
    let mut words = item.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$');
    if plain
        && words.all(|w| {
            !w.starts_with(|c: char| c.is_ascii_digit() || c == '$')
                && !is_label_word(w.to_ascii_lowercase().as_bytes())
        })
    {
        return None;
    }
    match figure(item.trim()) {
        Figure::Named(name) | Figure::Typed(name) => Some(name),
        Figure::Column => None,
        Figure::Unnamed => Some("?column?".to_string()),
    }
}

enum Figure {
    /// A plain column reference or a star: SQLite's own label is right.
    Column,
    /// A name of strength 2: a function's, a column's through an expression.
    Named(String),
    /// A type's name, strength 1: what a cast of something unnamed is called.
    Typed(String),
    /// `?column?`.
    Unnamed,
}

fn fold(name: &str) -> String {
    match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => name.to_ascii_lowercase(),
    }
}

/// The parts of a dotted name, or None if s is not one.
fn name_parts(s: &str) -> Option<Vec<&str>> {
    let b = s.as_bytes();
    let mut parts = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        if b.get(i) == Some(&b'"') {
            i += 1;
            loop {
                let k = i + s[i..].find('"')?;
                i = k + 1;
                if b.get(i) != Some(&b'"') {
                    break;
                }
                i += 1;
            }
        } else {
            if !b
                .get(i)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_' || *c >= 0x80)
            {
                return None;
            }
            while i < b.len() && (is_word(b[i]) || b[i] >= 0x80) {
                i += 1;
            }
        }
        parts.push(&s[start..i]);
        if i == b.len() {
            return Some(parts);
        }
        if b[i] != b'.' {
            return None;
        }
        i += 1;
    }
}

/// The name Postgres gives a type in a label.
fn type_label(ty: &str) -> String {
    let ty = ty.trim();
    let base = ty.split(['(', '[']).next().unwrap_or(ty).trim();
    let base = base.rsplit('.').next().unwrap_or(base);
    if base.starts_with('"') {
        return fold(base);
    }
    let words: Vec<String> = base.split_whitespace().map(str::to_ascii_lowercase).collect();
    let joined = words.join(" ");
    let with_zone = joined.contains("with time zone");
    match words.first().map_or("", String::as_str) {
        "int" | "integer" | "int4" => "int4",
        "bigint" | "int8" => "int8",
        "smallint" | "int2" => "int2",
        "real" | "float4" => "float4",
        "double" | "float8" | "float" => "float8",
        "boolean" | "bool" => "bool",
        "decimal" | "numeric" => "numeric",
        "varchar" => "varchar",
        "character" if joined.starts_with("character varying") => "varchar",
        "char" | "character" | "bpchar" => "bpchar",
        "timestamp" if with_zone => "timestamptz",
        "time" if with_zone => "timetz",
        w => return w.to_string(),
    }
    .to_string()
}

/// FigureColnameInternal over the text of one expression.
fn figure(s: &str) -> Figure {
    let code = code_only(s);
    let s_code = code.as_ref();
    // Parentheses around all of it.
    if s_code.starts_with('(') && closing(s_code, 0) == Some(s_code.len() - 1) {
        let inner = s[1..s.len() - 1].trim();
        let inner_code = code_only(inner);
        if inner_code.trim_start().len() >= 6 && inner_code[..6].eq_ignore_ascii_case("select") {
            // A scalar subquery: the label of its one column.
            return match result_list(inner) {
                Some((a, b)) => {
                    let first = items(&inner_code, a, b)[0];
                    let it = &inner[first.0..first.1];
                    match alias_at(&inner_code[first.0..first.1]) {
                        Some(p) => {
                            let alias = it[p..].trim();
                            let alias = alias
                                .strip_prefix("AS")
                                .or_else(|| alias.strip_prefix("as"))
                                .or_else(|| alias.strip_prefix("As"))
                                .or_else(|| alias.strip_prefix("aS"))
                                .unwrap_or(alias);
                            Figure::Named(fold(alias.trim()))
                        }
                        None => match figure(it.trim()) {
                            Figure::Column => Figure::Named(column_name(it.trim())),
                            Figure::Typed(n) => Figure::Named(n),
                            f => f,
                        },
                    }
                }
                None => Figure::Unnamed,
            };
        }
        return match figure(inner) {
            Figure::Column => Figure::Named(column_name(inner)),
            f => f,
        };
    }
    if s_code.ends_with('*') {
        return Figure::Column;
    }
    // A cast written x::type, the last at depth zero.
    if let Some(p) = last_top_level(s_code, "::") {
        let ty = &s[p + 2..];
        return match figure(s[..p].trim()) {
            Figure::Named(n) => Figure::Named(n),
            Figure::Column => Figure::Named(column_name(s[..p].trim())),
            _ => Figure::Typed(type_label(ty)),
        };
    }
    let words = top_words(s_code);
    // x COLLATE c: what x is called.
    if let Some(w) = words.iter().find(|w| w.text == "collate") {
        return match figure(s[..w.start].trim()) {
            Figure::Column => Figure::Named(column_name(s[..w.start].trim())),
            f => f,
        };
    }
    if let Some(parts) = name_parts(s) {
        let last = parts[parts.len() - 1];
        if parts.len() == 1 && !last.starts_with('"') {
            let low = last.to_ascii_lowercase();
            if NILADIC.contains(&low.as_str()) {
                return Figure::Named(low);
            }
            if matches!(low.as_str(), "true" | "false" | "null" | "default") {
                return Figure::Unnamed;
            }
        }
        return Figure::Column;
    }
    let first = words.first();
    if let Some(w) = first
        && w.start == 0
    {
        let rest = s_code[w.end..].trim_start();
        match w.text.as_str() {
            "case" if s_code.len() >= 3 && s_code[s_code.len() - 3..].eq_ignore_ascii_case("end") => {
                return Figure::Named("case".into());
            }
            "exists" if rest.starts_with('(') => return Figure::Named("exists".into()),
            "array" if rest.starts_with('[') || rest.starts_with('(') => return Figure::Named("array".into()),
            "row" if rest.starts_with('(') => return Figure::Named("row".into()),
            "cast" if rest.starts_with('(') => {
                let open = s_code.len() - rest.len();
                if closing(s_code, open) == Some(s_code.len() - 1 - open) {
                    let inner = &s[open + 1..s.len() - 1];
                    let inner_code = &s_code[open + 1..s_code.len() - 1];
                    if let Some(at) = top_words(inner_code)
                        .iter()
                        .rev()
                        .find(|w| w.text == "as")
                        .map(|w| w.start)
                    {
                        let operand = inner[..at].trim();
                        return match figure(operand) {
                            Figure::Named(n) => Figure::Named(n),
                            Figure::Column => Figure::Named(column_name(operand)),
                            _ => Figure::Typed(type_label(&inner[at + 2..])),
                        };
                    }
                }
            }
            _ => {}
        }
        // A typed literal: `date '2024-01-02'`, `interval '1 day'`.
        if s_code.ends_with('\'') && !s_code.contains('(') {
            let lit = s_code.find('\'').unwrap_or(0);
            if words.iter().all(|w| w.end <= lit) && lit > 0 {
                return Figure::Typed(type_label(&s[..lit]));
            }
        }
    }
    // A call, perhaps qualified, perhaps with FILTER, OVER or WITHIN GROUP after it.
    if let Some(open) = s_code.find('(')
        && let Some(parts) = name_parts(s[..open].trim_end())
        && let Some(close) = closing(s_code, open).map(|c| c + open)
    {
        let tail = top_words(&s_code[close + 1..]);
        let tail_ok = s_code[close + 1..].trim().is_empty()
            || tail
                .first()
                .is_some_and(|w| matches!(w.text.as_str(), "filter" | "over" | "within"));
        if tail_ok {
            let name = fold(parts[parts.len() - 1]);
            let inner = s_code[open + 1..close].to_ascii_lowercase();
            let name = match name.as_str() {
                "trim" if top_words(&inner).first().is_some_and(|w| w.text == "leading") => "ltrim".into(),
                "trim" if top_words(&inner).first().is_some_and(|w| w.text == "trailing") => "rtrim".into(),
                "trim" => "btrim".into(),
                _ => name,
            };
            return Figure::Named(name);
        }
    }
    Figure::Unnamed
}

/// The name of a column reference: its last part.
fn column_name(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix('(')
        .and_then(|x| x.strip_suffix(')'))
        .unwrap_or(s)
        .trim();
    match name_parts(s) {
        Some(parts) => fold(parts[parts.len() - 1]),
        None => "?column?".to_string(),
    }
}

/// The offset of the last op at depth zero in code.
fn last_top_level(code: &str, op: &str) -> Option<usize> {
    let b = code.as_bytes();
    let mut depth = 0i32;
    let mut found = None;
    for i in 0..b.len() {
        match b[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ if depth == 0 && b[i..].starts_with(op.as_bytes()) => found = Some(i),
            _ => {}
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_of(sql: &str) -> Vec<String> {
        result_labels(sql)
            .unwrap_or_default()
            .into_iter()
            .map(|l| l.unwrap_or_else(|| "-".into()))
            .collect()
    }

    // Each as Postgres 17 labels it; `-` is a column SQLite names itself.
    #[test]
    fn labels_as_postgres() {
        assert_eq!(
            labels_of(
                "SELECT count(*), max(1), 1+1, true, 'a', 1, now(), current_timestamp, current_user, x.a, \
                 x.a::text, '1'::int, (select 1 as z), exists(select 1), case when true then 1 end, \
                 coalesce(1,2), array[1], -x.a, (x.a), x.a is null, not true, interval '1 day', \
                 date '2020-01-01', pg_catalog.lower('A'), lower('A') collate \"C\", nullif(1,2), \
                 cast(x.a as int), null, x.*, a, (count(*)), 'x'::text, x.a + 1 as y, upper(lower('a')), \
                 extract(year from now()), trim('a'), \"x\".\"a\", true::int, json_build_object('a',1), \
                 count(*)::int, sum(a) OVER (), (SELECT b FROM t), x.a b FROM x"
            ),
            [
                "count",
                "max",
                "?column?",
                "?column?",
                "?column?",
                "?column?",
                "now",
                "current_timestamp",
                "current_user",
                "-",
                "a",
                "int4",
                "z",
                "exists",
                "case",
                "coalesce",
                "array",
                "?column?",
                "a",
                "?column?",
                "?column?",
                "interval",
                "date",
                "lower",
                "lower",
                "nullif",
                "a",
                "?column?",
                "-",
                "-",
                "count",
                "text",
                "-",
                "upper",
                "extract",
                "btrim",
                "-",
                "int4",
                "json_build_object",
                "count",
                "sum",
                "b",
                "-",
            ]
        );
    }

    #[test]
    fn labels_where_they_go() {
        assert_eq!(labels_of("SELECT a, b FROM t"), Vec::<String>::new());
        assert_eq!(labels_of("SELECT DISTINCT count(*) FROM t"), ["count"]);
        assert_eq!(labels_of("SELECT DISTINCT ON (a) a, max(b) FROM t"), ["-", "max"]);
        assert_eq!(labels_of("WITH c AS (SELECT 1) SELECT sum(x) FROM c"), ["sum"]);
        assert_eq!(
            labels_of("INSERT INTO t VALUES (1) RETURNING id, id + 1"),
            ["-", "?column?"]
        );
        assert_eq!(labels_of("SELECT 1 UNION SELECT 2"), ["?column?"]);
        assert_eq!(
            labels_of(r#"SELECT 1 x, count(*) "Total", lower(a) COLLATE "C" FROM t"#),
            ["-", "-", "lower"]
        );
        assert_eq!(labels_of("INSERT INTO t SELECT count(*) FROM u"), Vec::<String>::new());
        let sql = "SELECT count(*), a FROM t";
        assert_eq!(
            apply_labels(sql, &result_labels(sql)),
            r#"SELECT count(*) AS "count", a FROM t"#
        );
        let sql = "SELECT 1+1";
        assert_eq!(apply_labels(sql, &result_labels(sql)), r#"SELECT 1+1 AS "?column?""#);
        // An AS a pass wrote gives way.
        let labels = result_labels("SELECT x::int FROM t");
        assert_eq!(
            apply_labels("SELECT overlite_cast(x, 'int') AS x FROM t", &labels),
            r#"SELECT overlite_cast(x, 'int') AS "x" FROM t"#
        );
    }
}
