//! Set-returning functions. In a select list, Postgres runs `SELECT unnest(a),
//! unnest(b) FROM t` as one row per element, the calls stepping through their arrays
//! together and the shorter padded with NULLs. SQLite cannot return a set from a
//! select list, so the set moves into FROM, and each call reads a column of it. In FROM
//! a call takes the columns Postgres gives it, and it may read a table to its left.
//!
//! Each function is a scalar of the engine that builds a JSON array, walked by the
//! table `overlite_elements` or, for what has keys, `overlite_pairs`: see
//! builtins/srf.rs. Those are lateral. A subquery is not, so one is written only where
//! nothing is read from the left, for a star to give the right names.

use crate::auth::rls::top_level_keyword;
use crate::ddl::sequences::{read_paren_args, split_top_level};
use crate::rewrite::catalog::index_word_call;
use crate::rewrite::splice;
use crate::rewrite::strings::end_of_string_literal;
use crate::rewrite::table_funcs::{FromAlias, read_from_alias};
use crate::rewrite::tokens::{CodeTok, CodeTokKind, code_tokens};
use overlite_core::text::{quote_ident, quote_literal, trim_end_chars, trim_start_chars};
use overlite_engine::code_only;
use std::fmt::Write;

/// The functions, longest first where one starts another.
static SRF_NAMES: [&str; 14] = [
    "unnest",
    "generate_subscripts",
    "generate_series",
    "json_each_text",
    "jsonb_each_text",
    "json_each",
    "jsonb_each",
    "json_array_elements_text",
    "jsonb_array_elements_text",
    "json_array_elements",
    "jsonb_array_elements",
    "json_object_keys",
    "jsonb_object_keys",
    "regexp_split_to_table",
];

/// Words that end a FROM clause.
static SRF_CLAUSE_WORDS: [&str; 10] = [
    "where",
    "group",
    "having",
    "window",
    "order",
    "limit",
    "offset",
    "union",
    "except",
    "intersect",
];

/// Finds the next call of a set-returning function.
fn next_srf_call(low: &str, from: usize) -> Option<(usize, &'static str)> {
    SRF_NAMES
        .iter()
        .filter_map(|name| index_word_call(low, name, from).map(|i| (i, *name)))
        .min_by_key(|(i, name)| (*i, usize::MAX - name.len()))
}

/// One call: its name, its span, and its arguments.
#[derive(Debug)]
struct SrfCall {
    name: &'static str,
    start: usize,
    end: usize,
    args: Vec<String>,
}

fn read_call(sql: &str, name: &'static str, start: usize) -> Option<SrfCall> {
    let open = sql[start..].find('(')? + start;
    let (args, end) = read_paren_args(sql, open)?;
    let args = split_top_level(&args).iter().map(|a| a.trim().to_string()).collect();
    Some(SrfCall { name, start, end, args })
}

/// A set as the engine builds it.
#[derive(Debug)]
struct Set {
    /// The scalar that builds the rows.
    source: String,
    /// It has keys: overlite_pairs walks it.
    pairs: bool,
    /// How many columns its rows hold, past one only for a zip of arrays.
    width: usize,
}

fn array_arg(arg: &str) -> String {
    ["overlite_array_json(", arg, ")"].concat()
}

/// The scalar that builds the rows of a call.
fn set_of(call: &SrfCall) -> Set {
    let arg = call.args.first().map_or("NULL", String::as_str);
    let text = i32::from(call.name.ends_with("_text"));
    let sorted = i32::from(call.name.starts_with("jsonb"));
    let all = call.args.join(", ");
    let mut set = Set {
        source: String::new(),
        pairs: false,
        width: 1,
    };
    set.source = match call.name {
        "unnest" if call.args.len() > 1 => {
            set.width = call.args.len();
            let arrays: Vec<String> = call.args.iter().map(|a| array_arg(a)).collect();
            ["overlite_zip(", &arrays.join(", "), ")"].concat()
        }
        "unnest" => array_arg(arg),
        "generate_series" => ["overlite_series(", &all, ")"].concat(),
        "generate_subscripts" => ["overlite_subscripts(", arg, ")"].concat(),
        "regexp_split_to_table" => ["overlite_regexp_split(", &all, ")"].concat(),
        "json_object_keys" | "jsonb_object_keys" => {
            ["overlite_json_keys(", arg, ", ", &quote_literal(call.name), ")"].concat()
        }
        name if name.contains("_each") => {
            set.pairs = true;
            format!("overlite_json_pairs({arg}, {text}, {sorted})")
        }
        _ => format!("overlite_json_elements({arg}, {text})"),
    };
    set
}

