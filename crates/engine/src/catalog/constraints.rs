//! SQLite keeps a constraint's name in the table's DDL but reports neither it
//! nor anything keyed by it. Postgres clients read the names back from
//! pg_constraint and compare them with the ones they created, so the names are
//! recovered from the DDL, falling back to the names Postgres itself gives an
//! unnamed constraint.

use crate::builtins::json::json_quote;
use crate::catalog;
use crate::catalog::sqlite_type_oid;
use overlite_core::lazy_regex;
use overlite_core::text::eq_fold;

/// One identifier as SQLite accepts it: "double quoted", `backticked`,
/// [bracketed] or bare.
const SQL_IDENT: &str = r#"(?:"(?:[^"]|"")+"|`[^`]+`|\[[^\]]+\]|[A-Za-z_][\w$]*)"#;

lazy_regex!(
    RE_NAMED_FK,
    &format!(
        r"(?is)\bCONSTRAINT\s+({SQL_IDENT})\s+FOREIGN\s+KEY\s*\(([^)]*)\)\s*REFERENCES\s+({SQL_IDENT}(?:\s*\.\s*{SQL_IDENT})?)"
    )
);
lazy_regex!(
    RE_NAMED_PK,
    &format!(r"(?is)\bCONSTRAINT\s+({SQL_IDENT})\s+PRIMARY\s+KEY\b")
);
lazy_regex!(RE_QUALIFIED, &format!(r"^({SQL_IDENT})\s*\.\s*({SQL_IDENT})$"));

