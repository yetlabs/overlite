//! A DELETE or UPDATE that joins other tables: `DELETE FROM t USING u WHERE ...` and
//! `UPDATE t SET ... FROM u WHERE ...`.
//!
//! SQLite has UPDATE ... FROM but no DELETE ... USING, so a DELETE becomes one over the
//! rows the join finds: `DELETE FROM t WHERE rowid IN (SELECT t.rowid FROM t, u WHERE
//! ...)`. A target row the join meets twice is deleted once, as in Postgres. SQLite also
//! takes an alias only after AS, and a RETURNING clause there reads the target by its
//! table name and no other table at all: a RETURNING item that reads a joined table
//! becomes a subquery over the join, for the row being returned.

use overlite_engine::code_only;

/// Keywords that end a table reference where an alias could stand.
const NOT_ALIAS: &[&str] = &[
    "using",
    "where",
    "returning",
    "set",
    "from",
    "on",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "natural",
    "outer",
];

/// A word at depth zero of the statement, lowered, with where it lies.
struct Word {
    start: usize,
    end: usize,
    text: String,
}

/// The words at depth zero, read from the code (literals and quoted names blank).
fn top_words(code: &str) -> Vec<Word> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut i) = (0i32, 0);
    while i < b.len() {
        let c = b[i];
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
        } else if depth == 0 && (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !is_word(b[i - 1])) {
            let start = i;
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
            out.push(Word {
                start,
                end: i,
                text: code[start..i].to_ascii_lowercase(),
            });
            continue;
        }
        i += 1;
    }
    out
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// The identifier at i, quoted or bare, and where it ends.
fn ident_at(sql: &str, i: usize) -> Option<(String, usize)> {
    let b = sql.as_bytes();
    if i >= b.len() {
        return None;
    }
    if b[i] == b'"' {
        let mut j = i + 1;
        loop {
            let k = j + sql[j..].find('"')?;
            if b.get(k + 1) == Some(&b'"') {
                j = k + 2;
                continue;
            }
            return Some((sql[i..=k].to_string(), k + 1));
        }
    }
    if !(b[i].is_ascii_alphabetic() || b[i] == b'_' || b[i] >= 0x80) {
        return None;
    }
    let mut j = i;
    while j < b.len() && (is_word(b[j]) || b[j] >= 0x80) {
        j += 1;
    }
    Some((sql[i..j].to_string(), j))
}

/// The qualifier a name is matched by: a quoted one as written, a bare one folded.
fn fold(name: &str) -> String {
    match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => name.to_ascii_lowercase(),
    }
}

/// A table reference: its name as written, its alias if any, and where it ends.
struct Target {
    name: String,
    alias: Option<String>,
    end: usize,
}

/// Reads `[ONLY] name[.name] [*] [[AS] alias]` from i.
fn read_target(sql: &str, i: usize) -> Option<Target> {
    let b = sql.as_bytes();
    let mut i = skip_ws(b, i);
    if let Some((w, end)) = ident_at(sql, i)
        && w.eq_ignore_ascii_case("only")
        && ident_at(sql, skip_ws(b, end)).is_some()
    {
        i = skip_ws(b, end);
    }
    let start = i;
    let (_, mut end) = ident_at(sql, i)?;
    while b.get(end) == Some(&b'.') {
        end = ident_at(sql, end + 1)?.1;
    }
    let name = sql[start..end].to_string();
    let mut j = skip_ws(b, end);
    if b.get(j) == Some(&b'*') {
        j = skip_ws(b, j + 1);
    }
    let mut alias = None;
    let mut after = end;
    if let Some((w, e)) = ident_at(sql, j) {
        if w.eq_ignore_ascii_case("as") {
            let k = skip_ws(b, e);
            let (a, e2) = ident_at(sql, k)?;
            alias = Some(a);
            after = e2;
        } else if w.starts_with('"') || !NOT_ALIAS.contains(&w.to_ascii_lowercase().as_str()) {
            alias = Some(w);
            after = e;
        }
    }
    Some(Target {
        name,
        alias,
        end: after,
    })
}