/// What walks a set.
fn walker(set: &Set) -> String {
    let table = if set.pairs {
        "overlite_pairs"
    } else {
        "overlite_elements"
    };
    [table, "(", &set.source, ")"].concat()
}

/// The text of a column of a set's row under alias, or as itself without one.
fn column(set: &Set, alias: &str, i: usize) -> String {
    let dot = |c: &str| {
        if alias.is_empty() {
            c.to_string()
        } else {
            [alias, ".", c].concat()
        }
    };
    if set.pairs {
        return dot(if i == 0 { "key" } else { "value" });
    }
    if set.width > 1 {
        return format!("json_extract({}, '$[{i}]')", dot("value"));
    }
    dot("value")
}

/// The name Postgres gives each of a function's columns, without a column list.
fn default_names(call: &SrfCall, set: &Set, alias: &str) -> Vec<String> {
    if set.pairs {
        return vec!["key".to_string(), "value".to_string()];
    }
    if call.name.contains("array_elements") {
        return vec!["value".to_string()];
    }
    if !alias.is_empty() && set.width == 1 {
        return vec![alias.to_string()];
    }
    vec![call.name.to_string(); set.width]
}

/// Moves what the top-level statement or a subquery calls out of its select list and
/// into FROM, and gives what FROM calls its columns.
pub(crate) fn rewrite_set_returning(sql: &str) -> String {
    let low = code_only(sql).to_ascii_lowercase();
    if !SRF_NAMES.iter().any(|name| low.contains(name)) {
        return sql.to_string();
    }
    let mut sql = sql.to_string();
    let mut n = 0;
    let mut from = 0;
    loop {
        let low = code_only(&sql).to_ascii_lowercase();
        let Some((at, name)) = next_srf_call(&low, from) else {
            return sql;
        };
        let (start, end) = select_scope(&sql, at);
        let scope = &sql[start..end];
        let outcome = rewrite_in_scope(scope, at - start, name, &mut n);
        match outcome {
            Some(sub) => {
                sql = splice(&sql, start, end, &sub);
                from = start;
            }
            None => from = at + name.len(),
        }
    }
}

/// The bounds of the query around pos: the innermost parentheses a SELECT opens, or
/// the whole statement.
fn select_scope(sql: &str, pos: usize) -> (usize, usize) {
    let b = sql.as_bytes();
    let mut opens: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < pos {
        match b[i] {
            b'\'' => i = end_of_string_literal(sql, i) - 1,
            b'(' => opens.push(i),
            b')' => {
                opens.pop();
            }
            _ => {}
        }
        i += 1;
    }
    for &open in opens.iter().rev() {
        let inner = trim_start_chars(&sql[open + 1..], " \t\r\n").to_ascii_lowercase();
        if (inner.starts_with("select") || inner.starts_with("with"))
            && let Some((_, end)) = read_paren_args(sql, open)
        {
            return (open + 1, end - 1);
        }
    }
    (0, b.len())
}

/// The SELECT of scope that pos stands in: the last at its level before pos.
fn select_before(scope: &str, pos: usize) -> Option<usize> {
    let (mut found, mut from) = (None, 0);
    while let Some(at) = top_level_keyword(&scope[from..pos], "select") {
        found = Some(from + at);
        from += at + "select".len();
    }
    found
}

/// Where the FROM clause at from_idx ends, or the statement's last byte.
fn clause_end(scope: &str, after: usize) -> usize {
    let mut end = trim_end_chars(scope, "; \t\r\n").len();
    for kw in SRF_CLAUSE_WORDS {
        if let Some(p) = top_level_keyword(&scope[after..], kw) {
            end = end.min(after + p);
        }
    }
    end
}

fn rewrite_in_scope(scope: &str, pos: usize, name: &'static str, n: &mut usize) -> Option<String> {
    let call = read_call(scope, name, pos)?;
    let sel = select_before(scope, pos)?;
    let after_sel = sel + "select".len();
    if let Some(f) = top_level_keyword(&scope[after_sel..pos], "from") {
        return rewrite_from_call(scope, &call, after_sel + f, n);
    }
    let alias = format!("_srf{n}");
    *n += 1;
    rewrite_select_list(scope, after_sel, &alias)
}

