//! The tables a statement reads and the columns it compares, read from its tokens.
//! There is no parser here, so this reads only what it can be sure of: the table
//! after FROM, JOIN, UPDATE, INTO and USING with the alias that follows it, at any
//! depth, and a column written beside a parameter. A subquery, EXISTS, IN, a join
//! and a CTE are all just more of those. What a name means is left to the caller,
//! which knows the schema.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A word: a keyword or a name as written.
    Word,
    /// A double-quoted name.
    Quoted,
    /// A string literal, plain, E'' or dollar-quoted.
    Str,
    /// `$n` or `?`.
    Param,
    Num,
    Punct,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Tok {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) kind: Kind,
    /// Parentheses around it; one of its own counts as outside.
    pub(crate) depth: i32,
}

/// The tokens of sql, comments left out.
pub(crate) fn tokens(sql: &str) -> Vec<Tok> {
    let s = sql.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0;
    let mut i = 0;
    while i < s.len() {
        let start = i;
        let c = s[i];
        let kind = match c {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => {
                i += 1;
                continue;
            }
            b'-' if s.get(i + 1) == Some(&b'-') => {
                i = s[i..].iter().position(|c| *c == b'\n').map_or(s.len(), |n| i + n);
                continue;
            }
            b'/' if s.get(i + 1) == Some(&b'*') => {
                let mut d = 0;
                while i < s.len() {
                    if s[i..].starts_with(b"/*") {
                        d += 1;
                        i += 2;
                    } else if s[i..].starts_with(b"*/") {
                        d -= 1;
                        i += 2;
                        if d == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            b'\'' => {
                let escapes = i > 0 && (s[i - 1] | 0x20) == b'e' && (i < 2 || !is_word_byte(s[i - 2]));
                i += 1;
                while i < s.len() {
                    match s[i] {
                        b'\\' if escapes => i += 1,
                        b'\'' if s.get(i + 1) == Some(&b'\'') => i += 1,
                        b'\'' => break,
                        _ => {}
                    }
                    i += 1;
                }
                i = (i + 1).min(s.len());
                Kind::Str
            }
            b'"' => {
                i += 1;
                while i < s.len() && (s[i] != b'"' || s.get(i + 1) == Some(&b'"')) {
                    i += if s[i] == b'"' { 2 } else { 1 };
                }
                i = (i + 1).min(s.len());
                Kind::Quoted
            }
            b'$' if s.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i += 1;
                while i < s.len() && s[i].is_ascii_digit() {
                    i += 1;
                }
                Kind::Param
            }
            b'$' => {
                let tag_end = (i + 1..s.len()).find(|&j| !is_word_byte(s[j]));
                if let Some(j) = tag_end.filter(|&j| s[j] == b'$') {
                    let tag = &sql[i..=j];
                    i = sql[j + 1..].find(tag).map_or(s.len(), |n| j + 1 + n + tag.len());
                    Kind::Str
                } else {
                    i += 1;
                    Kind::Punct
                }
            }
            b'?' => {
                i += 1;
                while i < s.len() && s[i].is_ascii_digit() {
                    i += 1;
                }
                Kind::Param
            }
            b'0'..=b'9' => {
                while i < s.len() && (s[i].is_ascii_alphanumeric() || s[i] == b'.' || s[i] == b'_') {
                    i += 1;
                }
                Kind::Num
            }
            c if is_word_byte(c) => {
                while i < s.len() && (is_word_byte(s[i]) || s[i] == b'$') {
                    i += 1;
                }
                Kind::Word
            }
            _ => {
                let rest = &s[i..];
                let n = [&b"->>"[..], b"<>", b"!=", b"<=", b">=", b"==", b"||", b"::", b"->"]
                    .iter()
                    .find(|op| rest.starts_with(op))
                    .map_or(1, |op| op.len());
                i += n;
                Kind::Punct
            }
        };
        if c == b')' {
            depth -= 1;
        }
        out.push(Tok {
            start,
            end: i,
            kind,
            depth,
        });
        if c == b'(' {
            depth += 1;
        }
    }
    out
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

impl Tok {
    pub(crate) fn text<'s>(&self, sql: &'s str) -> &'s str {
        &sql[self.start..self.end]
    }

    pub(crate) fn is_word(&self, sql: &str, w: &str) -> bool {
        self.kind == Kind::Word && self.text(sql).eq_ignore_ascii_case(w)
    }

    pub(crate) fn is_punct(&self, sql: &str, p: &str) -> bool {
        self.kind == Kind::Punct && self.text(sql) == p
    }

    fn is_name(&self) -> bool {
        matches!(self.kind, Kind::Word | Kind::Quoted)
    }
}

/// A name as SQLite compares it: a quoted one unquoted, and all of it lowercase, as
/// SQLite matches table and column names without case.
pub(crate) fn ident(sql: &str, t: &Tok) -> String {
    let s = t.text(sql);
    if t.kind == Kind::Quoted && s.len() >= 2 && s.ends_with('"') {
        return s[1..s.len() - 1].replace("\"\"", "\"").to_lowercase();
    }
    s.to_lowercase()
}

/// The words that end a table reference rather than name its alias.
const NOT_ALIAS: &[&str] = &[
    "where",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "outer",
    "on",
    "using",
    "group",
    "order",
    "having",
    "limit",
    "offset",
    "fetch",
    "for",
    "set",
    "values",
    "default",
    "select",
    "union",
    "except",
    "intersect",
    "returning",
    "window",
    "lateral",
    "do",
    "as",
    "with",
    "tablesample",
    "only",
];

/// A dotted name starting at token i: its parts, and the index past it.
pub(crate) fn name_run(sql: &str, toks: &[Tok], i: usize) -> Option<(Vec<String>, usize)> {
    if !toks.get(i)?.is_name() {
        return None;
    }
    let mut parts = vec![ident(sql, &toks[i])];
    let mut j = i + 1;
    while j + 1 < toks.len() && toks[j].is_punct(sql, ".") && toks[j + 1].is_name() {
        parts.push(ident(sql, &toks[j + 1]));
        j += 2;
    }
    Some((parts, j))
}

/// Whether token i continues a dotted name, so that no name starts there.
pub(crate) fn inside_run(sql: &str, toks: &[Tok], i: usize) -> bool {
    i > 0 && toks[i - 1].is_punct(sql, ".")
}

/// A table a statement reads or writes.
#[derive(Debug, Clone)]
pub(crate) struct FromTable {
    /// Its name as written, quotes and schema included.
    pub(crate) text: String,
    /// The parts of its name, as ident gives them.
    pub(crate) parts: Vec<String>,
    pub(crate) alias: Option<String>,
}

/// The tables after FROM, JOIN, UPDATE, INTO and USING, at any depth, each with its
/// alias. A derived table or a function in FROM is no table, and its alias names
/// nothing here.
pub(crate) fn from_tables(sql: &str, toks: &[Tok]) -> Vec<FromTable> {
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t.kind != Kind::Word {
            continue;
        }
        let list = match t.text(sql).to_ascii_lowercase().as_str() {
            "from" | "using" => true,
            "join" | "update" | "into" => false,
            _ => continue,
        };
        let mut j = i + 1;
        while let Some(next) = table_ref(sql, toks, j, &mut out) {
            j = next;
            if !(list && toks.get(j).is_some_and(|c| c.is_punct(sql, ",") && c.depth == t.depth)) {
                break;
            }
            j += 1;
        }
    }
    out
}

