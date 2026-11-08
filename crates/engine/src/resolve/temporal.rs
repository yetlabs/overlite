//! Dates, times, timestamps and intervals in expressions, where SQLite sees text.
//!
//! A `+` or `-` with a side of such a type becomes `overlite_tadd(a, b, 'KA', 'KB')` or
//! `overlite_tsub` (builtins/temporal.rs), each side's kind named when the statement
//! says it: a column by its declared type, read with the probe of time_zone.rs, a cast
//! by its type, a sum by its sides. SQLite took `'2010-06-25 12:15:30' + '1 day'` for
//! 2011. A side the statement does not type, a literal or a parameter, is read by its
//! value.
//!
//! An item of the select list whose type Postgres knows and SQLite does not, MIN, MAX,
//! COALESCE, GREATEST, LEAST, NULLIF and the window functions over such a column,
//! DATE_TRUNC, the sum of intervals or a sum above, is cast to that type, which tells the
//! protocol what to send: a date went out as text.

use crate::blank::blank_literals;
use crate::builtins::temporal::{Kind, result_kind};
use crate::column_types::select_list;
use crate::resolve::time_zone::column_decl_cached;
pub(crate) use numeric::is_numeric_form;
use numeric::{might_hold_numerics, numeric_affinity, reads_or_writes_rows};

#[path = "numeric.rs"]
mod numeric;
use rusqlite::Connection;
use std::borrow::Cow;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tk {
    Word,
    Number,
    Str,
    Quoted,
    Punct,
}

#[derive(Clone, Debug)]
struct Tok {
    start: usize,
    end: usize,
    kind: Tk,
    /// Parentheses around it; one of its own counts as outside.
    depth: i32,
}

fn tokens(sql: &str) -> Vec<Tok> {
    let code = blank_literals(sql);
    let b = code.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'\'' => {
                i += 1;
                while i < b.len() && b[i] != b'\'' {
                    i += 1;
                }
                i += 1;
                Tk::Str
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                i += 1;
                Tk::Quoted
            }
            b'0'..=b'9' => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                    i += 1;
                }
                Tk::Number
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                Tk::Word
            }
            _ => {
                let two = b.get(i..i + 2).unwrap_or(&[]);
                i += if b.get(i..i + 3) == Some(b"->>") {
                    3
                } else if [&b"->"[..], b"||", b"<=", b">=", b"<>", b"!=", b"::", b"=="].contains(&two) {
                    2
                } else {
                    1
                };
                Tk::Punct
            }
        };
        let i = i.min(b.len());
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

/// Words that end an operand of `+` and `-` at their depth.
const BOUNDARY_WORDS: [&str; 53] = [
    "and",
    "or",
    "not",
    "when",
    "then",
    "else",
    "case",
    "end",
    "select",
    "where",
    "on",
    "as",
    "is",
    "in",
    "between",
    "like",
    "glob",
    "from",
    "by",
    "set",
    "values",
    "returning",
    "having",
    "limit",
    "offset",
    "distinct",
    "all",
    "regexp",
    "match",
    "escape",
    "union",
    "except",
    "intersect",
    "group",
    "order",
    "over",
    "asc",
    "desc",
    "nulls",
    "collate",
    "partition",
    "rows",
    "range",
    "groups",
    "filter",
    "window",
    "using",
    "join",
    "into",
    "do",
    "conflict",
    "isnull",
    "notnull",
];

fn text<'a>(sql: &'a str, t: &Tok) -> &'a str {
    &sql[t.start..t.end]
}

fn is_boundary(sql: &str, t: &Tok) -> bool {
    match t.kind {
        Tk::Word => BOUNDARY_WORDS.iter().any(|w| text(sql, t).eq_ignore_ascii_case(w)),
        Tk::Punct => matches!(
            text(sql, t),
            "," | ";" | "=" | "==" | "<" | ">" | "<=" | ">=" | "<>" | "!="
        ),
        _ => false,
    }
}

/// Whether the token before a `+` or `-` ends an operand, so the sign is an operator.
fn ends_operand(sql: &str, t: &Tok) -> bool {
    match t.kind {
        Tk::Number | Tk::Str | Tk::Quoted => true,
        Tk::Word => !is_boundary(sql, t),
        Tk::Punct => text(sql, t) == ")",
    }
}

/// The sums of a statement: each a span of tokens at one depth, its terms, and the
/// signs between them.
struct Chain {
    depth: i32,
    terms: Vec<(usize, usize)>,
    subs: Vec<bool>,
}

/// The sums (`+`, `-`) of a statement, or with mult its products (`*`, `/`), whose
/// terms a sum's sign or a `||` ends.
fn chains(sql: &str, toks: &[Tok], mult: bool) -> Vec<Chain> {
    let mut out: Vec<Chain> = Vec::new();
    let signed = |i: usize, ops: &[&str]| {
        let t = &toks[i];
        t.kind == Tk::Punct
            && ops.contains(&text(sql, t))
            && i > 0
            && toks[i - 1].depth >= t.depth
            && ends_operand(sql, &toks[i - 1])
    };
    let ops: &[&str] = if mult { &["*", "/"] } else { &["+", "-"] };
    let binary = |i: usize| signed(i, ops);
    let is_boundary = |sql: &str, i: usize| {
        is_boundary(sql, &toks[i])
            || mult && (signed(i, &["+", "-"]) || matches!(text(sql, &toks[i]), "||" | "%" | "->" | "->>"))
    };
    let mut i = 0;
    while i < toks.len() {
        if !binary(i) {
            i += 1;
            continue;
        }
        let d = toks[i].depth;
        // Back to where the first term starts.
        let mut s = i;
        while s > 0 {
            let p = &toks[s - 1];
            if p.depth < d || p.depth == d && is_boundary(sql, s - 1) {
                break;
            }
            s -= 1;
        }
        // On past the last term.
        let mut e = i + 1;
        let mut ops = Vec::new();
        while e < toks.len() {
            let t = &toks[e];
            if t.depth < d || t.depth == d && is_boundary(sql, e) {
                break;
            }
            if t.depth == d && binary(e) {
                ops.push(e);
            }
            e += 1;
        }
        ops.insert(0, i);
        ops.dedup();
        let mut terms = Vec::new();
        let mut subs = Vec::new();
        let mut from = s;
        for &op in &ops {
            terms.push((from, op));
            subs.push(matches!(text(sql, &toks[op]), "-" | "/"));
            from = op + 1;
        }
        terms.push((from, e));
        if terms.iter().all(|(a, b)| a < b) {
            out.push(Chain { depth: d, terms, subs });
        }
        i = e;
    }
    out
}