/// Rewrites the select list opening at list: every call in it reads the set FROM has
/// gained.
fn rewrite_select_list(scope: &str, list: usize, alias: &str) -> Option<String> {
    let from_idx = top_level_keyword(&scope[list..], "from").map(|f| list + f);
    let list_end = from_idx.unwrap_or_else(|| clause_end(scope, list));
    let low = code_only(scope).to_ascii_lowercase();
    let mut calls: Vec<SrfCall> = Vec::new();
    let mut at = list;
    while let Some((i, name)) = next_srf_call(&low[..list_end], at) {
        let call = read_call(scope, name, i)?;
        at = call.end;
        // A subquery's call is its own.
        if select_scope(scope, i).0 == 0 {
            calls.push(call);
        }
    }
    if calls.is_empty() {
        return None;
    }
    let sets: Vec<Set> = calls.iter().map(set_of).collect();

    let mut b = String::with_capacity(scope.len() + 64);
    let mut last = 0;
    let zipped = sets.len() > 1;
    for (i, (call, set)) in calls.iter().zip(&sets).enumerate() {
        b.push_str(&scope[last..call.start]);
        if set.pairs || set.width > 1 {
            let what = format!("{} returning a record in a select list", call.name);
            let _ = write!(b, "overlite_unsupported({})", quote_literal(&what));
        } else if zipped {
            let _ = write!(b, "json_extract({alias}.value, '$[{i}]')");
        } else {
            let _ = write!(b, "{alias}.value");
        }
        last = call.end;
    }
    let source = if zipped {
        let parts: Vec<&str> = sets.iter().map(|s| s.source.as_str()).collect();
        format!("overlite_elements(overlite_zip({}))", parts.join(", "))
    } else {
        walker(&sets[0])
    };
    let Some(from_idx) = from_idx else {
        b.push_str(trim_end_chars(&scope[last..list_end], " \t\r\n"));
        let _ = write!(b, " FROM {source} {alias}");
        if list_end < scope.len() {
            b.push(' ');
        }
        b.push_str(&scope[list_end..]);
        return Some(b);
    };
    // The join goes after any line comment closing the FROM clause, not into it.
    let from_end = clause_end(scope, from_idx + "from".len());
    let mut from_text = trim_end_chars(&scope[last..from_end], " \t\r\n").to_string();
    let nl = from_text.rfind('\n').map_or(0, |n| n + 1);
    if from_text[nl..].contains("--") {
        from_text.push('\n');
    }
    b.push_str(&from_text);
    let _ = write!(b, ", {source} {alias}");
    let rest = &scope[from_end..];
    if !rest.is_empty() && !trim_start_chars(rest, " \t\r\n").starts_with(';') {
        b.push(' ');
    }
    b.push_str(rest);
    Some(b)
}

/// Words that are no column, in an argument.
fn is_expression_word(w: &str) -> bool {
    matches!(
        w.to_ascii_lowercase().as_str(),
        "array"
            | "null"
            | "true"
            | "false"
            | "cast"
            | "as"
            | "case"
            | "when"
            | "then"
            | "else"
            | "end"
            | "and"
            | "or"
            | "not"
            | "is"
            | "in"
    )
}

/// Whether an argument reads a column: then the call needs the table on its left.
fn reads_column(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let toks = code_tokens(arg);
        toks.iter().enumerate().any(|(i, t)| {
            let word = &arg[t.span.clone()];
            let is_name = t.kind == CodeTokKind::Word
                && word
                    .as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_');
            let quoted = t.kind == CodeTokKind::Value && word.starts_with('"');
            let call = toks.get(i + 1).is_some_and(|n| n.is_punct(arg, "("));
            let after_cast = i >= 2 && toks[i - 1].is_punct(arg, ":") && toks[i - 2].is_punct(arg, ":");
            (is_name || quoted) && !call && !after_cast && !is_expression_word(word)
        })
    })
}