/// Reads one table reference at token j into out; the index past it.
fn table_ref(sql: &str, toks: &[Tok], mut j: usize, out: &mut Vec<FromTable>) -> Option<usize> {
    while toks
        .get(j)
        .is_some_and(|t| t.is_word(sql, "only") || t.is_word(sql, "lateral"))
    {
        j += 1;
    }
    let t = toks.get(j)?;
    let table = if t.is_punct(sql, "(") {
        j = after_parens(toks, j)?;
        None
    } else {
        let (parts, end) = name_run(sql, toks, j)?;
        if toks.get(end).is_some_and(|t| t.is_punct(sql, "(")) {
            // A function in FROM, or INSERT's column list after INTO t: a function
            // has the name of no table, and a column list ends the reference.
            if toks[j].depth == toks[end].depth && is_into(sql, toks, j) {
                out.push(FromTable {
                    text: sql[toks[j].start..toks[end - 1].end].to_string(),
                    parts,
                    alias: None,
                });
                return Some(end);
            }
            j = after_parens(toks, end)?;
            None
        } else {
            let text = sql[toks[j].start..toks[end - 1].end].to_string();
            j = end;
            Some((text, parts))
        }
    };
    let mut alias = None;
    if toks.get(j).is_some_and(|t| t.is_word(sql, "as")) {
        j += 1;
        if toks.get(j).is_some_and(Tok::is_name) {
            alias = Some(ident(sql, &toks[j]));
            j += 1;
        }
    } else if let Some(t) = toks.get(j)
        && t.is_name()
        && (t.kind == Kind::Quoted || !NOT_ALIAS.contains(&ident(sql, t).as_str()))
    {
        alias = Some(ident(sql, t));
        j += 1;
    }
    if let Some((text, parts)) = table {
        out.push(FromTable { text, parts, alias });
    }
    Some(j)
}

fn is_into(sql: &str, toks: &[Tok], j: usize) -> bool {
    j > 0 && toks[j - 1].is_word(sql, "into")
}