/// The declared type of a name in a write with no FROM of its own: a column of the
/// table of an UPDATE, or one the rewrite's `WITH _written(...)` of an INSERT ... SELECT
/// gives.
fn write_target_decl(q: &Connection, sql: &str, name: &str) -> Option<String> {
    let low = sql.to_ascii_lowercase();
    let probe = if let Some(w) = low.find("with _written(") {
        let body = w + low[w..].find(" as (")? + " as ".len();
        let code = blank_literals(&sql[body..]);
        let mut depth = 0;
        let end = code.bytes().position(|c| {
            match c {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            depth == 0
        })?;
        [&sql[w..=body + end], " SELECT ", name, " FROM _written"].concat()
    } else if low.trim_start().starts_with("update") {
        let rest = sql.trim_start()["update".len()..].trim_start();
        let rest = rest
            .strip_prefix("ONLY ")
            .or_else(|| rest.strip_prefix("only "))
            .unwrap_or(rest);
        let end = rest.find(|c: char| c.is_whitespace())?;
        ["SELECT ", name, " FROM ", &rest[..end]].concat()
    } else {
        return None;
    };
    let mut script = crate::sqlite::Script::new(q, &probe);
    let stmt = script.next().ok()??;
    let (_, decls) = stmt.column_info();
    decls.into_iter().next().filter(|d| !d.is_empty())
}

/// What a call written by the rewrite or by this pass gives, read from its text.
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

/// The kind of a term, None for one the statement does not type.
struct Typer<'a> {
    q: &'a Connection,
    probes: HashMap<String, Option<Kind>>,
    /// The schema_version the column cache is read at, once a statement.
    version: Option<i64>,
}