/// Rewrites a call in the FROM clause at from_idx.
fn rewrite_from_call(scope: &str, call: &SrfCall, from_idx: usize, n: &mut usize) -> Option<String> {
    // generate_series and generate_subscripts in FROM have passes of their own.
    if call.name.starts_with("generate_") {
        return None;
    }
    let low = code_only(scope).to_ascii_lowercase();
    let tail = read_from_alias(scope, &low, call.end);
    let set = set_of(call);
    let before = trim_end_chars(&low[..call.start], " \t\r\n");
    let has_left = call.start > from_idx && !before.ends_with("from");
    let lateral = has_left && reads_column(&call.args);

    let alias = if tail.alias.is_empty() {
        let alias = if lateral {
            format!("_srf{n}")
        } else {
            call.name.to_string()
        };
        *n += 1;
        alias
    } else {
        tail.alias.clone()
    };
    let names = column_names(call, &set, &tail);
    // The ordinality column comes after all of the function's own.
    let ord = tail.ordinality.then(|| {
        tail.cols
            .get(set.width.max(names.len()))
            .cloned()
            .unwrap_or_else(|| "ordinality".to_string())
    });

    if lateral {
        let map = lateral_map(&set, &names, ord.as_deref(), &alias, &tail);
        // A star cannot name the call's columns, which are expressions over the
        // walker's: they join it under their names.
        let star = select_before(scope, call.start).is_some_and(|sel| {
            split_top_level(&scope[sel + "select".len()..from_idx])
                .iter()
                .any(|item| {
                    let item = item.trim();
                    item == "*" || item.ends_with(".*")
                })
        });
        let head = if star && !tail.cols.is_empty() {
            let mut extra = String::new();
            for (name, expr) in map.iter().filter(|(name, _)| *name != alias) {
                let _ = write!(extra, ", {expr} AS {name}");
            }
            let list_end = trim_end_chars(&scope[..from_idx], " \t\r\n").len();
            [&scope[..list_end], &extra, &scope[list_end..call.start]].concat()
        } else {
            scope[..call.start].to_string()
        };
        let head = map_refs(&head, &map, &alias, true);
        let rest = map_refs(&scope[tail.end..], &map, &alias, false);
        return Some(format!("{head}{} AS {alias}{rest}", walker(&set)));
    }

    let mut cols: Vec<String> = names
        .iter()
        .enumerate()
        .map(|(i, name)| format!("{} AS {name}", column(&set, "", i)))
        .collect();
    if let Some(ord) = &ord {
        cols.push(format!("ord AS {ord}"));
    }
    let repl = format!("(SELECT {} FROM {}) AS {alias}", cols.join(", "), walker(&set));
    // A function's own column may go by the alias alone.
    let map: Vec<(String, String)> = if tail.cols.is_empty() && !set.pairs && names.first().is_some_and(|c| *c != alias)
    {
        vec![(alias.clone(), [&alias, ".", &names[0]].concat())]
    } else {
        Vec::new()
    };
    let head = map_refs(&scope[..call.start], &map, &alias, true);
    let rest = map_refs(&scope[tail.end..], &map, &alias, false);
    Some(format!("{head}{repl}{rest}"))
}

fn column_names(call: &SrfCall, set: &Set, tail: &FromAlias) -> Vec<String> {
    let mut names = default_names(call, set, &tail.alias);
    for (name, given) in names.iter_mut().zip(&tail.cols) {
        name.clone_from(given);
    }
    names
}

/// What each name a lateral call's columns go by stands for.
fn lateral_map(set: &Set, names: &[String], ord: Option<&str>, alias: &str, tail: &FromAlias) -> Vec<(String, String)> {
    let mut map: Vec<(String, String)> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (name.clone(), column(set, alias, i)))
        .collect();
    if let Some(ord) = ord {
        map.push((ord.to_string(), [alias, ".ord"].concat()));
    }
    if tail.cols.is_empty() && !set.pairs && set.width == 1 {
        map.push((alias.to_string(), column(set, alias, 0)));
    }
    map
}

/// Whether a token is the name, as written: a bare name folds, a quoted one does not.
fn names(text: &str, name: &str) -> bool {
    let plain = name.trim_matches('"');
    if name.starts_with('"') {
        return text == name;
    }
    text.eq_ignore_ascii_case(plain) || text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) == Some(plain)
}