/// Strips the quoting from one identifier ("a""b" -> a"b).
pub fn unquote_sql_ident(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 {
        let (first, last) = (b[0], b[b.len() - 1]);
        if first == b'"' && last == b'"' {
            return s[1..s.len() - 1].replace("\"\"", "\"");
        }
        if (first == b'`' && last == b'`') || (first == b'[' && last == b']') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// Splits a parenthesized column list body into unquoted names.
fn ident_list(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for p in body.split(',') {
        let p = unquote_sql_ident(p);
        if !p.is_empty() {
            out.push(p);
        }
    }
    out
}

/// Drops a schema qualifier: public.users and "public"."users" both name users.
pub fn relation_name(r: &str) -> String {
    let r = r.trim();
    if let Some(m) = RE_QUALIFIED.captures(r) {
        return unquote_sql_ident(&m[2]);
    }
    unquote_sql_ident(r)
}

/// Names the foreign key of table over from_cols (in key order) that references
/// ref_table, the one pragma_foreign_key_list numbers id: the name its DDL gave it, or
/// Postgres' default <table>_<col>[_<col>...]_fkey.
pub(crate) fn fk_constraint_name(ddl: &str, table: &str, from_cols: &[String], ref_table: &str, id: i64) -> String {
    if let Some(clause) = fk_clause(ddl, from_cols, ref_table, id) {
        return clause
            .name
            .unwrap_or_else(|| format!("{}_{}_fkey", table, from_cols.join("_")));
    }
    for m in RE_NAMED_FK.captures_iter(ddl) {
        let cols = ident_list(&m[2]);
        if eq_fold(&relation_name(&m[3]), ref_table) && equal_fold_list(&cols, from_cols) {
            return unquote_sql_ident(&m[1]);
        }
    }
    format!("{}_{}_fkey", table, from_cols.join("_"))
}

/// Names the UNIQUE constraint of table over cols (in key order): the name its DDL
/// gave it, on its column or apart, or Postgres' default <table>_<col>[_<col>...]_key.
pub(crate) fn unique_constraint_name(ddl: &str, table: &str, cols: &[String]) -> String {
    unique_clause(ddl, cols)
        .and_then(|c| c.name)
        .unwrap_or_else(|| format!("{}_{}_key", table, cols.join("_")))
}

/// How the UNIQUE constraint over cols is checked, as pg_constraint has it: 0 not
/// deferrable, 1 deferrable and initially immediate, 2 deferrable and initially
/// deferred. SQLite takes no DEFERRABLE there, so the rewrite keeps it in a comment
/// after the key, `/*overlite deferrable initially deferred*/`.
pub(crate) fn unique_deferral(ddl: &str, cols: &[String]) -> i64 {
    let Some(c) = unique_clause(ddl, cols) else { return 0 };
    let rest = &ddl[c.end..];
    let end = item_end(rest);
    let item = &rest[..end];
    if item.contains("/*overlite deferrable initially deferred*/") {
        2
    } else {
        i64::from(item.contains("/*overlite deferrable*/"))
    }
}

/// Where the item that text starts in ends: its first comma or closing parenthesis
/// outside parentheses and quotes of its own.
fn item_end(text: &str) -> usize {
    let b = text.as_bytes();
    let (mut depth, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = text[i + 2..].find("*/").map_or(b.len(), |e| i + e + 3);
            }
            b'(' => depth += 1,
            b')' if depth == 0 => return i,
            b')' => depth -= 1,
            b',' if depth == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// The UNIQUE constraint of a CREATE TABLE over cols: the name it is given, and
/// where in the DDL its key ends.
struct UniqueClause {
    name: Option<String>,
    end: usize,
}

fn unique_clause(ddl: &str, cols: &[String]) -> Option<UniqueClause> {
    unique_clauses(ddl, cols).into_iter().next()
}

/// Every UNIQUE constraint of a CREATE TABLE over cols, in the order written. SQLite
/// makes one index for all of them, where Postgres keeps each.
fn unique_clauses(ddl: &str, cols: &[String]) -> Vec<UniqueClause> {
    let mut found = Vec::new();
    let toks = ddl_tokens(ddl);
    let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.word && eq_fold(&t.text, w));
    let mark = |i: usize, m: &str| toks.get(i).is_some_and(|t| !t.word && t.text == m);
    let mut i = 0;
    while i < toks.len() {
        if toks[i].depth == 1 && is(i, "unique") {
            let named = i >= 2 && is(i - 2, "constraint");
            let name = named.then(|| toks[i - 1].text.clone());
            // A table's UNIQUE (cols), or one on the column its item starts with.
            let (key, end): (Vec<String>, usize) = if mark(i + 1, "(") {
                let inner: Vec<usize> = (i + 2..toks.len()).take_while(|&k| toks[k].depth == 2).collect();
                let close = inner.last().map_or(i + 2, |&k| k + 1);
                let end = toks.get(close).map_or(ddl.len(), |t| t.pos + 1);
                (
                    inner
                        .into_iter()
                        .filter(|&k| toks[k].word)
                        .map(|k| toks[k].text.clone())
                        .collect(),
                    end,
                )
            } else {
                let start = (0..i)
                    .rev()
                    .find(|&k| (toks[k].depth == 1 && mark(k, ",")) || (toks[k].depth == 0 && mark(k, "(")))
                    .map_or(0, |k| k + 1);
                let key = toks.get(start).map(|t| vec![t.text.clone()]).unwrap_or_default();
                (key, toks[i].pos + "unique".len())
            };
            if equal_fold_list(&key, cols) {
                found.push(UniqueClause { name, end });
            }
        }
        i += 1;
    }
    found
}

/// The UNIQUE constraints of a CREATE TABLE over cols as a JSON array of objects:
/// name and deferral (as unique_deferral has it). One with the name Postgres gives
/// when the DDL names none.
pub(crate) fn unique_constraints(ddl: &str, table: &str, cols: &[String]) -> String {
    let clauses = unique_clauses(ddl, cols);
    let default = || format!("{}_{}_key", table, cols.join("_"));
    if clauses.is_empty() {
        return format!(r#"[{{"name":{},"deferral":0}}]"#, json_quote(&default()));
    }
    let items: Vec<String> = clauses
        .iter()
        .map(|c| {
            let rest = &ddl[c.end..];
            let item = &rest[..item_end(rest)];
            let deferral = if item.contains("/*overlite deferrable initially deferred*/") {
                2
            } else {
                i64::from(item.contains("/*overlite deferrable*/"))
            };
            format!(
                r#"{{"name":{},"deferral":{deferral}}}"#,
                json_quote(&c.name.clone().unwrap_or_else(default))
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// How a foreign key is checked, as pg_constraint has it: 0 not deferrable, 1
/// deferrable and initially immediate, 2 deferrable and initially deferred.
pub(crate) fn fk_deferral(ddl: &str, from_cols: &[String], ref_table: &str, id: i64) -> i64 {
    fk_clause(ddl, from_cols, ref_table, id).map_or(0, |c| match (c.deferrable, c.deferred) {
        (false, _) => 0,
        (true, false) => 1,
        (true, true) => 2,
    })
}

/// A foreign key as a CREATE TABLE declares it, on its column or apart.
#[derive(Debug, Default, PartialEq)]
struct FkClause {
    name: Option<String>,
    cols: Vec<String>,
    ref_table: String,
    deferrable: bool,
    deferred: bool,
}

/// The clause of the foreign key pragma_foreign_key_list numbers id. SQLite numbers
/// them from the last declared, so id counts from the end; the columns and the table
/// have to agree, and when they do not the first clause that does is it.
fn fk_clause(ddl: &str, from_cols: &[String], ref_table: &str, id: i64) -> Option<FkClause> {
    let mut all = fk_clauses(ddl);
    let fits = |c: &FkClause| eq_fold(&c.ref_table, ref_table) && equal_fold_list(&c.cols, from_cols);
    let at = usize::try_from(id).ok().and_then(|id| all.len().checked_sub(id + 1));
    if let Some(at) = at.filter(|&at| fits(&all[at])) {
        return Some(all.swap_remove(at));
    }
    let at = all.iter().position(fits)?;
    Some(all.swap_remove(at))
}

/// One token of a CREATE TABLE: a word or a quoted name (unquoted), a string, or a
/// mark, with how deep in parentheses it is.
#[derive(Debug)]
struct Tok {
    text: String,
    word: bool,
    depth: i32,
    /// Where it starts in the DDL.
    pos: usize,
}

fn ddl_tokens(ddl: &str) -> Vec<Tok> {
    let b = ddl.as_bytes();
    let (mut out, mut depth, mut i) = (Vec::new(), 0, 0);
    while i < b.len() {
        let c = b[i];
        let start = i;
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = ddl[i..].find('\n').map_or(b.len(), |e| i + e);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = ddl[i + 2..].find("*/").map_or(b.len(), |e| i + e + 4);
            }
            b'\'' | b'"' | b'`' | b'[' => {
                let close = if c == b'[' { b']' } else { c };
                i += 1;
                while i < b.len() {
                    if b[i] == close {
                        if c != b'[' && b.get(i + 1) == Some(&close) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                out.push(Tok {
                    text: if c == b'\'' {
                        ddl[start..i].to_string()
                    } else {
                        unquote_sql_ident(&ddl[start..i])
                    },
                    word: c != b'\'',
                    depth,
                    pos: start,
                });
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                out.push(Tok {
                    text: ddl[start..i].to_string(),
                    word: true,
                    depth,
                    pos: start,
                });
            }
            _ => {
                if c == b')' {
                    depth -= 1;
                }
                out.push(Tok {
                    text: (c as char).to_string(),
                    word: false,
                    depth,
                    pos: start,
                });
                if c == b'(' {
                    depth += 1;
                }
                i += 1;
            }
        }
    }
    out
}

/// Every foreign key a CREATE TABLE declares, in the order it declares them.
fn fk_clauses(ddl: &str) -> Vec<FkClause> {
    let toks = ddl_tokens(ddl);
    let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.word && eq_fold(&t.text, w));
    let mark = |i: usize, m: &str| toks.get(i).is_some_and(|t| !t.word && t.text == m);
    let Some(open) = toks.iter().position(|t| t.depth == 0 && t.text == "(") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // The items of the column list, at depth 1, between commas.
    let mut start = open + 1;
    while start < toks.len() && toks[start].depth >= 1 {
        let mut end = start;
        while end < toks.len() && toks[end].depth >= 1 && !(toks[end].depth == 1 && mark(end, ",")) {
            end += 1;
        }
        let item = start..end;
        let table_level = is(start, "foreign")
            || (is(start, "constraint") && toks.get(start + 2).is_some_and(|_| is(start + 2, "foreign")));
        let column = if table_level
            || is(start, "constraint")
            || is(start, "primary")
            || is(start, "unique")
            || is(start, "check")
        {
            None
        } else {
            toks.get(start).map(|t| t.text.clone())
        };
        let mut i = item.start;
        while i < item.end {
            if toks[i].depth != 1 || !is(i, "references") {
                i += 1;
                continue;
            }
            let name = (i >= item.start + 2 && is(i - 2, "constraint")).then(|| toks[i - 1].text.clone());
            let name = if table_level {
                is(item.start, "constraint").then(|| toks[item.start + 1].text.clone())
            } else {
                name
            };
            let cols = if table_level {
                let k = (item.start..i).find(|&k| mark(k, "(")).unwrap_or(i);
                (k + 1..i)
                    .filter(|&k| toks[k].word && toks[k].depth == 2)
                    .map(|k| toks[k].text.clone())
                    .collect()
            } else {
                column.clone().into_iter().collect()
            };
            // The table, schema-qualified or not.
            let mut j = i + 1;
            let mut ref_table = toks.get(j).map(|t| t.text.clone()).unwrap_or_default();
            if mark(j + 1, ".") {
                j += 2;
                ref_table = toks.get(j).map(|t| t.text.clone()).unwrap_or_default();
            }
            j += 1;
            let mut c = FkClause {
                name,
                cols,
                ref_table,
                ..FkClause::default()
            };
            // Its actions and deferral, up to the next constraint.
            while j < item.end {
                if toks[j].depth == 1
                    && [
                        "constraint",
                        "references",
                        "primary",
                        "unique",
                        "check",
                        "default",
                        "collate",
                        "generated",
                    ]
                    .iter()
                    .any(|w| is(j, w))
                {
                    break;
                }
                if is(j, "deferrable") {
                    c.deferrable = !(j > 0 && is(j - 1, "not"));
                } else if is(j, "initially") {
                    c.deferred = is(j + 1, "deferred");
                }
                j += 1;
            }
            out.push(c);
            i = j;
        }
        start = end + 1;
    }
    out
}

/// The primary key's name from the DDL, or Postgres' default <table>_pkey.
pub(crate) fn pk_constraint_name(ddl: &str, table: &str) -> String {
    if let Some(m) = RE_NAMED_PK.captures(ddl) {
        return unquote_sql_ident(&m[1]);
    }
    format!("{table}_pkey")
}

fn equal_fold_list(a: &[String], b: &[String]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for i in 0..a.len() {
        if !eq_fold(&a[i], &b[i]) {
            return false;
        }
    }
    true
}

lazy_regex!(
    RE_IDENTITY_MARK,
    r"/\*overlite identity (always|by default) ([a-z0-9 ]+)\*/"
);

/// An identity column of a CREATE TABLE: its name, whether it is GENERATED ALWAYS,
/// and the type it was declared with. The server keeps these in a comment at the
/// end of the column's definition, `/*overlite identity always bigint*/`, which
/// SQLite keeps with the table's SQL through every rename and rebuild.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub column: String,
    pub always: bool,
    pub type_name: String,
}

/// The identity columns of a CREATE TABLE.
pub fn identity_columns(ddl: &str) -> Vec<Identity> {
    if !ddl.contains("/*overlite identity") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (column, text) in column_items(ddl) {
        // Not one a default's literal holds.
        let mut in_string = false;
        let code: String = text
            .chars()
            .map(|c| {
                if c == '\'' {
                    in_string = !in_string;
                    return c;
                }
                if in_string { ' ' } else { c }
            })
            .collect();
        if let Some(m) = RE_IDENTITY_MARK.captures(&code) {
            out.push(Identity {
                column,
                always: &m[1] == "always",
                type_name: m[2].to_string(),
            });
        }
    }
    out
}

/// Whether col of a CREATE TABLE is a key that is the rowid declared an int4: a serial's
/// (`/*overlite int4*/`, which ast::ddl writes after its AUTOINCREMENT) or an integer
/// identity's. SQLite declares any rowid INTEGER, which says nothing of its width.
pub(crate) fn int4_key(ddl: &str, col: &str) -> bool {
    if !ddl.contains("/*overlite int4*/") && !ddl.contains("integer*/") {
        return false;
    }
    if identity_of(ddl, col).is_some_and(|i| i.type_name == "integer") {
        return true;
    }
    column_items(ddl)
        .into_iter()
        .any(|(column, text)| eq_fold(&column, col) && text.contains("/*overlite int4*/"))
}

fn identity_of(ddl: &str, col: &str) -> Option<Identity> {
    identity_columns(ddl).into_iter().find(|i| eq_fold(&i.column, col))
}

/// pg_attribute.attidentity of col: 'a', 'd' or ''.
pub(crate) fn attidentity(ddl: &str, col: &str) -> &'static str {
    match identity_of(ddl, col) {
        Some(i) if i.always => "a",
        Some(_) => "d",
        None => "",
    }
}

/// The type an identity column was declared with, which a key that is SQLite's
/// rowid does not keep.
pub(crate) fn identity_type(ddl: &str, col: &str) -> Option<String> {
    identity_of(ddl, col).map(|i| i.type_name)
}

/// An item of a column list with its first name.
fn named_item(text: &str) -> (String, &str) {
    let t = text.trim_start();
    let name = if t.starts_with(['"', '`', '[']) {
        let close = match t.as_bytes()[0] {
            b'[' => ']',
            c => c as char,
        };
        t[1..].find(close).map_or(t, |e| &t[..e + 2])
    } else {
        t.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
            .next()
            .unwrap_or("")
    };
    (unquote_sql_ident(name), text)
}

/// The CHECK constraints of a CREATE TABLE as a JSON array of objects: name (the one
/// given, or Postgres' <table>_<col>_check when it reads one column, else
/// <table>_check), cols (the columns it reads, in table order), expr, and enum (an
/// unnamed `col IN ('a', ...)` on its column, the shape an enum column is stored in).
pub(crate) fn check_constraints(ddl: &str, relation: &str) -> String {
    let items = column_items(ddl);
    let is_table_item = |name: &str, text: &str| {
        let first = text
            .trim_start()
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("");
        !text.trim_start().starts_with(['"', '`', '['])
            && ["constraint", "primary", "unique", "check", "foreign", "exclude"]
                .iter()
                .any(|w| eq_fold(first, w))
            && !name.is_empty()
    };
    let columns: Vec<&str> = items
        .iter()
        .filter(|(name, text)| !is_table_item(name, text))
        .map(|(name, _)| name.as_str())
        .collect();
    let mut out = Vec::new();
    for (name, text) in &items {
        let own = (!is_table_item(name, text)).then_some(name.as_str());
        for (given, expr, not_valid) in checks_in(text) {
            let read: Vec<&str> = columns
                .iter()
                .copied()
                .filter(|c| ddl_tokens(&expr).iter().any(|t| t.word && eq_fold(&t.text, c)))
                .collect();
            let name = given.clone().unwrap_or_else(|| match (own, read.as_slice()) {
                (Some(col), _) => format!("{relation}_{col}_check"),
                (None, [col]) => format!("{relation}_{col}_check"),
                _ => format!("{relation}_check"),
            });
            let is_enum = given.is_none()
                && own.is_some_and(|col| {
                    let toks = ddl_tokens(&expr);
                    toks.len() >= 3
                        && eq_fold(&toks[0].text, col)
                        && toks[1].word
                        && eq_fold(&toks[1].text, "in")
                        && toks[2..].iter().all(|t| !t.word)
                });
            let cols: Vec<String> = read.iter().map(|c| json_quote(c)).collect();
            out.push(format!(
                r#"{{"name":{},"cols":[{}],"expr":{},"enum":{},"notvalid":{}}}"#,
                json_quote(&name),
                cols.join(","),
                json_quote(&expr),
                is_enum,
                not_valid
            ));
        }
    }
    format!("[{}]", out.join(","))
}

/// The mark an `ADD CONSTRAINT ... CHECK (...) NOT VALID` leaves after its CHECK: the
/// rows from before it were not checked, and VALIDATE CONSTRAINT takes it away.
pub const NOT_VALID_MARK: &str = "/*overlite not valid*/";

/// The CHECKs written in one item of a column list: the name each is given, its
/// expression, and whether it carries NOT_VALID_MARK.
fn checks_in(text: &str) -> Vec<(Option<String>, String, bool)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut depth) = (0, 0);
    let mut last_words: Vec<String> = Vec::new();
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let q = b[i];
                let start = i;
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                if q != b'\'' && depth == 0 {
                    last_words.push(unquote_sql_ident(&text[start..(i + 1).min(b.len())]));
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            c if depth == 0 && (c.is_ascii_alphabetic() || c == b'_') => {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                    i += 1;
                }
                let word = &text[start..i];
                if eq_fold(word, "check") {
                    let open = text[i..].find('(').map(|o| i + o);
                    let close = open.and_then(|open| {
                        let mut d = 0;
                        let mut k = open;
                        while k < b.len() {
                            match b[k] {
                                b'\'' => {
                                    k += 1;
                                    while k < b.len() && b[k] != b'\'' {
                                        k += 1;
                                    }
                                }
                                b'(' => d += 1,
                                b')' => {
                                    d -= 1;
                                    if d == 0 {
                                        return Some(k);
                                    }
                                }
                                _ => {}
                            }
                            k += 1;
                        }
                        None
                    });
                    if let (Some(open), Some(close)) = (open, close) {
                        let n = last_words.len();
                        let given =
                            (n >= 2 && eq_fold(&last_words[n - 2], "constraint")).then(|| last_words[n - 1].clone());
                        let not_valid = text[close + 1..].trim_start().starts_with(NOT_VALID_MARK);
                        out.push((given, text[open + 1..close].trim().to_string(), not_valid));
                        i = close + 1;
                        last_words.clear();
                        continue;
                    }
                }
                last_words.push(word.to_string());
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// The items of a CREATE TABLE's column list with the first name of each: a column
/// and its definition, or a table constraint and its first word.
fn column_items(ddl: &str) -> Vec<(String, &str)> {
    let b = ddl.as_bytes();
    let Some(open) = ddl.find('(') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let (mut depth, mut start, mut i) = (1, open + 1, open + 1);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += ddl[i..].find("*/").map_or(b.len() - i, |e| e + 1);
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i += ddl[i..].find('\n').unwrap_or(b.len() - i);
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    out.push(named_item(&ddl[start..i]));
                    break;
                }
            }
            b',' if depth == 1 => {
                out.push(named_item(&ddl[start..i]));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

lazy_regex!(RE_OPCLASS_COMMENT, r"/\*\s*(\w+)\s*\*/");

/// The operator class the CREATE INDEX in sql gave its column at seqno, which the
/// rewrite keeps in a comment after the column (`name /*varchar_pattern_ops*/`), or
/// None when it gave the default.
pub(crate) fn index_opclass(sql: &str, seqno: i64) -> Option<String> {
    if !sql.contains("/*") {
        return None;
    }
    RE_OPCLASS_COMMENT
        .captures(index_item(sql, seqno)?)
        .map(|m| m[1].to_string())
}

/// The column a key of a NULLS NOT DISTINCT index reads: the rewrite indexes
/// `ifnull(col, <blob>)`, an expression to SQLite.
pub(crate) fn index_null_key_column(sql: &str, seqno: i64) -> Option<String> {
    if !sql.contains("/*overlite nulls not distinct*/") {
        return None;
    }
    let m = RE_NULL_KEY.captures(index_item(sql, seqno)?)?;
    Some(unquote_sql_ident(&m[1]))
}

lazy_regex!(
    RE_NULL_KEY,
    r#"(?i)^\s*ifnull\(\s*("(?:[^"]|"")+"|[\w$]+)\s*,\s*x'6f7665726c6974652d6e756c6c'\s*\)"#
);

/// The text of the item at seqno of a CREATE INDEX's column list.
fn index_item(sql: &str, seqno: i64) -> Option<&str> {
    let b = sql.as_bytes();
    let on = sql.to_ascii_lowercase().find(" on ")?;
    let open = on + sql[on..].find('(')?;
    // The column list, split at its top-level commas, past quotes and comments.
    let (mut depth, mut item, mut start) = (0usize, 0i64, open + 1);
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += sql[i..].find("*/").map_or(b.len() - i, |e| e + 1);
            }
            b'(' => depth += 1,
            b')' | b',' if depth == 1 => {
                if item == seqno {
                    return Some(&sql[start..i]);
                }
                if b[i] == b')' {
                    return None;
                }
                item += 1;
                start = i + 1;
            }
            b')' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// What the rewrite writes before a covering index's INCLUDE columns, at the end of
/// its CREATE INDEX.
const INCLUDE_MARK: &str = "/*overlite include ";

/// The INCLUDE columns of a covering index, from its CREATE INDEX: the list as
/// written, and the names unquoted.
pub(crate) fn index_include(sql: &str) -> Option<(String, Vec<String>)> {
    let i = sql.rfind(INCLUDE_MARK)? + INCLUDE_MARK.len();
    let list = sql[i..].split("*/").next()?.trim();
    Some((list.to_string(), ident_list(list)))
}

/// The predicate of a partial index, from its CREATE INDEX: what follows the column
/// list's WHERE.
pub(crate) fn index_where(sql: &str) -> Option<String> {
    let sql = sql.find("/*overlite ").map_or(sql, |i| sql[..i].trim_end());
    let on = sql.to_ascii_lowercase().find(" on ")?;
    let open = on + sql[on..].find('(')?;
    let b = sql.as_bytes();
    let (mut depth, mut i) = (0, open);
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    let rest = sql.get(i + 1..)?.trim();
    let cond = rest
        .get(..5)
        .filter(|w| w.eq_ignore_ascii_case("where"))
        .map(|_| rest[5..].trim())?;
    Some(format!("({})", cond.trim_end_matches(';').trim()))
}

/// The oid of the btree operator class Postgres gives an index column of the
/// declared type by default, as pg_opclass lists it.
pub(crate) fn default_opclass_oid(decl: &str) -> i64 {
    match sqlite_type_oid(decl) {
        catalog::TYPE_OID_BOOL => 10003,
        catalog::TYPE_OID_BYTEA => 10006,
        catalog::TYPE_OID_DATE => 3122,
        catalog::TYPE_OID_FLOAT8 => 3123,
        catalog::TYPE_OID_INT2 => 1979,
        catalog::TYPE_OID_INT4 => 1978,
        catalog::TYPE_OID_INT8 => 3124,
        catalog::TYPE_OID_NUMERIC => 3125,
        catalog::TYPE_OID_TIMESTAMPTZ => 3127,
        catalog::TYPE_OID_TIMESTAMP => 3128,
        catalog::TYPE_OID_UUID => 10065,
        catalog::TYPE_OID_JSONB => 10088,
        _ => 3126, // text, varchar, and json, which has no btree class of its own
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_keys_in_declaration_order() {
        let ddl = r#"CREATE TABLE "r14" ("id" INTEGER PRIMARY KEY AUTOINCREMENT, "x" integer CONSTRAINT "r14 x" REFERENCES r14p(id) DEFERRABLE INITIALLY DEFERRED, y int REFERENCES main.r14p(code) ON DELETE CASCADE NOT DEFERRABLE, CONSTRAINT "r14_tbl_fk" FOREIGN KEY (y, "x") REFERENCES "r14p" (code, id) DEFERRABLE, FOREIGN KEY (y) REFERENCES r14p)"#;
        let got = fk_clauses(ddl);
        let names: Vec<_> = got.iter().map(|c| c.name.clone()).collect();
        assert_eq!(
            names,
            [Some("r14 x".to_string()), None, Some("r14_tbl_fk".to_string()), None]
        );
        assert_eq!(got[2].cols, ["y", "x"]);
        assert_eq!(got[1].ref_table, "r14p");
        let deferral: Vec<_> = got.iter().map(|c| (c.deferrable, c.deferred)).collect();
        assert_eq!(deferral, [(true, true), (false, false), (true, false), (false, false)]);
        let y = vec!["y".to_string()];
        assert_eq!(fk_constraint_name(ddl, "r14", &y, "r14p", 0), "r14_y_fkey");
        assert_eq!(fk_constraint_name(ddl, "r14", &y, "r14p", 2), "r14_y_fkey");
        assert_eq!(fk_constraint_name(ddl, "r14", &["x".to_string()], "r14p", 3), "r14 x");
        assert_eq!(fk_deferral(ddl, &["x".to_string()], "r14p", 3), 2);
    }

    #[test]
    fn unique_names() {
        let ddl = r#"CREATE TABLE t (a int CONSTRAINT "a one" UNIQUE, b int UNIQUE, c int, d int, CONSTRAINT t_c_d_uniq UNIQUE (c, "d"), UNIQUE (d, c))"#;
        let name = |cols: &[&str]| {
            let cols: Vec<String> = cols.iter().map(ToString::to_string).collect();
            unique_constraint_name(ddl, "t", &cols)
        };
        assert_eq!(name(&["a"]), "a one");
        assert_eq!(name(&["b"]), "t_b_key");
        assert_eq!(name(&["c", "d"]), "t_c_d_uniq");
        assert_eq!(name(&["d", "c"]), "t_d_c_key");
    }

    #[test]
    fn checks() {
        let ddl = r#"CREATE TABLE t ("pos" integer NOT NULL CHECK ("pos" >= 0), s text CHECK (s IN ('a', 'b')), a int, b int CONSTRAINT "b pos" CHECK (b > 0), CONSTRAINT price_ok CHECK (a > b), CHECK (a < 10), CHECK (1 = 1))"#;
        // json_quote writes < and > as \u003c and \u003e.
        assert_eq!(
            check_constraints(ddl, "t"),
            concat!(
                r#"[{"name":"t_pos_check","cols":["pos"],"expr":"\"pos\" \u003e= 0","enum":false,"notvalid":false},"#,
                r#"{"name":"t_s_check","cols":["s"],"expr":"s IN ('a', 'b')","enum":true,"notvalid":false},"#,
                r#"{"name":"b pos","cols":["b"],"expr":"b \u003e 0","enum":false,"notvalid":false},"#,
                r#"{"name":"price_ok","cols":["a","b"],"expr":"a \u003e b","enum":false,"notvalid":false},"#,
                r#"{"name":"t_a_check","cols":["a"],"expr":"a \u003c 10","enum":false,"notvalid":false},"#,
                r#"{"name":"t_check","cols":[],"expr":"1 = 1","enum":false,"notvalid":false}]"#
            )
        );
    }

    #[test]
    fn identity_marks() {
        let ddl = r#"CREATE TABLE t ("id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT /*overlite identity always bigint*/, a text DEFAULT 'x,/*overlite identity always int*/', "b c" int NOT NULL /*overlite identity by default integer*/, CHECK (a <> ','))"#;
        let got = identity_columns(ddl);
        assert_eq!(
            got,
            [
                Identity {
                    column: "id".into(),
                    always: true,
                    type_name: "bigint".into()
                },
                Identity {
                    column: "b c".into(),
                    always: false,
                    type_name: "integer".into()
                },
            ]
        );
        assert_eq!(attidentity(ddl, "ID"), "a");
        assert_eq!(attidentity(ddl, "a"), "");
        assert_eq!(identity_type(ddl, "b c").as_deref(), Some("integer"));
    }
}