/// The names a FROM list (or USING list) lets a column be qualified by.
fn qualifiers(list: &str) -> Vec<String> {
    let code = code_only(list);
    let b = list.as_bytes();
    let mut out = Vec::new();
    let mut starts = vec![0];
    let (mut depth, mut i) = (0i32, 0);
    let cb = code.as_bytes();
    while i < cb.len() {
        match cb[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => starts.push(i + 1),
            c if depth == 0 && c.is_ascii_alphabetic() && (i == 0 || !is_word(cb[i - 1])) => {
                let mut j = i;
                while j < cb.len() && is_word(cb[j]) {
                    j += 1;
                }
                if code[i..j].eq_ignore_ascii_case("join") {
                    starts.push(j);
                }
                i = j;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    for s in starts {
        let i = skip_ws(b, s);
        if b.get(i) == Some(&b'(') {
            // A derived table: its alias follows the parenthesis.
            let Some(close) = closing_paren(list, i) else {
                continue;
            };
            if let Some(t) = read_alias(list, close + 1) {
                out.push(fold(&t));
            }
            continue;
        }
        if let Some(t) = read_target(list, i) {
            let q = t
                .alias
                .unwrap_or_else(|| t.name.rsplit('.').next().unwrap_or(&t.name).to_string());
            out.push(fold(&q));
        }
    }
    out
}

fn read_alias(sql: &str, i: usize) -> Option<String> {
    let b = sql.as_bytes();
    let i = skip_ws(b, i);
    let (w, e) = ident_at(sql, i)?;
    if w.eq_ignore_ascii_case("as") {
        return ident_at(sql, skip_ws(b, e)).map(|(a, _)| a);
    }
    (!NOT_ALIAS.contains(&w.to_ascii_lowercase().as_str())).then_some(w)
}

fn closing_paren(sql: &str, open: usize) -> Option<usize> {
    let code = code_only(sql);
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

/// Each `q.` in the code of s whose qualifier is in names, by position and length.
fn qualified_refs(s: &str, names: &[String]) -> Vec<(usize, usize)> {
    let blank = overlite_engine::blank_literals(s);
    let b = blank.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != b'\'' {
                i += 1;
            }
            i += 1;
            continue;
        }
        let starts_name = c == b'"' || ((c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !is_word(b[i - 1])));
        if starts_name
            && (i == 0 || b[i - 1] != b'.')
            && let Some((w, end)) = ident_at(s, i)
        {
            if b.get(end) == Some(&b'.') && names.contains(&fold(&w)) {
                out.push((i, end - i));
            }
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// s with every `old.` qualifier in it written `new.`.
fn requalify(s: &str, old: &str, new: &str) -> String {
    let refs = qualified_refs(s, &[fold(old)]);
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for (i, n) in refs {
        out.push_str(&s[at..i]);
        out.push_str(new);
        at = i + n;
    }
    out.push_str(&s[at..]);
    out
}

/// Splits a RETURNING list at its top-level commas.
fn split_items(list: &str) -> Vec<&str> {
    let code = code_only(list);
    let mut out = Vec::new();
    let (mut depth, mut from) = (0i32, 0);
    for (i, c) in code.bytes().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&list[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(&list[from..]);
    out
}

/// A RETURNING list SQLite can run: the target read by its table name, and an item
/// reading a joined table as a subquery over the join for this row.
fn returning_list(list: &str, target: &Target, joined: &str, cond: Option<&str>, joined_names: &[String]) -> String {
    let table = &target.name;
    let to_target = |s: &str| match &target.alias {
        Some(a) => requalify(s, a, table),
        None => s.to_string(),
    };
    let items: Vec<String> = split_items(list)
        .into_iter()
        .map(|item| {
            let trimmed = item.trim();
            // `alias.*` is the target's columns, which SQLite spells `*`.
            if let Some(a) = &target.alias
                && let Some(q) = trimmed.strip_suffix(".*")
                && fold(q.trim()) == fold(a)
            {
                return " *".to_string();
            }
            if qualified_refs(item, joined_names).is_empty() {
                return to_target(item);
            }
            let code = code_only(trimmed);
            let (expr, label) = match last_top_level_as(&code) {
                Some(at) => (trimmed[..at].trim_end(), Some(trimmed[at + 2..].trim().to_string())),
                None => (trimmed, None),
            };
            let label = label.or_else(|| {
                let last = expr.rsplit('.').next().unwrap_or(expr);
                (expr.contains('.') && ident_at(last, 0).is_some_and(|(w, e)| e == last.len() && !w.is_empty()))
                    .then(|| last.to_string())
            });
            let mut sub = format!(" (SELECT {} FROM {}", to_target(expr), to_target(joined.trim()));
            if let Some(c) = cond {
                sub.push_str(" WHERE ");
                sub.push_str(&to_target(c.trim()));
            }
            sub.push_str(" LIMIT 1)");
            if let Some(l) = label {
                sub.push_str(" AS ");
                sub.push_str(&l);
            }
            sub
        })
        .collect();
    items.join(",")
}

/// The index of an item's AS label at depth zero.
fn last_top_level_as(code: &str) -> Option<usize> {
    top_words(code)
        .into_iter()
        .rev()
        .find(|w| w.text == "as")
        .map(|w| w.start)
}

/// The word index of the statement's verb, past a WITH clause, if it is verb.
fn verb_at(words: &[Word], verb: &str) -> Option<usize> {
    let first = words.first()?;
    if first.text == verb {
        return Some(0);
    }
    if first.text != "with" {
        return None;
    }
    words.iter().position(|w| w.text == verb).filter(|&i| {
        // Nothing but CTE names and AS / RECURSIVE / MATERIALIZED words before it.
        words[1..i]
            .iter()
            .all(|w| !matches!(w.text.as_str(), "select" | "insert" | "update" | "delete"))
    })
}

/// `DELETE FROM t [AS a] USING ... [WHERE ...] [RETURNING ...]`, as a DELETE over the
/// rowids the join finds.
pub(crate) fn rewrite_delete_using(sql: &str) -> String {
    let code = code_only(sql);
    let words = top_words(&code);
    let Some(d) = verb_at(&words, "delete") else {
        return sql.to_string();
    };
    let Some(from) = words.get(d + 1).filter(|w| w.text == "from") else {
        return sql.to_string();
    };
    let Some(target) = read_target(sql, from.end) else {
        return sql.to_string();
    };
    let Some(using) = words.iter().find(|w| w.start >= target.end) else {
        return sql.to_string();
    };
    if using.text != "using" || !sql[target.end..using.start].trim().is_empty() {
        return sql.to_string();
    }
    let after = |kw: &str| {
        words
            .iter()
            .find(|w| w.start > using.start && w.text == kw)
            .map(|w| (w.start, w.end))
    };
    let ret = after("returning");
    let body_end = ret.map_or_else(|| sql.trim_end().trim_end_matches(';').len(), |r| r.0);
    let wh = after("where").filter(|w| w.0 < body_end);
    let joined = &sql[using.end..wh.map_or(body_end, |w| w.0)];
    let cond = wh.map(|w| &sql[w.1..body_end]);
    let reference = target.alias.clone().unwrap_or_else(|| target.name.clone());
    let aliased = match &target.alias {
        Some(a) => format!("{} AS {}", target.name, a),
        None => target.name.clone(),
    };
    let mut out = String::with_capacity(sql.len() + 64);
    out.push_str(&sql[..from.end]);
    out.push(' ');
    out.push_str(&target.name);
    out.push_str(" WHERE rowid IN (SELECT ");
    out.push_str(&reference);
    out.push_str(".rowid FROM ");
    out.push_str(&aliased);
    out.push_str(", ");
    out.push_str(joined.trim());
    if let Some(c) = cond {
        out.push_str(" WHERE ");
        out.push_str(c.trim());
    }
    out.push(')');
    if let Some((_, rend)) = ret {
        let list = sql[rend..].trim_end().trim_end_matches(';');
        out.push_str(" RETURNING");
        out.push_str(&returning_list(list, &target, joined, cond, &qualifiers(joined)));
    }
    out
}

/// `UPDATE t [AS] a SET ... [FROM ...] [WHERE ...] [RETURNING ...]`: the alias takes its
/// AS, and RETURNING reads what SQLite lets it.
pub(crate) fn rewrite_update_from(sql: &str) -> String {
    let code = code_only(sql);
    let words = top_words(&code);
    let Some(u) = verb_at(&words, "update") else {
        return sql.to_string();
    };
    let Some(target) = read_target(sql, words[u].end) else {
        return sql.to_string();
    };
    let Some(set) = words.iter().find(|w| w.start >= target.end) else {
        return sql.to_string();
    };
    if set.text != "set" {
        return sql.to_string();
    }
    let ret = words
        .iter()
        .find(|w| w.start > set.start && w.text == "returning")
        .map(|w| (w.start, w.end));
    if target.alias.is_none() && ret.is_none() {
        return sql.to_string();
    }
    let body_end = ret.map_or_else(|| sql.trim_end().trim_end_matches(';').len(), |r| r.0);
    let after = |kw: &str| {
        words
            .iter()
            .find(|w| w.start > set.start && w.start < body_end && w.text == kw)
            .map(|w| (w.start, w.end))
    };
    let from = after("from");
    let wh = after("where");
    let mut out = String::with_capacity(sql.len() + 64);
    out.push_str(&sql[..words[u].end]);
    out.push(' ');
    out.push_str(&target.name);
    if let Some(a) = &target.alias {
        out.push_str(" AS ");
        out.push_str(a);
    }
    out.push(' ');
    out.push_str(&sql[set.start..body_end]);
    if let Some((_, rend)) = ret {
        let list = sql[rend..].trim_end().trim_end_matches(';');
        let (joined, names) = match from {
            Some(f) => {
                let j = &sql[f.1..wh.filter(|w| w.0 > f.0).map_or(body_end, |w| w.0)];
                (j, qualifiers(j))
            }
            None => ("", Vec::new()),
        };
        let cond = wh.map(|w| &sql[w.1..body_end]);
        if !out.ends_with(char::is_whitespace) {
            out.push(' ');
        }
        out.push_str("RETURNING");
        out.push_str(&returning_list(list, &target, joined, cond, &names));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn rewrite_delete_using_cases() {
        assert_rewrites(
            rewrite_delete_using,
            &[
                (
                    "DELETE FROM project_issue USING project WHERE project.id = project_issue.project_id AND project.repo_id = $1",
                    "DELETE FROM project_issue WHERE rowid IN (SELECT project_issue.rowid FROM project_issue, project WHERE project.id = project_issue.project_id AND project.repo_id = $1)",
                ),
                (
                    "DELETE FROM t a USING u AS b, v WHERE a.x = b.x AND v.y = b.y RETURNING a.id, b.name",
                    "DELETE FROM t WHERE rowid IN (SELECT a.rowid FROM t AS a, u AS b, v WHERE a.x = b.x AND v.y = b.y) RETURNING t.id, (SELECT b.name FROM u AS b, v WHERE t.x = b.x AND v.y = b.y LIMIT 1) AS name",
                ),
                ("DELETE FROM t WHERE id = 1", "DELETE FROM t WHERE id = 1"),
                (
                    "DELETE FROM t USING u",
                    "DELETE FROM t WHERE rowid IN (SELECT t.rowid FROM t, u)",
                ),
                ("DELETE FROM t WHERE x = 'USING u'", "DELETE FROM t WHERE x = 'USING u'"),
                (
                    r#"DELETE FROM ONLY "T" AS "a" USING u JOIN v ON v.id = u.v WHERE "a".k = u.k;"#,
                    r#"DELETE FROM "T" WHERE rowid IN (SELECT "a".rowid FROM "T" AS "a", u JOIN v ON v.id = u.v WHERE "a".k = u.k)"#,
                ),
            ],
        );
    }

    #[test]
    fn rewrite_update_from_cases() {
        assert_rewrites(
            rewrite_update_from,
            &[
                ("UPDATE t SET a = 1 WHERE id = 2", "UPDATE t SET a = 1 WHERE id = 2"),
                (
                    "UPDATE t x SET a = u.b FROM u WHERE u.id = x.id",
                    "UPDATE t AS x SET a = u.b FROM u WHERE u.id = x.id",
                ),
                (
                    "UPDATE t AS x SET a = u.b FROM u WHERE u.id = x.id RETURNING x.id, u.b AS ub",
                    "UPDATE t AS x SET a = u.b FROM u WHERE u.id = x.id RETURNING t.id, (SELECT u.b FROM u WHERE u.id = t.id LIMIT 1) AS ub",
                ),
                ("UPDATE t SET a = 1 RETURNING id", "UPDATE t SET a = 1 RETURNING id"),
            ],
        );
    }
}