impl Typer<'_> {
    fn kind(&mut self, sql: &str, toks: &[Tok], (a, b): (usize, usize)) -> Option<Kind> {
        let span = &sql[toks[a].start..toks[b - 1].end];
        if b - a == 1 {
            let t = &toks[a];
            return match t.kind {
                Tk::Number if span.contains('.') => Some(Kind::Decimal),
                Tk::Number => Some(Kind::Number),
                Tk::Word if span.starts_with('$') => None,
                Tk::Word | Tk::Quoted => match span.to_ascii_lowercase().as_str() {
                    "current_date" => Some(Kind::Date),
                    "current_timestamp" => Some(Kind::TimestampTz),
                    "current_time" => Some(Kind::Time),
                    "null" => None,
                    _ => self.column(sql, t.start, span),
                },
                _ => None,
            };
        }
        // A name of parts: t.c, "t"."c".
        let name = toks[a..b].iter().enumerate().all(|(k, t)| {
            if k % 2 == 1 {
                text(sql, t) == "."
            } else {
                matches!(t.kind, Tk::Word | Tk::Quoted)
            }
        });
        if name {
            return self.column(sql, toks[a].start, span);
        }
        // CASE ... END: a numeric when a branch is one, as Postgres' common type is.
        if text(sql, &toks[a]).eq_ignore_ascii_case("case") && text(sql, &toks[b - 1]).eq_ignore_ascii_case("end") {
            let d = toks[a].depth;
            let mut nested = 0;
            let mut from = None;
            let mut found = None;
            for k in a + 1..b {
                let t = &toks[k];
                if t.depth != d || t.kind != Tk::Word {
                    continue;
                }
                let w = text(sql, t).to_ascii_lowercase();
                match w.as_str() {
                    "case" => nested += 1,
                    "end" if nested > 0 => nested -= 1,
                    "then" | "else" if nested == 0 => {
                        if let Some(f) = from.take() {
                            found = found.or(self.kind(sql, toks, (f, k)).filter(|k| *k == Kind::Decimal));
                        }
                        from = Some(k + 1);
                    }
                    "when" | "end" if nested == 0 => {
                        if let Some(f) = from.take()
                            && f < k
                        {
                            found = found.or(self.kind(sql, toks, (f, k)).filter(|k| *k == Kind::Decimal));
                        }
                    }
                    _ => {}
                }
            }
            return found;
        }
        // ( SELECT item ... ): the kind of the item.
        if text(sql, &toks[a]) == "("
            && text(sql, &toks[b - 1]) == ")"
            && toks[b - 1].depth == toks[a].depth
            && b - a > 3
            && text(sql, &toks[a + 1]).eq_ignore_ascii_case("select")
        {
            let d = toks[a].depth + 1;
            let mut from = a + 2;
            if text(sql, &toks[from]).eq_ignore_ascii_case("distinct")
                || text(sql, &toks[from]).eq_ignore_ascii_case("all")
            {
                from += 1;
            }
            let end = (from..b - 1)
                .find(|&k| {
                    toks[k].depth == d && {
                        let w = text(sql, &toks[k]);
                        w == ","
                            || ["from", "where", "group", "order", "limit", "as", "union"]
                                .iter()
                                .any(|x| w.eq_ignore_ascii_case(x))
                    }
                })
                .unwrap_or(b - 1);
            return (from < end).then(|| self.kind(sql, toks, (from, end))).flatten();
        }
        // ( term )
        if text(sql, &toks[a]) == "(" && text(sql, &toks[b - 1]) == ")" && toks[b - 1].depth == toks[a].depth {
            let inner_closed = toks[a + 1..b - 1].iter().all(|t| t.depth > toks[a].depth);
            if inner_closed && b - a > 2 {
                return self.kind(sql, toks, (a + 1, b - 1));
            }
        }
        call_kind(span).or_else(|| self.passing(sql, span))
    }

    fn column(&mut self, sql: &str, pos: usize, name: &str) -> Option<Kind> {
        if let Some(k) = self.probes.get(name) {
            return *k;
        }
        let decl = column_decl_cached(self.q, sql, pos, name, &mut self.version)
            .or_else(|| write_target_decl(self.q, sql, name));
        let k = decl.and_then(|d| Kind::of_decl(&d));
        self.probes.insert(name.to_string(), k);
        k
    }

    /// The kind of a call whose value is one of its arguments' or follows from it:
    /// MIN, MAX, COALESCE, a window's LAST_VALUE..., DATE_TRUNC, SUM and AVG of
    /// intervals.
    fn passing(&mut self, sql: &str, item: &str) -> Option<Kind> {
        let t = item.trim();
        let open = t.find('(')?;
        let name = t[..open].trim().to_ascii_lowercase();
        let close = matching(t, open)?;
        // Past the call, an aggregate's FILTER (...) and a window's OVER (...) only.
        let rest = t[close + 1..].trim().to_ascii_lowercase();
        if !rest.is_empty() && !rest.starts_with("over") && !rest.starts_with("filter") {
            return None;
        }
        let args = &t[open + 1..close];
        let first = first_arg(args);
        let at = sql.find(t).unwrap_or(0) + open + 1;
        let arg_kind = |me: &mut Self, arg: &str| {
            let toks = tokens(arg);
            if toks.is_empty() {
                return None;
            }
            let shifted: Vec<Tok> = toks
                .iter()
                .map(|x| Tok {
                    start: x.start + at,
                    end: x.end + at,
                    ..x.clone()
                })
                .collect();
            if sql.get(at..at + arg.len()) == Some(arg) {
                me.kind(sql, &shifted, (0, shifted.len()))
            } else {
                None
            }
        };
        match name.as_str() {
            // The first argument the statement types: COALESCE(MIN(x) FILTER (...), '...'::time).
            "coalesce" | "ifnull" | "greatest" | "least" => {
                let mut rest = args;
                loop {
                    let arg = first_arg(rest);
                    if arg.trim().is_empty() {
                        return None;
                    }
                    let offset = args.len() - rest.len();
                    let at_arg = at + offset;
                    let toks: Vec<Tok> = tokens(arg)
                        .into_iter()
                        .map(|x| Tok {
                            start: x.start + at_arg,
                            end: x.end + at_arg,
                            ..x
                        })
                        .collect();
                    if !toks.is_empty()
                        && sql.get(at_arg..at_arg + arg.len()) == Some(arg)
                        && let Some(k) = self.kind(sql, &toks, (0, toks.len()))
                    {
                        return Some(k);
                    }
                    if arg.len() >= rest.len() {
                        return None;
                    }
                    rest = &rest[arg.len() + 1..];
                }
            }
            "min" | "max" | "nullif" | "first_value" | "last_value" | "nth_value" | "lag" | "lead" | "any_value" => {
                arg_kind(self, first)
            }
            "sum" | "avg" => arg_kind(self, first).filter(|k| matches!(k, Kind::Interval | Kind::Decimal)),
            "abs" | "round" | "ceil" | "ceiling" | "floor" | "trunc" => {
                arg_kind(self, first).filter(|k| *k == Kind::Decimal)
            }
            "date_trunc" => {
                let second = args[first.len()..].trim_start_matches(',');
                let lead = args.len() - second.len();
                let at2 = at + lead + (second.len() - second.trim_start().len());
                let toks: Vec<Tok> = tokens(second.trim())
                    .into_iter()
                    .map(|x| Tok {
                        start: x.start + at2,
                        end: x.end + at2,
                        ..x
                    })
                    .collect();
                if toks.is_empty() || sql.get(at2..at2 + second.trim().len()) != Some(second.trim()) {
                    return None;
                }
                match self.kind(sql, &toks, (0, toks.len()))? {
                    Kind::Date | Kind::TimestampTz => Some(Kind::TimestampTz),
                    k @ (Kind::Timestamp | Kind::Interval) => Some(k),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn matching(s: &str, open: usize) -> Option<usize> {
    let code = blank_literals(s);
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

fn first_arg(args: &str) -> &str {
    let code = blank_literals(args);
    let mut depth = 0;
    for (i, c) in code.bytes().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => return &args[..i],
            _ => {}
        }
    }
    args
}

/// The declared type the protocol reads for a sum the pass wrote.
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

fn kind_name(k: Option<Kind>) -> &'static str {
    k.map_or("", Kind::name)
}

/// Writes each sum with a side of a date or time type as the call that computes it, and
/// casts the select-list items whose type only Postgres knows.
pub(crate) fn resolve_temporal<'a>(q: &Connection, query: &'a str) -> Cow<'a, str> {
    let has_sign = blank_literals(query)
        .bytes()
        .any(|c| matches!(c, b'+' | b'-' | b'*' | b'/'));
    let low = query.to_ascii_lowercase();
    let typed_items = low.trim_start().starts_with("select")
        && [
            "min(",
            "max(",
            "coalesce(",
            "greatest(",
            "least(",
            "nullif(",
            "ifnull(",
            "_value(",
            "lag(",
            "lead(",
            "date_trunc(",
            "sum(",
            "avg(",
            "abs(",
            "round(",
            "ceil",
            "floor(",
            "trunc(",
        ]
        .iter()
        .any(|f| low.contains(f));
    let numerics = reads_or_writes_rows(query) && might_hold_numerics(&low);
    // A comparison the statement says a date or an interval may stand in (a cast, a
    // date function, date or interval arithmetic, a column so named), or an ORDER BY,
    // whose key may be an interval column. A point lookup says none.
    let temporals = low.contains("order")
        || (["interval", "date", "overlite_t"].iter().any(|w| low.contains(w))
            && blank_literals(query).bytes().any(|c| matches!(c, b'=' | b'<' | b'>')));
    // A session whose TimeZone is not UTC reads a timestamptz's wall clock in it.
    let zoned = !crate::builtins::dates::session_is_utc();
    let temporals = temporals || (zoned && query.contains('\''));
    if !has_sign && !typed_items && !numerics && !temporals && !zoned {
        return Cow::Borrowed(query);
    }
    let mut typer = Typer {
        q,
        probes: HashMap::new(),
        version: None,
    };
    let mut sql = query.to_string();
    if has_sign {
        // The deepest sum first, so that an outer one reads the call an inner became.
        let mut skip: Vec<String> = Vec::new();
        for _ in 0..64 {
            let toks = tokens(&sql);
            // Products before the sums that hold them, deepest first.
            let mut found: Vec<(bool, Chain)> = chains(&sql, &toks, true).into_iter().map(|c| (true, c)).collect();
            found.extend(chains(&sql, &toks, false).into_iter().map(|c| (false, c)));
            found.sort_by_key(|(mult, c)| (std::cmp::Reverse(c.depth), !*mult));
            let mut changed = false;
            for (mult, chain) in found {
                let span = &sql[toks[chain.terms[0].0].start..toks[chain.terms[chain.terms.len() - 1].1 - 1].end];
                if skip.iter().any(|s| s == span) {
                    continue;
                }
                let kinds: Vec<Option<Kind>> = chain.terms.iter().map(|t| typer.kind(&sql, &toks, *t)).collect();
                let temporal = kinds.iter().any(|k| k.is_some_and(Kind::temporal));
                let decimal = kinds.contains(&Some(Kind::Decimal));
                if !temporal && !decimal {
                    skip.push(span.to_string());
                    continue;
                }
                if !temporal {
                    // A numeric: exact arithmetic, wherever a statement that reads or writes
                    // rows has one. numeric_affinity below makes the decimal text it gives
                    // compare as a number. A table's DDL keeps SQLite's arithmetic, so the
                    // file opens without these functions.
                    let (s, e) = (
                        toks[chain.terms[0].0].start,
                        toks[chain.terms[chain.terms.len() - 1].1 - 1].end,
                    );
                    if !reads_or_writes_rows(&sql) {
                        skip.push(span.to_string());
                        continue;
                    }
                    // A literal with a point is a numeric: its text, not SQLite's double.
                    let term = |t: (usize, usize)| {
                        let x = &sql[toks[t.0].start..toks[t.1 - 1].end];
                        if t.1 - t.0 == 1 && toks[t.0].kind == Tk::Number && !x.bytes().all(|c| c.is_ascii_digit()) {
                            ["'", x, "'"].concat()
                        } else {
                            x.to_string()
                        }
                    };
                    let mut acc = term(chain.terms[0]);
                    for (k, sub) in chain.subs.iter().enumerate() {
                        let op = match (mult, *sub) {
                            (true, true) => "'/'",
                            (true, false) => "'*'",
                            (false, true) => "'-'",
                            (false, false) => "'+'",
                        };
                        acc = [
                            "overlite_numeric_op(",
                            &acc,
                            ", ",
                            &term(chain.terms[k + 1]),
                            ", ",
                            op,
                            ")",
                        ]
                        .concat();
                    }
                    // Cast, so that the select list says it is a numeric.
                    let acc = ["overlite_cast(", &acc, ", 'numeric')"].concat();
                    sql = [&sql[..s], &acc, &sql[e..]].concat();
                    changed = true;
                    break;
                }
                let term = |t: (usize, usize)| sql[toks[t.0].start..toks[t.1 - 1].end].to_string();
                let mut acc = term(chain.terms[0]);
                let mut acc_kind = kinds[0];
                if mult {
                    // An interval times or over a number; any other product of a date or
                    // time type is refused by overlite_tmul, as Postgres has no operator.
                    for (k, div) in chain.subs.iter().enumerate() {
                        let next_kind = kinds[k + 1];
                        acc = [
                            "overlite_tmul(",
                            &acc,
                            ", ",
                            &term(chain.terms[k + 1]),
                            ", '",
                            kind_name(acc_kind),
                            "', '",
                            kind_name(next_kind),
                            "', '",
                            if *div { "/" } else { "*" },
                            "')",
                        ]
                        .concat();
                        acc_kind = Some(Kind::Interval);
                    }
                    let (s, e) = (
                        toks[chain.terms[0].0].start,
                        toks[chain.terms[chain.terms.len() - 1].1 - 1].end,
                    );
                    sql = [&sql[..s], &acc, &sql[e..]].concat();
                    changed = true;
                    break;
                }
                for (k, sub) in chain.subs.iter().enumerate() {
                    let f = if *sub { "overlite_tsub(" } else { "overlite_tadd(" };
                    let next_kind = kinds[k + 1];
                    acc = [
                        f,
                        &acc,
                        ", ",
                        &term(chain.terms[k + 1]),
                        ", '",
                        kind_name(acc_kind),
                        "', '",
                        kind_name(next_kind),
                        "')",
                    ]
                    .concat();
                    acc_kind = match (acc_kind, next_kind) {
                        (Some(a), Some(b)) => result_kind(*sub, a, b),
                        _ => None,
                    };
                }
                let (s, e) = (
                    toks[chain.terms[0].0].start,
                    toks[chain.terms[chain.terms.len() - 1].1 - 1].end,
                );
                sql = [&sql[..s], &acc, &sql[e..]].concat();
                changed = true;
                break;
            }
            if !changed {
                break;
            }
        }
    }
    if typed_items {
        sql = cast_items(&mut typer, &sql);
    }
    if numerics || (has_sign && reads_or_writes_rows(&sql)) {
        sql = numeric_affinity(&mut typer, &sql);
    }
    if temporals && reads_or_writes_rows(&sql) {
        sql = typed_comparisons(&mut typer, &sql);
    }
    if zoned && reads_or_writes_rows(&sql) {
        sql = session_zone_reads(&mut typer, &sql);
    }
    if sql == query {
        Cow::Borrowed(query)
    } else {
        Cow::Owned(sql)
    }
}