/// The index past the parenthesis closing the one at token open.
pub(crate) fn after_parens(toks: &[Tok], open: usize) -> Option<usize> {
    let d = toks.get(open)?.depth;
    (open + 1..toks.len())
        .find(|&k| toks[k].depth == d && k > open)
        .map(|k| k + 1)
}

/// How a parameter meets a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Meets {
    /// Compared with it, or in its IN list, BETWEEN or arithmetic: the column's type.
    Value,
    /// `col = ANY($n)`: an array of the column's type.
    Array,
}

/// A column a parameter meets, by the parameter's number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamColumn {
    pub param: usize,
    /// The parts in front of the column's name, as ident gives them: an alias, a
    /// table, a schema and a table, or none.
    pub qualifier: Vec<String>,
    pub column: String,
    pub meets: Meets,
}

/// A table a statement names, for typing its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedTable {
    /// As written, quotes and schema included.
    pub text: String,
    /// The parts of its name, lowercase and unquoted.
    pub parts: Vec<String>,
    pub alias: Option<String>,
}

const COMPARISON: &[&str] = &["=", "<>", "!=", "<", ">", "<=", ">=", "+", "-", "*", "/"];

/// The tables a statement names and the column each of its parameters meets, where
/// one stands beside it: `col op $n`, `$n op col`, `col = ANY($n)`, `col [NOT] IN
/// (..., $n, ...)` and `col BETWEEN $n AND $m`, at any depth.
pub fn param_columns(sql: &str) -> (Vec<NamedTable>, Vec<ParamColumn>) {
    let toks = tokens(sql);
    let tables = from_tables(sql, &toks)
        .into_iter()
        .map(|t| NamedTable {
            text: t.text,
            parts: t.parts,
            alias: t.alias,
        })
        .collect();
    let mut out = Vec::new();
    for (k, t) in toks.iter().enumerate() {
        if t.kind != Kind::Param || !t.text(sql).starts_with('$') {
            continue;
        }
        let Ok(param) = t.text(sql)[1..].parse::<usize>() else {
            continue;
        };
        let mut push = |run: Option<Vec<String>>, meets: Meets| {
            if let Some(mut parts) = run {
                let column = parts.pop().unwrap_or_default();
                out.push(ParamColumn {
                    param,
                    qualifier: parts,
                    column,
                    meets,
                });
            }
        };
        let punct = |i: usize, set: &[&str]| {
            toks.get(i)
                .is_some_and(|t| t.kind == Kind::Punct && set.contains(&t.text(sql)))
        };
        // col op $n
        if k >= 2 && punct(k - 1, COMPARISON) {
            push(run_ending_at(sql, &toks, k - 2), Meets::Value);
            continue;
        }
        // $n op col
        if punct(k + 1, COMPARISON)
            && let Some((parts, end)) = name_run(sql, &toks, k + 2)
            && !toks
                .get(end)
                .is_some_and(|t| t.is_punct(sql, "(") || t.is_punct(sql, "."))
        {
            push(Some(parts), Meets::Value);
            continue;
        }
        // col = ANY ( $n )
        if k >= 4
            && toks[k - 1].is_punct(sql, "(")
            && toks[k - 2].is_word(sql, "any")
            && toks[k - 3].is_punct(sql, "=")
            && toks.get(k + 1).is_some_and(|t| t.is_punct(sql, ")"))
        {
            push(run_ending_at(sql, &toks, k - 4), Meets::Array);
            continue;
        }
        // col BETWEEN $n AND $m
        if k >= 2 && toks[k - 1].is_word(sql, "between") {
            push(run_ending_at(sql, &toks, k - 2), Meets::Value);
            continue;
        }
        if k >= 4 && toks[k - 1].is_word(sql, "and") && toks[k - 3].is_word(sql, "between") {
            push(run_ending_at(sql, &toks, k - 4), Meets::Value);
            continue;
        }
        // col [NOT] IN ( ..., $n, ... )
        if k >= 1
            && (toks[k - 1].is_punct(sql, "(") || toks[k - 1].is_punct(sql, ","))
            && toks
                .get(k + 1)
                .is_some_and(|t| t.is_punct(sql, ")") || t.is_punct(sql, ","))
            && let Some(open) = (0..k)
                .rev()
                .find(|&i| toks[i].depth == t.depth - 1 && toks[i].is_punct(sql, "("))
            && open >= 2
            && toks[open - 1].is_word(sql, "in")
            && (open + 1..k).all(|i| matches!(toks[i].kind, Kind::Param | Kind::Str | Kind::Num | Kind::Punct))
        {
            push(run_ending_at(sql, &toks, open - 2), Meets::Value);
        }
    }
    (tables, out)
}

