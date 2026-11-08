//! Set-returning functions in FROM, and catalog table functions with no SQLite form.

use crate::auth::rls::top_level_keyword;
use crate::ddl::sequences::{read_paren_args, split_top_level};
use crate::rewrite::catalog::{index_word_call, is_word_byte, replace_call};
use crate::rewrite::functions::{has_word_at, skip_spaces};
use crate::rewrite::splice;
use crate::rewrite::strings::map_outside_strings;
use overlite_core::lazy_regex;
use overlite_core::text::{replace_all, trim_end_chars};
use overlite_engine::code_only;

// Matches the LATERAL keyword.
lazy_regex!(RE_LATERAL, r"(?i)\blateral\b");

/// Drops the LATERAL keyword: SQLite treats table-valued functions in the FROM
/// clause as implicitly lateral, so `FROM t, LATERAL json_each(t.x)` works once
/// the keyword is removed.
pub(crate) fn rewrite_lateral(sql: &str) -> String {
    if !sql.to_lowercase().contains("lateral") {
        return sql.to_string();
    }
    map_outside_strings(sql, |code| replace_all(&RE_LATERAL, code, " "))
}

/// Turns generate_series(start, stop[, step]) into a recursive-CTE subquery
/// producing a "generate_series" column, so "FROM generate_series(1, 5)" yields
/// real rows. Non-numeric or unparsable forms fall back to an empty relation.
pub(crate) fn rewrite_generate_series(sql: &str) -> String {
    let mut sql = sql.to_string();
    let mut low = code_only(&sql).to_ascii_lowercase();
    let mut from = 0usize;
    loop {
        let Some(i) = index_word_call(&low, "generate_series", from) else {
            return sql;
        };
        let b = sql.as_bytes();
        let mut open = i + "generate_series".len();
        while open < b.len() && b[open] == b' ' {
            open += 1;
        }
        let Some((args, end)) = read_paren_args(&sql, open) else {
            return sql;
        };
        let tail = read_from_alias(&sql, &low, end);
        let col = tail.first_column("generate_series");
        let ord = tail.ordinality.then(|| tail.ordinality_column());
        let parts = split_top_level(&args);
        let mut repl = if parts.len() >= 2 {
            let mut step = "1";
            if parts.len() >= 3 {
                step = parts[2].trim();
            }
            gen_series_cte(parts[0].trim(), parts[1].trim(), step, &col, ord.as_deref())
        } else {
            let ord = ord.map_or(String::new(), |o| [", NULL AS ", &o].concat());
            ["(SELECT NULL AS ", &col, &ord, " WHERE 0)"].concat()
        };
        // SQLite takes the table alias, and the column list is in the subquery now.
        let mut stop = end;
        if tail.ordinality || !tail.cols.is_empty() {
            stop = tail.end;
            if !tail.alias.is_empty() {
                repl = [repl.as_str(), " ", &tail.alias].concat();
            }
        }
        sql = splice(&sql, i, stop, &repl);
        low = code_only(&sql).to_ascii_lowercase();
        from = i + repl.len();
    }
}

/// Builds the recursive CTE for generate_series(start, stop, step). The range
/// guard works for both ascending (step>0) and descending (step<0), and emits no
/// rows when start is already past stop. col is the output column name, and ord
/// that of a 1-based row number, for WITH ORDINALITY.
fn gen_series_cte(start: &str, stop: &str, step: &str, col: &str, ord: Option<&str>) -> String {
    let in_range = |v: &str| -> String {
        [
            "(((",
            step,
            ") > 0 AND ",
            v,
            " <= (",
            stop,
            ")) OR ",
            "((",
            step,
            ") < 0 AND ",
            v,
            " >= (",
            stop,
            ")))",
        ]
        .concat()
    };
    let (ord_decl, ord_seed, ord_next, ord_out) = match ord {
        Some(o) => (", ord", ", 1", ", ord + 1", [", ord AS ", o].concat()),
        None => ("", "", "", String::new()),
    };
    [
        "(WITH RECURSIVE _gs(value",
        ord_decl,
        ") AS (",
        "SELECT (",
        start,
        ")",
        ord_seed,
        " WHERE ",
        &in_range(&["(", start, ")"].concat()),
        " UNION ALL SELECT value + (",
        step,
        ")",
        ord_next,
        " FROM _gs WHERE ",
        &in_range(&["value + (", step, ")"].concat()),
        ") SELECT value AS ",
        col,
        &ord_out,
        " FROM _gs)",
    ]
    .concat()
}