/// Comparisons whose sides SQLite would compare as the wrong text: an interval with an
/// interval, which overlite_interval_cmp compares by span (`1 mon` is past `29 days`),
/// and a date with a timestamp, whose date is made the timestamp at its midnight.
fn typed_comparisons(typer: &mut Typer<'_>, sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut done: Vec<usize> = Vec::new();
    for _ in 0..64 {
        let toks = tokens(&sql);
        let is_cmp = |t: &Tok| {
            t.kind == Tk::Punct && matches!(text(&sql, t), "=" | "==" | "<>" | "!=" | "<" | "<=" | ">" | ">=")
        };
        let ends = |k: usize, d: i32| {
            let t = &toks[k];
            t.depth < d || (t.depth == d && (is_boundary(&sql, t) || is_cmp(t) || text(&sql, t) == ";"))
        };
        let mut changed = false;
        for c in 0..toks.len() {
            if !is_cmp(&toks[c]) || done.contains(&toks[c].start) {
                continue;
            }
            let d = toks[c].depth;
            let mut a = c;
            while a > 0 && !ends(a - 1, d) {
                a -= 1;
            }
            let mut b = c + 1;
            while b < toks.len() && !ends(b, d) {
                b += 1;
            }
            if a == c || b == c + 1 || (text(&sql, &toks[c]) == "=" && is_assignment(&sql, &toks, a)) {
                done.push(toks[c].start);
                continue;
            }
            let (kl, kr) = match (
                cheap_kind(typer, &sql, &toks, (a, c)),
                cheap_kind(typer, &sql, &toks, (c + 1, b)),
            ) {
                // A parameter, a number or NULL beside a column: no rule here takes it.
                (Cheap::Column, Cheap::Other) | (Cheap::Other, Cheap::Column) => {
                    done.push(toks[c].start);
                    continue;
                }
                (Cheap::Column, Cheap::Known(k)) if !k.temporal() => {
                    done.push(toks[c].start);
                    continue;
                }
                (Cheap::Known(k), Cheap::Column) if !k.temporal() => {
                    done.push(toks[c].start);
                    continue;
                }
                (Cheap::Known(x), Cheap::Known(y)) => (Some(x), Some(y)),
                (x, y) => {
                    let probe = |cheap: Cheap, t: &mut Typer<'_>, span: (usize, usize)| match cheap {
                        Cheap::Known(k) => Some(k),
                        Cheap::Other => None,
                        Cheap::Column => t.kind(&sql, &toks, span),
                    };
                    (probe(x, typer, (a, c)), probe(y, typer, (c + 1, b)))
                }
            };
            let left = &sql[toks[a].start..toks[c - 1].end];
            let right = &sql[toks[c + 1].start..toks[b - 1].end];
            let op = text(&sql, &toks[c]);
            let number = |k: Option<Kind>| matches!(k, Some(Kind::Number | Kind::Decimal));
            let stamp = |k: Option<Kind>| match k {
                Some(Kind::Timestamp) => Some("timestamp"),
                Some(Kind::TimestampTz) => Some("timestamptz"),
                _ => None,
            };
            let repl = if (kl == Some(Kind::Interval) || kr == Some(Kind::Interval)) && !number(kl) && !number(kr) {
                Some(["overlite_interval_cmp(", left, ", ", right, ") ", op, " 0"].concat())
            } else if let (Some(Kind::Date), Some(ty)) = (kl, stamp(kr)) {
                Some(["overlite_cast(", left, ", '", ty, "') ", op, " ", right].concat())
            } else if let (Some(ty), Some(Kind::Date)) = (stamp(kl), kr) {
                Some([left, " ", op, " overlite_cast(", right, ", '", ty, "')"].concat())
            } else {
                None
            };
            let Some(repl) = repl else {
                done.push(toks[c].start);
                continue;
            };
            let (s, e) = (toks[a].start, toks[b - 1].end);
            sql = [&sql[..s], &repl, &sql[e..]].concat();
            // Positions after the change moved; what was checked before it did not.
            done.retain(|&p| p < s);
            changed = true;
            break;
        }
        if !changed {
            break;
        }
    }
    interval_order(typer, &sql)
}