/// Writes each reference of text to a name of map as what it stands for. Inside a
/// subquery the name may be another's, and it is left alone. A select-list item that is
/// such a reference keeps the name as its label.
fn map_refs(text: &str, map: &[(String, String)], alias: &str, in_select_list: bool) -> String {
    if map.is_empty() {
        return text.to_string();
    }
    let toks = code_tokens(text);
    let mut out = String::with_capacity(text.len() + 16);
    let mut at = 0;
    let mut subquery_depth = None;
    let is_punct = |i: usize, p: &str| toks.get(i).is_some_and(|t: &CodeTok| t.is_punct(text, p));
    for (i, t) in toks.iter().enumerate() {
        if let Some(depth) = subquery_depth {
            if t.depth < depth {
                subquery_depth = None;
            }
            continue;
        }
        if is_punct(i, "(") && toks.get(i + 1).is_some_and(|n| n.is_word(text, "select")) {
            subquery_depth = Some(t.depth + 1);
            continue;
        }
        let word = &text[t.span.clone()];
        let name_token = t.kind == CodeTokKind::Word || (t.kind == CodeTokKind::Value && word.starts_with('"'));
        if !name_token || is_punct(i + 1, "(") || is_punct(i + 1, ".") {
            continue;
        }
        let qualified = i >= 2 && is_punct(i - 1, ".");
        if qualified && !names(&text[toks[i - 2].span.clone()], alias) {
            continue;
        }
        let Some((_, expr)) = map.iter().find(|(name, _)| names(word, name)) else {
            continue;
        };
        if i > 0 && toks[i - 1].is_word(text, "as") {
            continue;
        }
        let start = if qualified {
            toks[i - 2].span.start
        } else {
            t.span.start
        };
        out.push_str(&text[at..start]);
        out.push_str(expr);
        let first = if qualified { i - 2 } else { i };
        let opens_item = first == 0
            || is_punct(first - 1, ",")
            || toks[first - 1].is_word(text, "select")
            || toks[first - 1].is_word(text, "distinct");
        let closes_item = is_punct(i + 1, ",") || toks.get(i + 1).is_none_or(|n| n.is_word(text, "from"));
        if in_select_list && opens_item && closes_item && toks[..first].iter().any(|p| p.is_word(text, "select")) {
            let _ = write!(out, " AS {}", quote_ident(word.trim_matches('"')));
        }
        at = t.span.end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn select_list_calls_read_a_set_in_from() {
        assert_rewrites(
            rewrite_set_returning,
            &[
                (
                    "SELECT unnest(tags) FROM posts",
                    "SELECT _srf0.value FROM posts, overlite_elements(overlite_array_json(tags)) _srf0",
                ),
                (
                    "SELECT unnest(a) AS x, unnest(b) AS y FROM t WHERE k = 1",
                    "SELECT json_extract(_srf0.value, '$[0]') AS x, json_extract(_srf0.value, '$[1]') AS y FROM t, overlite_elements(overlite_zip(overlite_array_json(a), overlite_array_json(b))) _srf0 WHERE k = 1",
                ),
                (
                    "SELECT x FROM (SELECT unnest(a) AS x FROM t ORDER BY 1) s",
                    "SELECT x FROM (SELECT _srf0.value AS x FROM t, overlite_elements(overlite_array_json(a)) _srf0 ORDER BY 1) s",
                ),
                (
                    "WITH r AS (SELECT unnest(k) AS v FROM i -- note\n WHERE NOT d) SELECT v FROM r",
                    "WITH r AS (SELECT _srf0.value AS v FROM i -- note\n, overlite_elements(overlite_array_json(k)) _srf0 WHERE NOT d) SELECT v FROM r",
                ),
                (
                    "SELECT unnest(ARRAY[1,2])",
                    "SELECT _srf0.value FROM overlite_elements(overlite_array_json(ARRAY[1,2])) _srf0",
                ),
                (
                    "SELECT 1 + unnest(a), 2",
                    "SELECT 1 + _srf0.value, 2 FROM overlite_elements(overlite_array_json(a)) _srf0",
                ),
            ],
        );
    }

    #[test]
    fn from_calls_take_the_columns_postgres_gives() {
        assert_rewrites(
            rewrite_set_returning,
            &[
                (
                    "SELECT * FROM unnest(ARRAY[1,2]) AS u(x)",
                    "SELECT * FROM (SELECT value AS x FROM overlite_elements(overlite_array_json(ARRAY[1,2]))) AS u",
                ),
                (
                    "SELECT * FROM unnest($1)",
                    "SELECT * FROM (SELECT value AS unnest FROM overlite_elements(overlite_array_json($1))) AS unnest",
                ),
                (
                    "SELECT id, x FROM t, unnest(t.a) AS u(x) ORDER BY id, x",
                    "SELECT id, u.value AS \"x\" FROM t, overlite_elements(overlite_array_json(t.a)) AS u ORDER BY id, u.value",
                ),
                (
                    "SELECT u.x FROM t JOIN unnest(t.a) AS u(x) ON true",
                    "SELECT u.value AS \"x\" FROM t JOIN overlite_elements(overlite_array_json(t.a)) AS u ON true",
                ),
                (
                    "SELECT k, v FROM o, jsonb_each(o.d) AS e(k, v)",
                    "SELECT e.key AS \"k\", e.value AS \"v\" FROM o, overlite_pairs(overlite_json_pairs(o.d, 0, 1)) AS e",
                ),
            ],
        );
    }

    #[test]
    fn a_column_is_told_from_a_call() {
        assert!(reads_column(&["t.a".to_string()]));
        assert!(reads_column(&["a".to_string()]));
        assert!(reads_column(&["coalesce(a, b)".to_string()]));
        assert!(!reads_column(&["ARRAY[1,2]".to_string()]));
        assert!(!reads_column(&["'{1,2}'::int[]".to_string()]));
        assert!(!reads_column(&["$1".to_string()]));
    }
}