/// The dotted name whose last token is at index last, unless it is a call.
fn run_ending_at(sql: &str, toks: &[Tok], last: usize) -> Option<Vec<String>> {
    let mut first = last;
    if !toks.get(last)?.is_name() {
        return None;
    }
    while first >= 2 && toks[first - 1].is_punct(sql, ".") && toks[first - 2].is_name() {
        first -= 2;
    }
    let (parts, end) = name_run(sql, toks, first)?;
    (end == last + 1).then_some(parts)
}

/// The tables of from_tables by the names a qualifier may use for them: the alias
/// when there is one, and the table's own name and its last part otherwise.
pub(crate) fn by_qualifier(tables: &[FromTable]) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for (i, t) in tables.iter().enumerate() {
        if let Some(a) = &t.alias {
            out.insert(a.clone(), i);
        } else {
            out.insert(t.parts.join("."), i);
            if let Some(last) = t.parts.last() {
                out.entry(last.clone()).or_insert(i);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<&str> {
        tokens(sql).iter().map(|t| t.text(sql)).collect()
    }

    #[test]
    fn tokens_cases() {
        assert_eq!(
            texts("SELECT \"a\"\"b\".c, 'it''s', $12, ? FROM t -- x\nWHERE a<>b /* y */"),
            [
                "SELECT",
                "\"a\"\"b\"",
                ".",
                "c",
                ",",
                "'it''s'",
                ",",
                "$12",
                ",",
                "?",
                "FROM",
                "t",
                "WHERE",
                "a",
                "<>",
                "b"
            ]
        );
        assert_eq!(
            texts("a->>'k' $$ x $$ E'\\''"),
            ["a", "->>", "'k'", "$$ x $$", "E", "'\\''"]
        );
    }

    #[test]
    fn from_tables_cases() {
        let names = |sql: &str| -> Vec<(String, Option<String>)> {
            from_tables(sql, &tokens(sql))
                .into_iter()
                .map(|t| (t.parts.join("."), t.alias))
                .collect()
        };
        let t = |n: &str, a: Option<&str>| (n.to_string(), a.map(str::to_string));
        assert_eq!(
            names("SELECT 1 FROM a, b x, c AS y WHERE 1"),
            [t("a", None), t("b", Some("x")), t("c", Some("y"))]
        );
        assert_eq!(
            names("SELECT 1 FROM p WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.b = $1)"),
            [t("p", None), t("t2", None)]
        );
        assert_eq!(
            names(r#"SELECT 1 FROM "public"."Q" AS "t1" LEFT JOIN r ON r.id = t1.id"#),
            [t("public.q", Some("t1")), t("r", None)]
        );
        assert_eq!(names("UPDATE ONLY u SET a = 1"), [t("u", None)]);
        assert_eq!(names("INSERT INTO v (a, b) VALUES (1, 2)"), [t("v", None)]);
        assert_eq!(
            names("SELECT 1 FROM (SELECT 1 FROM w) AS d, json_each(x) j"),
            [t("w", None)]
        );
        assert_eq!(
            names("DELETE FROM d1 USING d2 WHERE d1.id = d2.id"),
            [t("d1", None), t("d2", None)]
        );
    }

    #[test]
    fn param_columns_cases() {
        let cols = |sql: &str| -> Vec<(usize, String, Meets)> {
            param_columns(sql)
                .1
                .into_iter()
                .map(|p| {
                    let mut q = p.qualifier.join(".");
                    if !q.is_empty() {
                        q.push('.');
                    }
                    (p.param, q + &p.column, p.meets)
                })
                .collect()
        };
        let v = |n: usize, c: &str| (n, c.to_string(), Meets::Value);
        assert_eq!(
            cols("SELECT 1 FROM p WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.p_id = p.id AND t2.b = $1)"),
            [v(1, "t2.b")]
        );
        assert_eq!(
            cols("SELECT 1 FROM t WHERE $1 = x.b AND c + $2 > 0"),
            [v(1, "x.b"), v(2, "c")]
        );
        assert_eq!(
            cols(r#"SELECT 1 FROM t WHERE "t1"."isCorrect" = $3"#),
            [v(3, "t1.iscorrect")]
        );
        assert_eq!(
            cols("SELECT 1 FROM t WHERE a = ANY ($1)"),
            [(1, "a".to_string(), Meets::Array)]
        );
        assert_eq!(cols("SELECT 1 FROM t WHERE a IN ($1, $2)"), [v(1, "a"), v(2, "a")]);
        assert_eq!(
            cols("SELECT 1 FROM t WHERE a BETWEEN $1 AND $2"),
            [v(1, "a"), v(2, "a")]
        );
        assert_eq!(cols("SELECT f($1) FROM t WHERE lower(a) = $2"), []);
    }
}