/// What depends on the session's TimeZone where a timestamptz, stored as bare UTC, is
/// read: CURRENT_DATE, a cast of one to text, date, time or timestamp, and EXTRACT and
/// DATE_TRUNC over one, which Postgres take in the session's zone.
fn session_zone_reads(typer: &mut Typer<'_>, sql: &str) -> String {
    let zone = crate::builtins::dates::session_zone().replace('\'', "''");
    let mut sql = sql.to_string();
    for _ in 0..64 {
        let toks = tokens(&sql);
        let mut repl: Option<(usize, usize, String)> = None;
        for (i, t) in toks.iter().enumerate() {
            let w = text(&sql, t).to_ascii_lowercase();
            if t.kind == Tk::Word && w == "current_date" {
                repl = Some((t.start, t.end, "overlite_current_date()".to_string()));
                break;
            }
            if t.kind != Tk::Word || toks.get(i + 1).is_none_or(|n| text(&sql, n) != "(") {
                continue;
            }
            let Some(close) =
                (i + 2..toks.len()).find(|&k| toks[k].depth == toks[i + 1].depth && text(&sql, &toks[k]) == ")")
            else {
                continue;
            };
            // The argument a call reads a timestamptz in: CAST(x AS type), overlite_cast(x,
            // 'type'), overlite_extract('f', x), DATE_TRUNC('u', x).
            let (arg, target): ((usize, usize), String) = match w.as_str() {
                "cast" => {
                    let Some(as_at) = (i + 2..close).rev().find(|&k| {
                        toks[k].depth == toks[i + 1].depth + 1 && text(&sql, &toks[k]).eq_ignore_ascii_case("as")
                    }) else {
                        continue;
                    };
                    (
                        (i + 2, as_at),
                        sql[toks[as_at].end..toks[close].start].trim().to_ascii_lowercase(),
                    )
                }
                "overlite_cast" | "overlite_assign" => {
                    let Some(comma) = (i + 2..close)
                        .rev()
                        .find(|&k| toks[k].depth == toks[i + 1].depth + 1 && text(&sql, &toks[k]) == ",")
                    else {
                        continue;
                    };
                    (
                        (i + 2, comma),
                        sql[toks[comma].end..toks[close].start]
                            .trim()
                            .trim_matches('\'')
                            .to_ascii_lowercase(),
                    )
                }
                // COALESCE, GREATEST and LEAST of a timestamptz and a timestamp or a date:
                // Postgres makes the others timestamptz, read in the session's zone.
                "coalesce" | "ifnull" | "greatest" | "least" | "dec_greatest" | "dec_least" => {
                    let d = toks[i + 1].depth + 1;
                    let mut bounds = Vec::new();
                    let mut from = i + 2;
                    for (k, tk) in toks.iter().enumerate().take(close + 1).skip(i + 2) {
                        if k == close || (tk.depth == d && text(&sql, tk) == ",") {
                            bounds.push((from, k));
                            from = k + 1;
                        }
                    }
                    let kinds: Vec<Option<Kind>> = bounds
                        .iter()
                        .map(|&(a, b)| if a < b { typer.kind(&sql, &toks, (a, b)) } else { None })
                        .collect();
                    if !kinds.contains(&Some(Kind::TimestampTz)) {
                        continue;
                    }
                    let Some(at) = kinds
                        .iter()
                        .position(|k| matches!(k, Some(Kind::Timestamp | Kind::Date)))
                    else {
                        continue;
                    };
                    let (a, b) = bounds[at];
                    let x = &sql[toks[a].start..toks[b - 1].end];
                    repl = Some((toks[a].start, toks[b - 1].end, ["overlite_tz_in(", x, ")"].concat()));
                    break;
                }
                // A tstzrange's bounds a client gives are read in the session's zone too.
                "tstzrange" => {
                    let d = toks[i + 1].depth + 1;
                    let commas: Vec<usize> = (i + 2..close)
                        .filter(|&k| toks[k].depth == d && text(&sql, &toks[k]) == ",")
                        .collect();
                    let bounds = [
                        (i + 2, *commas.first().unwrap_or(&close)),
                        (
                            commas.first().map_or(close, |c| c + 1),
                            *commas.get(1).unwrap_or(&close),
                        ),
                    ];
                    let Some(&(a, b)) = bounds.iter().find(|&&(a, b)| {
                        a < b
                            && !text(&sql, &toks[a]).eq_ignore_ascii_case("null")
                            && typer.kind(&sql, &toks, (a, b)) != Some(Kind::TimestampTz)
                    }) else {
                        continue;
                    };
                    let x = &sql[toks[a].start..toks[b - 1].end];
                    repl = Some((toks[a].start, toks[b - 1].end, ["overlite_tz_in(", x, ")"].concat()));
                    break;
                }
                "overlite_extract" | "date_trunc" => {
                    let Some(comma) =
                        (i + 2..close).find(|&k| toks[k].depth == toks[i + 1].depth + 1 && text(&sql, &toks[k]) == ",")
                    else {
                        continue;
                    };
                    ((comma + 1, close), w.clone())
                }
                _ => continue,
            };
            if arg.0 >= arg.1 {
                continue;
            }
            let arg_kind = typer.kind(&sql, &toks, arg);
            let x = &sql[toks[arg.0].start..toks[arg.1 - 1].end];
            // What a client gives as a timestamptz, a literal, a parameter or a date, is
            // read in the session's zone; a timestamptz already is one.
            if (w == "overlite_cast" || w == "overlite_assign")
                && (target == "timestamptz"
                    || target.starts_with("timestamptz(")
                    || target.starts_with("timestamp with time zone"))
            {
                if arg_kind == Some(Kind::TimestampTz) {
                    continue;
                }
                let new = [
                    &sql[t.start..toks[arg.0].start],
                    "overlite_tz_in(",
                    x,
                    ")",
                    &sql[toks[arg.1 - 1].end..toks[close].end],
                ]
                .concat();
                repl = Some((t.start, toks[close].end, new));
                break;
            }
            // DATE_TRUNC of a date is a timestamptz at the zone's midnight.
            if w == "date_trunc"
                && arg_kind == Some(Kind::Date)
                && !(i >= 2 && text(&sql, &toks[i - 2]).eq_ignore_ascii_case("overlite_tz_in"))
            {
                let call = &sql[t.start..toks[close].end];
                repl = Some((t.start, toks[close].end, ["overlite_tz_in(", call, ")"].concat()));
                break;
            }
            if arg_kind != Some(Kind::TimestampTz) {
                continue;
            }
            let wall = ["overlite_column_at_time_zone(", x, ", '", &zone, "')"].concat();
            let new = match target.as_str() {
                "text" | "varchar" | "character varying" => ["overlite_tz_text(", x, ")"].concat(),
                "date" | "time" | "timestamp" | "time without time zone" | "timestamp without time zone"
                    if w != "overlite_assign" =>
                {
                    ["overlite_cast(", &wall, ", '", &target, "')"].concat()
                }
                "overlite_extract" | "date_trunc" => String::new(),
                // Stored as bare UTC, a timestamptz written on (a copy, a cast to text or
                // to a timestamp) goes with its offset, which no zone can misread.
                _ if w == "overlite_cast" || w == "overlite_assign" => [
                    &sql[t.start..toks[arg.0].start],
                    "overlite_tz_text(",
                    x,
                    ")",
                    &sql[toks[arg.1 - 1].end..toks[close].end],
                ]
                .concat(),
                _ => continue,
            };
            let new = match target.as_str() {
                _ if !new.is_empty() => new,
                "overlite_extract" => [&sql[t.start..toks[arg.0].start], &wall, ")"].concat(),
                // The zone's wall clock truncated, read back as the instant it is there,
                // in the form a stored timestamptz has.
                "date_trunc" => ["overlite_tz_in(", &sql[t.start..toks[arg.0].start], &wall, "))"].concat(),
                _ => continue,
            };
            repl = Some((t.start, toks[close].end, new));
            break;
        }
        // A literal compared with a timestamptz is one, read in the session's zone.
        if repl.is_none() {
            repl = zoned_literal(typer, &sql, &toks);
        }
        let Some((s, e, new)) = repl else {
            break;
        };
        sql = [&sql[..s], &new, &sql[e..]].concat();
    }
    sql
}