/// What may follow a function in FROM: [WITH ORDINALITY] [[AS] alias [(columns)]].
/// Names stay as written, quotes included.
#[derive(Debug)]
pub(crate) struct FromAlias {
    pub(crate) ordinality: bool,
    pub(crate) alias: String,
    pub(crate) cols: Vec<String>,
    /// Just past the last of it, or where the reading started if nothing is there.
    pub(crate) end: usize,
}

impl FromAlias {
    /// The name of a one-column function's result: the column list's, or the
    /// alias, which Postgres also gives the column, or the function's own.
    pub(crate) fn first_column(&self, default: &str) -> String {
        match (self.cols.first(), self.alias.is_empty()) {
            (Some(c), _) => c.clone(),
            (None, false) => self.alias.clone(),
            (None, true) => default.to_string(),
        }
    }

    pub(crate) fn ordinality_column(&self) -> String {
        self.cols
            .get(1)
            .map_or("ordinality".to_string(), std::clone::Clone::clone)
    }
}

/// Reads what follows a function call ending at pos. low is the statement's
/// code_only text, lowered, so a keyword is never found in a quoted alias.
pub(crate) fn read_from_alias(sql: &str, low: &str, pos: usize) -> FromAlias {
    let b = sql.as_bytes();
    let mut out = FromAlias {
        ordinality: false,
        alias: String::new(),
        cols: Vec::new(),
        end: pos,
    };
    let mut p = skip_spaces(sql, pos);
    if has_word_at(low, p, "with") {
        let q = skip_spaces(sql, p + "with".len());
        if has_word_at(low, q, "ordinality") {
            out.ordinality = true;
            out.end = q + "ordinality".len();
            p = skip_spaces(sql, out.end);
        }
    }
    let as_written = has_word_at(low, p, "as");
    if as_written {
        p = skip_spaces(sql, p + "as".len());
    }
    let start = p;
    if p < b.len() && b[p] == b'"' {
        p += 1;
        while p < b.len() {
            if b[p] == b'"' {
                if b.get(p + 1) != Some(&b'"') {
                    p += 1;
                    break;
                }
                p += 1;
            }
            p += 1;
        }
    } else {
        while p < b.len() && is_word_byte(b[p]) {
            p += 1;
        }
    }
    if p == start || (!as_written && is_alias_keyword(&low[start..p])) {
        return out;
    }
    out.alias = sql[start..p].to_string();
    out.end = p;
    let q = skip_spaces(sql, p);
    if q < b.len()
        && b[q] == b'('
        && let Some((inner, end)) = read_paren_args(sql, q)
    {
        out.cols = split_top_level(&inner).iter().map(|c| c.trim().to_string()).collect();
        out.cols.retain(|c| !c.is_empty());
        out.end = end;
    }
    out
}