/// The first string literal compared with a timestamptz, as the timestamptz it reads as.
fn zoned_literal(typer: &mut Typer<'_>, sql: &str, toks: &[Tok]) -> Option<(usize, usize, String)> {
    for (c, t) in toks.iter().enumerate() {
        if t.kind != Tk::Punct || !matches!(text(sql, t), "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=") {
            continue;
        }
        let (Some(l), Some(r)) = (c.checked_sub(1), toks.get(c + 1)) else {
            continue;
        };
        let (lit, other) = if toks[l].kind == Tk::Str {
            (l, (c + 1, c + 2))
        } else if r.kind == Tk::Str {
            (c + 1, (l, c))
        } else {
            continue;
        };
        if toks
            .get(c + 2)
            .is_some_and(|n| text(sql, n) == "(" || text(sql, n) == "::")
        {
            continue;
        }
        if typer.kind(sql, toks, other) == Some(Kind::TimestampTz) {
            let lit_text = text(sql, &toks[lit]);
            return Some((
                toks[lit].start,
                toks[lit].end,
                ["overlite_tz_in(", lit_text, ")"].concat(),
            ));
        }
    }
    None
}

/// What a side of a comparison is without a look at the schema: a bare column, a kind
/// the statement gives (a cast, a call, a literal an interval reads), or something no
/// rule takes (a parameter, a number, NULL).
#[derive(Clone, Copy)]
enum Cheap {
    Column,
    Known(Kind),
    Other,
}

fn cheap_kind(typer: &mut Typer<'_>, sql: &str, toks: &[Tok], (a, b): (usize, usize)) -> Cheap {
    let name = toks[a..b].iter().enumerate().all(|(k, t)| {
        if k % 2 == 1 {
            text(sql, t) == "."
        } else {
            matches!(t.kind, Tk::Word | Tk::Quoted)
        }
    });
    let word = text(sql, &toks[a]);
    if name
        && !word.starts_with('$')
        && !word.eq_ignore_ascii_case("null")
        && !word.eq_ignore_ascii_case("current_date")
        && !word.eq_ignore_ascii_case("current_timestamp")
    {
        return Cheap::Column;
    }
    if b - a == 1 && toks[a].kind == Tk::Str {
        let inner = word.trim_matches('\'');
        let date_like = inner.len() >= 10 && inner.as_bytes()[4] == b'-';
        // `29 days`, `1 day 02:00`: a unit or a clock, not a bare number.
        let worded = inner.bytes().any(|c| c.is_ascii_alphabetic() || c == b':');
        return if worded && !date_like && crate::builtins::casts::parse_interval(inner).is_some() {
            Cheap::Known(Kind::Interval)
        } else {
            Cheap::Other
        };
    }
    if b - a == 1 && matches!(toks[a].kind, Tk::Number) || word.starts_with('$') || word == "?" {
        return Cheap::Other;
    }
    typer.kind(sql, toks, (a, b)).map_or(Cheap::Other, Cheap::Known)
}

/// Whether the `=` after the operand starting at a assigns, in an UPDATE's SET list:
/// SET comes before it at its depth, with no clause between.
fn is_assignment(sql: &str, toks: &[Tok], a: usize) -> bool {
    let d = toks[a].depth;
    for t in toks[..a].iter().rev() {
        if t.depth < d {
            return false;
        }
        if t.depth > d || t.kind != Tk::Word {
            continue;
        }
        let w = text(sql, t).to_ascii_lowercase();
        if w == "set" {
            return true;
        }
        if [
            "where",
            "when",
            "on",
            "having",
            "and",
            "or",
            "select",
            "from",
            "returning",
            "then",
            "else",
        ]
        .contains(&w.as_str())
        {
            return false;
        }
    }
    false
}

/// Each ORDER BY key that is an interval, ordered by its span: COLLATE INTERVAL.
fn interval_order(typer: &mut Typer<'_>, sql: &str) -> String {
    let toks = tokens(sql);
    let mut inserts: Vec<usize> = Vec::new();
    for k in 0..toks.len().saturating_sub(1) {
        if !(text(sql, &toks[k]).eq_ignore_ascii_case("order") && text(sql, &toks[k + 1]).eq_ignore_ascii_case("by")) {
            continue;
        }
        let d = toks[k].depth;
        let mut a = k + 2;
        while a < toks.len() {
            let stop = |t: &Tok| {
                t.depth < d
                    || (t.depth == d
                        && (text(sql, t) == ","
                            || [
                                "asc",
                                "desc",
                                "nulls",
                                "collate",
                                "limit",
                                "offset",
                                "union",
                                "except",
                                "intersect",
                                "for",
                                "returning",
                            ]
                            .iter()
                            .any(|w| text(sql, t).eq_ignore_ascii_case(w))
                            || text(sql, t) == ")"
                            || text(sql, t) == ";"))
            };
            let mut b = a;
            while b < toks.len() && !stop(&toks[b]) {
                b += 1;
            }
            let collated = toks
                .get(b)
                .is_some_and(|t| text(sql, t).eq_ignore_ascii_case("collate"));
            if b > a && !collated && typer.kind(sql, &toks, (a, b)) == Some(Kind::Interval) {
                inserts.push(toks[b - 1].end);
            }
            // On to the next key, past its direction.
            while b < toks.len() && toks[b].depth >= d && !(toks[b].depth == d && text(sql, &toks[b]) == ",") {
                if toks[b].depth == d
                    && [
                        "limit",
                        "offset",
                        "union",
                        "except",
                        "intersect",
                        "for",
                        "returning",
                        ")",
                        ";",
                    ]
                    .iter()
                    .any(|w| text(sql, &toks[b]).eq_ignore_ascii_case(w))
                {
                    break;
                }
                b += 1;
            }
            if toks.get(b).is_none_or(|t| text(sql, t) != "," || t.depth != d) {
                break;
            }
            a = b + 1;
        }
    }
    if inserts.is_empty() {
        return sql.to_string();
    }
    let mut out = sql.to_string();
    for at in inserts.into_iter().rev() {
        out.insert_str(at, " COLLATE INTERVAL");
    }
    out
}