// A parenthesis closing right before an alias with a column list.
lazy_regex!(RE_ALIAS_LIST, r#"(?i)\)\s*(?:as\s+)?(?:"[^"]+"|\w+)\s*\("#);

/// The columns of a VALUES list or a SELECT, as far as the text tells: None for a star.
fn source_columns(source: &str) -> Option<Vec<String>> {
    let code = code_only(source).to_ascii_lowercase();
    let body = code.trim_start();
    if body.starts_with("values") {
        let open = source.find('(')?;
        let (first, _) = read_paren_args(source, open)?;
        let n = split_top_level(&first).len();
        return Some((1..=n).map(|i| format!("column{i}")).collect());
    }
    let start = top_level_keyword(source, "select")? + "select".len();
    let end = top_level_keyword(&source[start..], "from").map_or(source.len(), |f| start + f);
    let mut names = Vec::new();
    for item in split_top_level(&source[start..end]) {
        let item = item.trim();
        if item.ends_with('*') {
            return None;
        }
        let last = item.rsplit([' ', '\t', '\n', '.']).next().unwrap_or(item);
        let plain = last.bytes().all(is_word_byte) && !last.bytes().next().is_some_and(|c| c.is_ascii_digit());
        let quoted = last.len() > 2 && last.starts_with('"') && last.ends_with('"');
        names.push(if plain || quoted {
            last.to_string()
        } else {
            "\"?column?\"".to_string()
        });
    }
    Some(names)
}

/// Gives `(VALUES ...) AS v(a, b)` and `(SELECT ...) AS v(a, b)` their column names,
/// which SQLite has no syntax for, through a CTE's column list. An alias list shorter
/// than the columns leaves the rest as they are, so they are named here. Where a star
/// hides how many there are, the statement fails on the count: 0A000 and NOTES.md,
/// Limits.
pub(crate) fn rewrite_derived_aliases(sql: &str) -> String {
    if !RE_ALIAS_LIST.is_match(sql) {
        return sql.to_string();
    }
    let mut sql = sql.to_string();
    let mut n = 0;
    let mut from = 0;
    loop {
        let low = code_only(&sql).to_ascii_lowercase();
        let Some((open, end, tail)) = next_aliased_source(&sql, &low, from) else {
            return sql;
        };
        let source = sql[open + 1..end - 1].to_string();
        let mut cols = tail.cols.clone();
        if let Some(all) = source_columns(&source) {
            cols.extend(all.into_iter().skip(tail.cols.len()));
        }
        let cte = format!("_alias{n}");
        let repl = format!(
            "(WITH {cte}({}) AS ({source}) SELECT * FROM {cte}) AS {}",
            cols.join(", "),
            tail.alias
        );
        sql = splice(&sql, open, tail.end, &repl);
        n += 1;
        // Past the head written here: the source inside may have aliases of its own.
        from = open + 1;
    }
}

/// The next `(source) alias(columns)` from an index on: where its parentheses open and
/// end, and what follows them.
fn next_aliased_source(sql: &str, low: &str, from: usize) -> Option<(usize, usize, FromAlias)> {
    let b = sql.as_bytes();
    let mut i = from;
    while let Some(at) = low.get(i..)?.find('(') {
        let open = i + at;
        i = open + 1;
        let word = skip_spaces(sql, open + 1);
        if !has_word_at(low, word, "values") && !has_word_at(low, word, "select") {
            continue;
        }
        let Some((_, end)) = read_paren_args(sql, open) else {
            continue;
        };
        // What opens a subquery here is a FROM item, not an expression's value.
        let before = trim_end_chars(&low[..open], " \t\r\n");
        let in_from = before.ends_with(',') || ["from", "join", "using"].iter().any(|w| before.ends_with(w));
        if !in_from || b.get(open).is_none() {
            continue;
        }
        let tail = read_from_alias(sql, low, end);
        if !tail.cols.is_empty() && !tail.alias.is_empty() {
            return Some((open, end, tail));
        }
    }
    None
}

/// Reports whether w is a SQL keyword that can follow a FROM item (so it is not
/// an alias).
fn is_alias_keyword(w: &str) -> bool {
    matches!(
        w,
        "as" | "on"
            | "where"
            | "group"
            | "order"
            | "having"
            | "limit"
            | "offset"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "full"
            | "cross"
            | "natural"
            | "using"
            | "union"
            | "except"
            | "intersect"
            | "and"
            | "or"
            | "window"
            | "returning"
            | "for"
            | "fetch"
    )
}

/// Gives generate_subscripts a column-alias list, which SQLite has no syntax for, by
/// selecting its column under that name.
pub(crate) fn rewrite_aliased_table_funcs(sql: &str) -> String {
    const NAME: &str = "generate_subscripts";
    let mut sql = sql.to_string();
    let mut low = code_only(&sql).to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(i) = index_word_call(&low, NAME, from) {
        from = i + NAME.len();
        let open = skip_spaces(&sql, from);
        let Some((args, end)) = read_paren_args(&sql, open) else {
            continue;
        };
        let tail = read_from_alias(&sql, &low, end);
        if tail.cols.is_empty() {
            continue;
        }
        let parts = split_top_level(&args);
        let Some(arr) = parts.first().map(|a| a.trim()) else {
            continue;
        };
        let repl = format!(
            "(SELECT value AS {} FROM overlite_elements(overlite_subscripts({arr}))) {}",
            tail.cols[0], tail.alias
        );
        sql = splice(&sql, i, tail.end, &repl);
        low = code_only(&sql).to_ascii_lowercase();
        from = i + repl.len();
    }
    sql
}

/// Replaces the partition set-returning functions (used in FROM with a
/// column-alias list, e.g. "pg_partition_ancestors(x) AS a(relid, depth)" in
/// psql's \d trigger query) with an empty relation carrying those columns.
pub(crate) fn rewrite_partition_srf(sql: &str) -> String {
    let mut sql = sql.to_string();
    for n in ["pg_partition_ancestors", "pg_partition_tree", "pg_partition_root"] {
        sql = rewrite_empty_table_func(&sql, n);
    }
    sql
}

/// Replaces "name(...) [AS alias(cols)]" with an empty relation "(SELECT NULL AS
/// col, ... WHERE 0) [alias]".
fn rewrite_empty_table_func(sql: &str, name: &str) -> String {
    let mut sql = sql.to_string();
    let mut low = sql.to_ascii_lowercase();
    let lname = name.to_lowercase();
    let mut from = 0usize;
    loop {
        let Some(i) = index_word_call(&low, &lname, from) else {
            return sql;
        };
        let sb = sql.as_bytes();
        let mut open = i + name.len();
        while open < sb.len() && sb[open] == b' ' {
            open += 1;
        }
        let Some((_, end)) = read_paren_args(&sql, open) else {
            from = i + name.len();
            continue;
        };
        let mut cols: Vec<String> = vec![name.to_string()];
        let mut alias = "";
        let mut next = end;
        let mut p = skip_spaces(&sql, end);
        if has_word_at(&low, p, "as") {
            p = skip_spaces(&sql, p + 2);
        }
        let as_ = p;
        while p < sb.len() && is_word_byte(sb[p]) {
            p += 1;
        }
        if p > as_ {
            alias = &sql[as_..p];
            next = p;
            let q = skip_spaces(&sql, p);
            if q < sb.len()
                && sb[q] == b'('
                && let Some((inner, cend)) = read_paren_args(&sql, q)
            {
                cols = split_top_level(&inner);
                next = cend;
            }
        }
        let mut b = String::from("(SELECT ");
        for (k, c) in cols.iter().enumerate() {
            if k > 0 {
                b.push_str(", ");
            }
            b.push_str("NULL AS ");
            b.push_str(c.trim());
        }
        b.push_str(" WHERE 0)");
        if !alias.is_empty() {
            b.push(' ');
            b.push_str(alias);
        }
        let repl = b;
        sql = splice(&sql, i, next, &repl);
        low = sql.to_ascii_lowercase();
        from = i + repl.len();
    }
}

/// Replaces pg_options_to_table(...) — a set-returning function pg_dump expands
/// reloptions with — by an empty (option_name, option_value) relation.
pub(crate) fn rewrite_pg_options_to_table(sql: &str) -> String {
    replace_call(
        sql,
        "pg_options_to_table",
        "(SELECT NULL AS option_name, NULL AS option_value WHERE 0)",
    )
}