/// The type a cast to a kind names.
fn cast_name(k: Kind) -> Option<&'static str> {
    Some(match k {
        Kind::Date => "date",
        Kind::Time => "time",
        Kind::Timestamp => "timestamp",
        Kind::TimestampTz => "timestamptz",
        Kind::Interval => "interval",
        Kind::Decimal => "numeric",
        Kind::Number => return None,
    })
}

/// Casts each select-list item of a date or time type SQLite has no declared type for.
fn cast_items(typer: &mut Typer<'_>, sql: &str) -> String {
    let Some((start, end)) = select_list(sql) else {
        return sql.to_string();
    };
    let list = &sql[start..end];
    let code = blank_literals(list);
    let b = code.as_bytes();
    let mut items = Vec::new();
    let (mut depth, mut from) = (0, 0);
    for (i, c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                items.push((from, i));
                from = i + 1;
            }
            _ => {}
        }
    }
    items.push((from, b.len()));
    let mut out = String::with_capacity(sql.len() + 32);
    out.push_str(&sql[..start]);
    let mut at = start;
    for (a, e) in items {
        // The first item, past a DISTINCT or ALL.
        let mut a = a;
        if a == 0 {
            let head = code[..e].trim_start().to_ascii_lowercase();
            for word in ["distinct ", "all "] {
                if head.starts_with(word) {
                    a = code.len() - code.trim_start().len() + word.len();
                }
            }
        }
        let raw = &list[a..e];
        let lead = raw.len() - raw.trim_start().len();
        let item_code = code[a..e].trim();
        // The expression, before its label.
        let expr_len = item_code
            .to_ascii_lowercase()
            .rfind(" as ")
            .filter(|p| item_code[..*p].matches('(').count() == item_code[..*p].matches(')').count())
            .unwrap_or(item_code.len());
        let expr = &raw.trim_start()[..expr_len];
        let s = start + a + lead;
        let toks: Vec<Tok> = tokens(expr)
            .into_iter()
            .map(|t| Tok {
                start: t.start + s,
                end: t.end + s,
                ..t
            })
            .collect();
        if toks.len() < 3 || call_kind(expr).is_some() {
            continue;
        }
        // A scalar subquery has the type of what it selects.
        let mut kind = typer.passing(sql, expr);
        if kind.is_none() && text(sql, &toks[0]) == "(" {
            kind = typer.kind(sql, &toks, (0, toks.len()));
        }
        let Some(name) = kind.and_then(cast_name) else {
            continue;
        };
        out.push_str(&sql[at..s]);
        out.push_str("overlite_cast(");
        // ABS and ROUND of a numeric, exact: SQLite's, kept elsewhere, give a double.
        let low = expr.to_ascii_lowercase();
        match ["abs(", "round("].iter().find(|f| low.starts_with(**f)) {
            Some(f) if name == "numeric" => {
                out.push_str("overlite_");
                out.push_str(&f[..f.len() - 1]);
                out.push_str("_exact");
                out.push_str(&expr[f.len() - 1..]);
            }
            _ => out.push_str(expr),
        }
        out.push_str(", '");
        out.push_str(name);
        out.push_str("')");
        if expr_len == item_code.len() {
            // SQLite's name for the column was the expression's text.
            out.push_str(" AS \"");
            out.push_str(&expr.replace('"', "\"\""));
            out.push('"');
        }
        at = s + expr.len();
    }
    out.push_str(&sql[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums() {
        let q = Connection::open_in_memory().unwrap();
        q.execute_batch("CREATE TABLE t (s timestamp, d interval, n int); CREATE TABLE ra (d date)")
            .unwrap();
        for (sql, want) in [
            (
                "SELECT datetime('now') + overlite_cast('2 days', 'interval') > datetime('now')",
                "SELECT overlite_tadd(datetime('now'), overlite_cast('2 days', 'interval'), 'TIMESTAMPTZ', 'INTERVAL') > datetime('now')",
            ),
            (
                r#"SELECT DISTINCT DATE_TRUNC('year', "ra"."d") AS "datefield" FROM "ra" ORDER BY 1 ASC"#,
                r#"SELECT DISTINCT overlite_cast(DATE_TRUNC('year', "ra"."d"), 'timestamptz') AS "datefield" FROM "ra" ORDER BY 1 ASC"#,
            ),
            (
                "SELECT DISTINCT max(s) FROM t",
                "SELECT DISTINCT overlite_cast(max(s), 'timestamp') AS \"max(s)\" FROM t",
            ),
            (
                "SELECT s + d - n, n + 1 FROM t",
                "SELECT overlite_tsub(overlite_tadd(s, d, 'TIMESTAMP', 'INTERVAL'), n, 'TIMESTAMP', 'NUMBER'), n + 1 FROM t",
            ),
            ("UPDATE t SET n = n + 1", "UPDATE t SET n = n + 1"),
        ] {
            assert_eq!(resolve_temporal(&q, sql), want);
        }
    }
}
