//! A boolean column may hold text. The Go server stored a boolean bound as text as
//! that text, so a BOOLEAN column of a database it wrote holds 'true' and 'false' (or
//! 't', 'yes'...) beside the 1 and 0 this server writes. SQLite compares them as what
//! they are: `b = true` met only the integers, and `WHERE b` read every word as 0.
//! Stored data is not rewritten (ARCHITECTURE.md), so each reading of such a column
//! is made to read both:
//!
//! - `b = <value>` and `b <> <value>`, with a literal, a parameter or a cast to
//!   boolean, become `b IN (<forms>)` and `b NOT IN (<forms>)`, the forms being 1 or 0
//!   and the spellings of the same truth value in TRUE_FORMS or FALSE_FORMS. SQLite
//!   searches an index on b for each, so `col = $1` and `col = true` keep their plan.
//!   A parameter's forms come from overlite_bool_forms, as a list SQLite reads once.
//! - Any other reading (`WHERE b`, `NOT b`, `IS TRUE`, `CASE WHEN b`, an argument, a
//!   grouping or an order) reads `overlite_bool(b)`: 1, 0 or NULL, whatever the
//!   spelling, as Postgres' boolin reads it.
//! - A column that is a whole item of a select list or of RETURNING is left as it
//!   is: its declared type makes it a boolean on the way out, whatever it holds.
//!
//! bool_and, bool_or and every are min and max over overlite_bool.
//!
//! Which names are boolean columns comes from the schema, kept per database until
//! its schema_version moves. A name is read as one when the table it qualifies, or
//! every table of the statement that has a column of that name, declares it boolean.

use crate::column_types::select_list;
use crate::db_state::DbState;
use crate::resolve::names::{
    FromTable, Kind, Tok, after_parens, by_qualifier, from_tables, inside_run, name_run, tokens,
};
use overlite_core::parse_bool;
use overlite_core::sync::RwLockExt;
use rusqlite::Connection;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The text a boolean column may hold for true, besides 1: what Postgres' boolin
/// reads as true, in lower case, upper case and capitalized. Integers aside, these
/// are what a client sends, so what the Go server stored.
pub(crate) const TRUE_FORMS: &[&str] = &[
    "t", "true", "y", "yes", "on", "T", "TRUE", "Y", "YES", "ON", "True", "Yes", "On",
];
/// The same for false, besides 0.
pub(crate) const FALSE_FORMS: &[&str] = &[
    "f", "false", "n", "no", "off", "F", "FALSE", "N", "NO", "OFF", "False", "No", "Off",
];

/// The boolean columns of a database, and every column of the tables that have one.
#[derive(Debug, Default)]
pub(crate) struct BoolColumns {
    /// PRAGMA schema_version when this was read.
    version: i64,
    /// Lowercase table name to lowercase column name to what it is, for the tables
    /// that have a column named like some boolean column.
    tables: HashMap<String, HashMap<String, Mode>>,
    /// The lowercase names of the boolean columns.
    names: HashSet<String>,
    /// Whether some boolean column is mixed.
    any_mixed: bool,
}

/// What a column is to the readings of a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mode {
    /// Not a boolean.
    Other,
    /// A boolean stored as 1 and 0 alone, as the storage format has it: read as it is,
    /// but for a string compared with it, which is read as a boolean.
    Canonical,
    /// A boolean that may hold text too: a mixed column, or any in a file not in the
    /// storage format.
    Mixed,
}

impl BoolColumns {
    fn read(q: &Connection) -> BoolColumns {
        let mut out = BoolColumns {
            version: schema_version(q),
            ..Default::default()
        };
        // The tables' columns in one statement, as a migration's next statement after
        // each CREATE TABLE reads them again. A view is read on its own: one that no
        // longer compiles fails pragma_table_info, and has no columns to read anyway.
        let user = |p: &str| {
            format!(r"{p}name NOT LIKE 'sqlite\_%' ESCAPE '\' AND {p}name NOT LIKE '\_overlite\_%' ESCAPE '\'")
        };
        let mut rows: Vec<(String, String, String)> = Vec::new();
        let tables = format!(
            "SELECT m.name, p.name, p.type FROM sqlite_master AS m, pragma_table_info(m.name) AS p
             WHERE m.type = 'table' AND {}",
            user("m.")
        );
        if let Ok(mut st) = q.prepare(&tables)
            && let Ok(it) = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        {
            rows.extend(it.filter_map(std::result::Result::ok));
        }
        let views: Vec<String> = q
            .prepare(&format!(
                "SELECT name FROM sqlite_master WHERE type = 'view' AND {}",
                user("")
            ))
            .and_then(|mut st| st.query_map([], |r| r.get(0))?.collect())
            .unwrap_or_default();
        let view_names: HashSet<String> = views.iter().map(|v| v.to_lowercase()).collect();
        for view in views {
            if let Ok(mut st) = q.prepare("SELECT name, type FROM pragma_table_info(?1)")
                && let Ok(it) = st.query_map([&view], |r| Ok((view.clone(), r.get(0)?, r.get(1)?)))
            {
                rows.extend(it.filter_map(std::result::Result::ok));
            }
        }
        // In a file in the storage format, a boolean holds 1 and 0 alone unless its
        // column is listed mixed. Before it, any may hold text.
        let formatted = crate::storage::current_version(q) >= crate::storage::FORMAT_VERSION;
        let mixed: HashSet<(String, String)> = crate::storage::mixed_columns(q).into_iter().collect();
        let mut all: HashMap<String, HashMap<String, Mode>> = HashMap::new();
        for (table, col, decl) in rows {
            let col = col.to_lowercase();
            let table = table.to_lowercase();
            // A view's column may read a mixed one: with any, its booleans keep both.
            let view_of_mixed = view_names.contains(&table) && !mixed.is_empty();
            let listed = mixed.contains(&(table.clone(), col.clone()));
            let mode = match (is_bool_decl(&decl), formatted && !listed && !view_of_mixed) {
                (false, _) => Mode::Other,
                (true, true) => Mode::Canonical,
                (true, false) => Mode::Mixed,
            };
            if mode != Mode::Other {
                out.names.insert(col.clone());
                out.any_mixed |= mode == Mode::Mixed;
            }
            all.entry(table).or_default().insert(col, mode);
        }
        out.tables = all
            .into_iter()
            .filter(|(_, cols)| cols.keys().any(|c| out.names.contains(c)))
            .collect();
        out
    }

    /// Whether table has column col, and if so whether it is boolean. A table that
    /// has no column named like a boolean one is not kept, and answers None.
    fn column(&self, table: &str, col: &str) -> Option<Mode> {
        self.tables.get(table)?.get(col).copied()
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

fn is_bool_decl(decl: &str) -> bool {
    let d = decl.trim();
    d.eq_ignore_ascii_case("boolean") || d.eq_ignore_ascii_case("bool")
}

fn schema_version(q: &Connection) -> i64 {
    q.prepare_cached("PRAGMA schema_version")
        .and_then(|mut s| s.query_row([], |r| r.get(0)))
        .unwrap_or(-1)
}

impl DbState {
    /// The boolean columns, read once and kept until fresh is asked for and the
    /// schema has moved.
    fn bool_columns(&self, q: &Connection, fresh: bool) -> Arc<BoolColumns> {
        let kept = self.bools.read_locked().clone();
        if let Some(cols) = kept
            && (!fresh || cols.version == schema_version(q))
        {
            return cols;
        }
        let cols = Arc::new(BoolColumns::read(q));
        *self.bools.write_locked() = Some(cols.clone());
        cols
    }

    /// Drops what bool_columns kept, after a statement that may change the schema.
    pub(crate) fn forget_bool_columns(&self) {
        *self.bools.write_locked() = None;
    }
}

const AGGREGATES: &[&str] = &["bool_and", "bool_or", "every"];

static NO_NAMES: std::sync::LazyLock<HashSet<String>> = std::sync::LazyLock::new(HashSet::new);

/// Reports whether a word of sql, case ignored, is one of names or, with
/// aggregates, a boolean aggregate. One pass and no allocation: it runs on every
/// statement of a database that has a boolean column.
fn mentions(sql: &str, names: &HashSet<String>, aggregates: bool) -> bool {
    let b = sql.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let mut buf = [0u8; 64];
    let mut i = 0;
    while i < b.len() {
        if !is_word(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_word(b[i]) {
            i += 1;
        }
        let w = &b[start..i];
        if w.len() > buf.len() {
            continue;
        }
        let lower = &mut buf[..w.len()];
        lower.copy_from_slice(w);
        lower.make_ascii_lowercase();
        let Ok(word) = std::str::from_utf8(lower) else { continue };
        if names.contains(word) || (aggregates && AGGREGATES.contains(&word)) {
            return true;
        }
    }
    false
}

/// Rewrites what reads a boolean column so that 'true' and 1 read alike.
pub(crate) fn resolve_booleans<'a>(st: &DbState, q: &Connection, sql: &'a str) -> Cow<'a, str> {
    if !reads_rows(sql) {
        return Cow::Borrowed(sql);
    }
    let mut cols = st.bool_columns(q, false);
    // A column in the storage format is read as it is, but for a string compared with
    // it: with no string and no boolean aggregate there is nothing to do, and a lookup
    // keeps its plain `col = $1`.
    let names = if cols.any_mixed || sql.contains('\'') {
        &cols.names
    } else {
        &NO_NAMES
    };
    if !mentions(sql, names, true) {
        return Cow::Borrowed(sql);
    }
    if !cols.is_empty() && mentions(sql, &cols.names, false) {
        // The schema may have moved since; a name seen is worth one PRAGMA.
        cols = st.bool_columns(q, true);
    }
    match Rewriter::new(sql, &cols).run() {
        Some(out) => Cow::Owned(out),
        None => Cow::Borrowed(sql),
    }
}

/// A statement that reads rows: the only ones whose booleans are compared.
fn reads_rows(sql: &str) -> bool {
    let mut words = sql.split_ascii_whitespace().map(str::to_ascii_lowercase);
    let mut w = words.next().unwrap_or_default();
    if w == "explain" {
        w = words.next().unwrap_or_default();
        if w == "query" {
            words.next();
            w = words.next().unwrap_or_default();
        }
    }
    let w = w.trim_start_matches('(');
    ["select", "with", "update", "delete", "insert", "values"]
        .iter()
        .any(|k| w.starts_with(k))
}

/// Words that bind an operand tighter than a comparison does, or go on with it.
const OPERATOR_WORDS: &[&str] = &[
    "is", "in", "not", "like", "ilike", "between", "collate", "glob", "regexp", "match", "isnull", "notnull", "escape",
    "similar",
];

/// The clause keywords a statement is read by.
const CLAUSES: &[&str] = &[
    "select",
    "from",
    "where",
    "set",
    "values",
    "returning",
    "group",
    "order",
    "having",
    "on",
    "using",
    "limit",
    "offset",
    "window",
    "partition",
    "conflict",
    "into",
];

/// What a column is compared with.
enum Operand {
    /// A truth value known now.
    Const(bool),
    /// An expression, whose truth value is known when the statement runs.
    Expr(String),
}

struct Rewriter<'a> {
    sql: &'a str,
    toks: Vec<Tok>,
    cols: &'a BoolColumns,
    tables: Vec<FromTable>,
    qualifiers: HashMap<String, usize>,
    /// Names a select list gives to something other than a boolean column.
    labels: HashSet<String>,
    edits: Vec<(usize, usize, String)>,
}

impl<'a> Rewriter<'a> {
    fn new(sql: &'a str, cols: &'a BoolColumns) -> Rewriter<'a> {
        let toks = tokens(sql);
        let tables = from_tables(sql, &toks);
        let qualifiers = by_qualifier(&tables);
        Rewriter {
            sql,
            toks,
            cols,
            tables,
            qualifiers,
            labels: HashSet::new(),
            edits: Vec::new(),
        }
    }

    fn tok(&self, i: usize) -> Option<&Tok> {
        self.toks.get(i)
    }

    fn word_at(&self, i: usize, w: &str) -> bool {
        self.tok(i).is_some_and(|t| t.is_word(self.sql, w))
    }

    fn punct_at(&self, i: usize, set: &[&str]) -> bool {
        self.tok(i)
            .is_some_and(|t| t.kind == Kind::Punct && set.contains(&t.text(self.sql)))
    }

    fn text(&self, from: usize, to: usize) -> &'a str {
        &self.sql[self.toks[from].start..self.toks[to - 1].end]
    }

    /// Whether the column `parts` names is a boolean one.
    fn is_bool(&self, parts: &[String]) -> bool {
        self.bool_mode(parts) != Mode::Other
    }

    /// What the column `parts` names is.
    fn bool_mode(&self, parts: &[String]) -> Mode {
        let Some((col, qual)) = parts.split_last() else {
            return Mode::Other;
        };
        if !self.cols.names.contains(col) {
            return Mode::Other;
        }
        if !qual.is_empty() {
            let key = qual.join(".");
            let named = self
                .qualifiers
                .get(&key)
                .or_else(|| qual.last().and_then(|q| self.qualifiers.get(q)));
            if let Some(&i) = named {
                return self.table_column(&self.tables[i], col).unwrap_or(Mode::Other);
            }
            // A CTE, a derived table or `excluded`: what it reads decides.
        } else if self.labels.contains(col) {
            return Mode::Other;
        }
        let mut found = Mode::Other;
        for t in &self.tables {
            match self.table_column(t, col) {
                Some(Mode::Other) => return Mode::Other,
                Some(m) => found = found.max(m),
                None => {}
            }
        }
        found
    }

    fn table_column(&self, t: &FromTable, col: &str) -> Option<Mode> {
        self.cols
            .column(&t.parts.join("."), col)
            .or_else(|| t.parts.last().and_then(|last| self.cols.column(last, col)))
    }

    /// A column reference starting at token i: its parts and the index past it.
    /// Not a function's name, nor the middle of a dotted name.
    fn column_at(&self, i: usize) -> Option<(Vec<String>, usize)> {
        if inside_run(self.sql, &self.toks, i) {
            return None;
        }
        let (parts, end) = name_run(self.sql, &self.toks, i)?;
        if self.punct_at(end, &["("]) {
            return None;
        }
        Some((parts, end))
    }

    fn run(mut self) -> Option<String> {
        self.collect_labels();
        let mut clause: Vec<&str> = vec![""];
        let mut skip_to = 0;
        let mut i = 0;
        while i < self.toks.len() {
            let t = self.toks[i];
            if t.kind == Kind::Punct {
                match t.text(self.sql) {
                    "(" => {
                        // INSERT's column list, ON CONFLICT's target and USING's columns name
                        // columns; they read nothing.
                        let names_columns = i > 0
                            && (self.word_at(i - 1, "conflict") || self.word_at(i - 1, "using") || self.after_into(i));
                        if names_columns && let Some(after) = after_parens(&self.toks, i) {
                            skip_to = skip_to.max(after);
                        }
                        clause.push("");
                    }
                    ")" => {
                        clause.pop();
                        if clause.is_empty() {
                            clause.push("");
                        }
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }
            if t.kind == Kind::Word {
                let w = t.text(self.sql).to_ascii_lowercase();
                if let Some(c) = CLAUSES.iter().find(|c| **c == w)
                    && let Some(top) = clause.last_mut()
                {
                    *top = c;
                }
                if AGGREGATES.contains(&w.as_str()) && self.punct_at(i + 1, &["("]) {
                    self.aggregate(i);
                    i += 1;
                    continue;
                }
            }
            if i < skip_to {
                i += 1;
                continue;
            }
            let Some((parts, end)) = self.column_at(i) else {
                i += 1;
                continue;
            };
            match self.bool_mode(&parts) {
                Mode::Mixed => self.column(i, end, clause.last().copied().unwrap_or("")),
                Mode::Canonical => self.strings_compared(i, end),
                Mode::Other => {}
            }
            i = end;
        }
        if self.edits.is_empty() {
            return None;
        }
        self.keep_labels();
        self.edits.sort_by_key(|e| e.0);
        let mut out = String::with_capacity(self.sql.len() + 64 * self.edits.len());
        let mut at = 0;
        for (start, end, text) in &self.edits {
            if *start < at {
                continue; // overlaps one already made
            }
            out.push_str(&self.sql[at..*start]);
            out.push_str(text);
            at = *end;
        }
        out.push_str(&self.sql[at..]);
        Some(out)
    }

    /// Whether the parenthesis at token open follows INSERT's target, `INTO t`,
    /// `INTO s.t` or `INTO t AS a`: its column list.
    fn after_into(&self, open: usize) -> bool {
        let mut k = open;
        if k >= 2 && self.word_at(k - 2, "as") {
            k -= 2;
        }
        if k == 0 || !matches!(self.toks[k - 1].kind, Kind::Word | Kind::Quoted) {
            return false;
        }
        k -= 1;
        while k >= 2 && self.punct_at(k - 1, &["."]) {
            k -= 2;
        }
        k >= 1 && self.word_at(k - 1, "into")
    }

    /// SQLite labels an unnamed result column with the text of its expression, so a
    /// rewritten item of the outermost select list is named after what it was.
    fn keep_labels(&mut self) {
        let Some((start, end)) = select_list(self.sql) else {
            return;
        };
        let inside: Vec<usize> = (0..self.toks.len())
            .filter(|&k| self.toks[k].start >= start && self.toks[k].end <= end)
            .collect();
        let Some(&first) = inside.first() else { return };
        let depth = self.toks[first].depth;
        let mut items: Vec<(usize, usize)> = Vec::new();
        let mut from = first;
        for &k in &inside {
            if self.toks[k].depth == depth && self.toks[k].is_punct(self.sql, ",") {
                items.push((from, k));
                from = k + 1;
            }
        }
        items.push((from, inside[inside.len() - 1] + 1));
        for (a, b) in items {
            if a >= b {
                continue;
            }
            let (s, e) = (self.toks[a].start, self.toks[b - 1].end);
            if !self.edits.iter().any(|(es, ee, _)| *es >= s && *ee <= e) || self.labelled(a, b) {
                continue;
            }
            let label = self.sql[s..e].replace('"', "\"\"");
            self.edits.push((e, e, format!(" AS \"{label}\"")));
        }
    }

    /// Whether the select list item at tokens a..b names itself, with AS or without.
    fn labelled(&self, a: usize, b: usize) -> bool {
        let sql = self.sql;
        if b - a < 2 {
            return false;
        }
        let (last, prev) = (self.toks[b - 1], self.toks[b - 2]);
        if prev.is_word(sql, "as") {
            return true;
        }
        let keyword = |t: &Tok| {
            ["end", "true", "false", "null", "unknown"]
                .iter()
                .any(|w| t.is_word(sql, w))
        };
        let joins = |t: &Tok| {
            [
                "not", "and", "or", "is", "in", "like", "ilike", "between", "then", "else", "when", "case", "distinct",
                "all", "select",
            ]
            .iter()
            .any(|w| t.is_word(sql, w))
        };
        matches!(last.kind, Kind::Word | Kind::Quoted)
            && !keyword(&last)
            && match prev.kind {
                Kind::Punct => prev.is_punct(sql, ")"),
                Kind::Word => !joins(&prev),
                _ => true,
            }
    }

    /// The names after AS that label something other than a boolean column: a bare
    /// reference to one of them is that something, not the column.
    fn collect_labels(&mut self) {
        for i in 1..self.toks.len() {
            if !self.word_at(i, "as") || i + 1 >= self.toks.len() {
                continue;
            }
            let label = &self.toks[i + 1];
            if !matches!(label.kind, Kind::Word | Kind::Quoted) {
                continue;
            }
            let name = crate::resolve::names::ident(self.sql, label);
            if !self.cols.names.contains(&name) {
                continue;
            }
            // What stands before AS: a bare or dotted name of a boolean column keeps
            // the label boolean; a table's alias is no label.
            let mut first = i - 1;
            while first >= 2 && self.punct_at(first - 1, &["."]) {
                first -= 2;
            }
            let before = name_run(self.sql, &self.toks, first).filter(|(_, end)| *end == i);
            let is_table = before
                .as_ref()
                .is_some_and(|(parts, _)| self.tables.iter().any(|t| &t.parts == parts));
            let is_bool_column = before.as_ref().is_some_and(|(parts, _)| self.is_bool(parts));
            if !is_table && !is_bool_column {
                self.labels.insert(name);
            }
        }
    }

    /// bool_and(x) is pg_bool(min(overlite_bool(x))), and bool_or max.
    fn aggregate(&mut self, i: usize) {
        let Some(after) = after_parens(&self.toks, i + 1) else {
            return;
        };
        let f = if self.word_at(i, "bool_or") { "max" } else { "min" };
        let t = self.toks[i];
        let close = self.toks[after - 1];
        self.edits
            .push((t.start, self.toks[i + 1].end, format!("pg_bool({f}(overlite_bool(")));
        self.edits.push((close.start, close.end, ")))".to_string()));
    }

    /// Rewrites the boolean column at tokens i..end.
    fn column(&mut self, i: usize, end: usize, clause: &str) {
        let sql = self.sql;
        let col = self.text(i, end);
        let prev = i.checked_sub(1).map(|p| self.toks[p]);
        let next = self.tok(end).copied();
        let next_word = |w: &str| next.is_some_and(|t| t.is_word(sql, w));
        if prev.is_some_and(|p| p.is_word(sql, "as")) {
            return;
        }
        // The target of SET.
        if clause == "set"
            && prev.is_some_and(|p| p.is_word(sql, "set") || p.is_punct(sql, ",") || p.is_punct(sql, "("))
            && next.is_some_and(|n| n.is_punct(sql, "=") || n.is_punct(sql, ",") || n.is_punct(sql, ")"))
        {
            return;
        }
        // A whole item of a select list goes out as it is.
        if (clause == "select" || clause == "returning")
            && prev.is_some_and(|p| {
                p.is_word(sql, "select")
                    || p.is_word(sql, "distinct")
                    || p.is_word(sql, "all")
                    || p.is_word(sql, "returning")
                    || p.is_punct(sql, ",")
            })
            && next.is_none_or(|n| match n.kind {
                Kind::Punct => [",", ")", ";"].contains(&n.text(sql)),
                Kind::Word => !OPERATOR_WORDS.contains(&n.text(sql).to_ascii_lowercase().as_str()),
                Kind::Quoted => true,
                _ => false,
            })
        {
            return;
        }
        // IS [NOT] NULL reads no truth value, and keeps an index.
        if next_word("is")
            && (self.word_at(end + 1, "null") || (self.word_at(end + 1, "not") && self.word_at(end + 2, "null")))
        {
            return;
        }
        let prev_free = prev.is_none_or(|p| match p.kind {
            Kind::Punct => ["(", ","].contains(&p.text(sql)),
            Kind::Word => !OPERATOR_WORDS.contains(&p.text(sql).to_ascii_lowercase().as_str()) || p.is_word(sql, "not"),
            _ => false,
        });
        // col = value, col <> value
        if prev_free && self.punct_at(end, &["=", "==", "<>", "!="]) {
            let negated = self.punct_at(end, &["<>", "!="]);
            if let Some((operand, after)) = self.operand(end + 1)
                && self.ends_operand(after)
            {
                let text = in_forms(col, negated, &operand);
                self.edits.push((self.toks[i].start, self.toks[after - 1].end, text));
                return;
            }
        }
        // value = col
        if i >= 2 && self.punct_at(i - 1, &["=", "==", "<>", "!="]) && self.ends_operand(end) {
            let negated = self.punct_at(i - 1, &["<>", "!="]);
            if let Some(start) =
                (i.saturating_sub(8)..i - 1).find(|&s| self.operand(s).is_some_and(|(_, a)| a == i - 1))
                && let Some((operand, _)) = self.operand(start)
                && start.checked_sub(1).is_none_or(|p| {
                    let p = self.toks[p];
                    match p.kind {
                        Kind::Punct => ["(", ","].contains(&p.text(sql)),
                        Kind::Word => {
                            !OPERATOR_WORDS.contains(&p.text(sql).to_ascii_lowercase().as_str())
                                || p.is_word(sql, "not")
                        }
                        _ => false,
                    }
                })
            {
                let text = in_forms(col, negated, &operand);
                self.edits.push((self.toks[start].start, self.toks[end - 1].end, text));
                return;
            }
        }
        // col [NOT] IN (values): the column read as a boolean, and so the values.
        let in_at = if next_word("in") {
            Some(end + 1)
        } else if next_word("not") && self.word_at(end + 1, "in") {
            Some(end + 2)
        } else {
            None
        };
        if let Some(open) = in_at.filter(|o| self.punct_at(*o, &["("]))
            && !(self.word_at(open + 1, "select") || self.word_at(open + 1, "with") || self.word_at(open + 1, "values"))
            && let Some(after) = after_parens(&self.toks, open)
        {
            let mut k = open + 1;
            while k < after - 1 {
                if let Some((operand, a)) = self.operand(k)
                    && self.punct_at(a, &[",", ")"])
                {
                    let value = match operand {
                        Operand::Const(b) => i64::from(b).to_string(),
                        Operand::Expr(e) => format!("pg_bool({e})"),
                    };
                    self.edits.push((self.toks[k].start, self.toks[a - 1].end, value));
                    k = a + 1;
                } else {
                    k += 1;
                }
            }
        }
        self.edits.push((
            self.toks[i].start,
            self.toks[end - 1].end,
            format!("overlite_bool({col})"),
        ));
    }

    /// A column in the storage format holds 1 and 0, which SQLite compares as they are:
    /// only a string compared with it needs reading, as Postgres reads it as a boolean.
    /// `col = 't'`, `'f' <> col` and `col IN ('t', 'f')` take 1 or 0 for the string, and
    /// one no boolean reads as is pg_bool of it, 22P02. Nothing else changes, so the
    /// column keeps its index and a parameter its plain `col = $1`.
    fn strings_compared(&mut self, i: usize, end: usize) {
        let mut at: Vec<usize> = Vec::new();
        if self.punct_at(end, &["=", "==", "<>", "!="]) && self.ends_operand(end + 2) {
            at.push(end + 1);
        }
        if i >= 2 && self.punct_at(i - 1, &["=", "==", "<>", "!="]) && self.ends_operand(end) {
            at.push(i - 2);
        }
        let in_at = if self.word_at(end, "in") {
            Some(end + 1)
        } else if self.word_at(end, "not") && self.word_at(end + 1, "in") {
            Some(end + 2)
        } else {
            None
        };
        if let Some(open) = in_at.filter(|o| self.punct_at(*o, &["("]))
            && let Some(after) = after_parens(&self.toks, open)
        {
            at.extend(
                (open + 1..after - 1)
                    .filter(|&k| self.punct_at(k - 1, &["(", ","]) && self.punct_at(k + 1, &[",", ")"])),
            );
        }
        for k in at {
            let Some(t) = self.tok(k).copied() else { continue };
            let text = t.text(self.sql);
            if t.kind != Kind::Str || !text.starts_with('\'') || text.len() < 2 {
                continue;
            }
            let value = match parse_bool(&text[1..text.len() - 1].replace("''", "'")) {
                Some(b) => i64::from(b).to_string(),
                None => format!("pg_bool({text})"),
            };
            self.edits.push((t.start, t.end, value));
        }
    }

    /// A value a boolean column can be compared with at token k, and the index past it.
    fn operand(&self, k: usize) -> Option<(Operand, usize)> {
        let sql = self.sql;
        let t = self.tok(k)?;
        let text = t.text(sql);
        match t.kind {
            Kind::Word if t.is_word(sql, "true") => Some((Operand::Const(true), k + 1)),
            Kind::Word if t.is_word(sql, "false") => Some((Operand::Const(false), k + 1)),
            Kind::Num if text == "1" || text == "0" => Some((Operand::Const(text == "1"), k + 1)),
            Kind::Str if text.starts_with('\'') && text.len() >= 2 => {
                let inner = text[1..text.len() - 1].replace("''", "'");
                match parse_bool(&inner) {
                    Some(b) => Some((Operand::Const(b), k + 1)),
                    None => Some((Operand::Expr(text.to_string()), k + 1)),
                }
            }
            Kind::Param => Some((Operand::Expr(text.to_string()), k + 1)),
            Kind::Word if (t.is_word(sql, "pg_bool") || t.is_word(sql, "cast")) && self.punct_at(k + 1, &["("]) => {
                let after = after_parens(&self.toks, k + 1)?;
                let call = self.text(k, after);
                let is_bool_cast = t.is_word(sql, "pg_bool") || {
                    let low = call.to_ascii_lowercase();
                    let inner = low.trim_end_matches(')').trim_end();
                    inner.ends_with(" as boolean") || inner.ends_with(" as bool")
                };
                is_bool_cast.then(|| (Operand::Expr(call.to_string()), after))
            }
            _ => None,
        }
    }

    /// Whether the token at k ends an operand, rather than going on with it.
    fn ends_operand(&self, k: usize) -> bool {
        self.tok(k).is_none_or(|t| match t.kind {
            Kind::Punct => [")", ",", ";"].contains(&t.text(self.sql)),
            Kind::Word => !OPERATOR_WORDS.contains(&t.text(self.sql).to_ascii_lowercase().as_str()),
            _ => false,
        })
    }
}

/// `col [NOT] IN (...)` over every form of the operand's truth value.
fn in_forms(col: &str, negated: bool, operand: &Operand) -> String {
    let op = if negated { "NOT IN" } else { "IN" };
    match operand {
        Operand::Const(b) => format!("{col} {op} ({})", const_forms(*b)),
        Operand::Expr(e) => format!("{col} {op} (SELECT value FROM json_each(overlite_bool_forms({e})))"),
    }
}

/// 1 or 0 and the spellings of the same truth value, as SQL.
fn const_forms(b: bool) -> String {
    let (n, words) = if b { ("1", TRUE_FORMS) } else { ("0", FALSE_FORMS) };
    let mut out = n.to_string();
    for w in words {
        out.push_str(", '");
        out.push_str(w);
        out.push('\'');
    }
    out
}

/// What overlite_bool_forms answers: the forms of v's truth value as a JSON array,
/// `[null]` for NULL, so that `col IN` it is NULL as `col = NULL` is.
pub(crate) fn bool_forms_json(b: Option<bool>) -> String {
    let Some(b) = b else {
        return "[null]".to_string();
    };
    let (n, words) = if b { ("1", TRUE_FORMS) } else { ("0", FALSE_FORMS) };
    let mut out = format!("[{n}");
    for w in words {
        out.push_str(",\"");
        out.push_str(w);
        out.push('"');
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::Sqlite;
    use crate::sqlite::tests::{TempDir, must_exec};

    fn rewritten(eng: &Sqlite, sql: &str) -> String {
        eng.with_own_session(|s| resolve_booleans(&s.state, &s.conn, sql).into_owned())
    }

    fn plan(eng: &Sqlite, sql: &str) -> String {
        let sql = rewritten(eng, sql);
        eng.with_own_session(|s| {
            let mut st = s.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let nulls = vec![rusqlite::types::Null; st.parameter_count()];
            let rows = st
                .query_map(rusqlite::params_from_iter(nulls), |r| r.get::<_, String>(3))
                .unwrap();
            rows.map(std::result::Result::unwrap).collect::<Vec<_>>().join("; ")
        })
    }

    /// A database whose m.b is mixed: it holds a value no boolean reads as, so the
    /// conversion to the storage format leaves it, and it keeps both readings.
    fn engine() -> (TempDir, std::sync::Arc<Sqlite>) {
        let dir = TempDir::new();
        let path = dir.join("b.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE m (id INTEGER PRIMARY KEY, b BOOLEAN, n INTEGER, s TEXT);
             CREATE INDEX m_b ON m (b);
             INSERT INTO m (id, b) VALUES (99, 'maybe');
             CREATE TABLE o (id INTEGER PRIMARY KEY, b TEXT);",
        )
        .unwrap();
        drop(db);
        let eng = Sqlite::open(&path).unwrap();
        (dir, eng)
    }

    /// A database in the storage format from its start: its booleans are 1 and 0.
    fn canonical_engine() -> (TempDir, std::sync::Arc<Sqlite>) {
        let dir = TempDir::new();
        let eng = Sqlite::open(&dir.join("c.db")).unwrap();
        must_exec(
            &eng,
            "CREATE TABLE m (id INTEGER PRIMARY KEY, b BOOLEAN, n INTEGER, s TEXT)",
        );
        must_exec(&eng, "CREATE INDEX m_b ON m (b)");
        (dir, eng)
    }

    // A column in the storage format is read as it is: only a string compared with it
    // becomes the 1 or 0 it reads as.
    #[test]
    fn canonical_columns_stay_plain() {
        let (_dir, eng) = canonical_engine();
        for (sql, want) in [
            ("SELECT id FROM m WHERE b = $1", "SELECT id FROM m WHERE b = $1"),
            ("SELECT id FROM m WHERE b = true", "SELECT id FROM m WHERE b = true"),
            ("SELECT id FROM m WHERE b", "SELECT id FROM m WHERE b"),
            ("SELECT id FROM m WHERE NOT b", "SELECT id FROM m WHERE NOT b"),
            ("SELECT id FROM m WHERE b = 'f'", "SELECT id FROM m WHERE b = 0"),
            ("SELECT id FROM m WHERE 'yes' <> m.b", "SELECT id FROM m WHERE 1 <> m.b"),
            (
                "SELECT id FROM m WHERE b IN ('t', $1)",
                "SELECT id FROM m WHERE b IN (1, $1)",
            ),
            (
                "SELECT id FROM m WHERE b = 'maybe'",
                "SELECT id FROM m WHERE b = pg_bool('maybe')",
            ),
            ("SELECT id FROM m WHERE s = 'f'", "SELECT id FROM m WHERE s = 'f'"),
            (
                "SELECT bool_or(b) AS x FROM m",
                "SELECT pg_bool(max(overlite_bool(b))) AS x FROM m",
            ),
        ] {
            assert_eq!(rewritten(&eng, sql), want, "{sql}");
        }
        for sql in ["SELECT id FROM m WHERE b = ?1", "SELECT id FROM m WHERE b = 'f'"] {
            let p = plan(&eng, sql);
            assert!(p.contains("INDEX m_b (b=?)"), "{sql}: {p}");
        }
    }

    #[test]
    fn rewrites() {
        let (_dir, eng) = engine();
        must_exec(&eng, "CREATE VIEW mv AS SELECT id, b FROM m");
        let t = const_forms(true);
        let f = const_forms(false);
        for (sql, want) in [
            ("SELECT id FROM m WHERE b = true".to_string(), format!("SELECT id FROM m WHERE b IN ({t})")),
            ("SELECT id FROM m WHERE b <> 'f'".into(), format!("SELECT id FROM m WHERE b NOT IN ({f})")),
            ("SELECT id FROM m WHERE 0 = m.b".into(), format!("SELECT id FROM m WHERE m.b IN ({f})")),
            (
                "SELECT id FROM m WHERE b = $1".into(),
                "SELECT id FROM m WHERE b IN (SELECT value FROM json_each(overlite_bool_forms($1)))".into(),
            ),
            ("SELECT id FROM m WHERE b".into(), "SELECT id FROM m WHERE overlite_bool(b)".into()),
            ("SELECT id FROM m WHERE NOT b AND n = 1".into(), "SELECT id FROM m WHERE NOT overlite_bool(b) AND n = 1".into()),
            ("SELECT b, id FROM m".into(), "SELECT b, id FROM m".into()),
            ("SELECT x.b AS b FROM m x".into(), "SELECT x.b AS b FROM m x".into()),
            ("SELECT id FROM m WHERE b IS NULL".into(), "SELECT id FROM m WHERE b IS NULL".into()),
            ("SELECT id FROM m WHERE b IS TRUE".into(), "SELECT id FROM m WHERE overlite_bool(b) IS TRUE".into()),
            ("SELECT id FROM m WHERE b IN ('t', $1)".into(), "SELECT id FROM m WHERE overlite_bool(b) IN (1, pg_bool($1))".into()),
            ("UPDATE m SET b = NOT b WHERE id = 1".into(), "UPDATE m SET b = NOT overlite_bool(b) WHERE id = 1".into()),
            ("INSERT INTO m (id, b) VALUES (1, true)".into(), "INSERT INTO m (id, b) VALUES (1, true)".into()),
            (
                r#"INSERT INTO "m" AS "x" (id, b) VALUES (1, true) ON CONFLICT (b) DO UPDATE SET b = excluded.b"#.into(),
                r#"INSERT INTO "m" AS "x" (id, b) VALUES (1, true) ON CONFLICT (b) DO UPDATE SET b = overlite_bool(excluded.b)"#.into(),
            ),
            (
                "SELECT bool_and(b) FROM m".into(),
                r#"SELECT pg_bool(min(overlite_bool(overlite_bool(b)))) AS "bool_and(b)" FROM m"#.into(),
            ),
            (
                "SELECT NOT b, b = true AS t, count(*) n FROM m".into(),
                format!(r#"SELECT NOT overlite_bool(b) AS "NOT b", b IN ({t}) AS t, count(*) n FROM m"#),
            ),
            // Another table's text column of the same name is no boolean.
            ("SELECT id FROM o WHERE b = 'true'".into(), "SELECT id FROM o WHERE b = 'true'".into()),
            ("SELECT o.id FROM o JOIN m ON m.id = o.id WHERE o.b = 't' AND m.b = 't'".into(),
             format!("SELECT o.id FROM o JOIN m ON m.id = o.id WHERE o.b = 't' AND m.b IN ({t})")),
            ("SELECT o.id FROM o JOIN m ON m.id = o.id WHERE b".into(), "SELECT o.id FROM o JOIN m ON m.id = o.id WHERE b".into()),
            // A label that is not the column.
            ("SELECT count(*) AS b FROM m GROUP BY n HAVING b > 1".into(), "SELECT count(*) AS b FROM m GROUP BY n HAVING b > 1".into()),
            ("CREATE VIEW v AS SELECT id FROM m WHERE b".into(), "CREATE VIEW v AS SELECT id FROM m WHERE b".into()),
            ("SELECT id FROM mv WHERE b".into(), "SELECT id FROM mv WHERE overlite_bool(b)".into()),
        ] {
            assert_eq!(rewritten(&eng, &sql), want, "{sql}");
        }
    }

    // `col = true` and `col = $1` search the index on col as they did before.
    #[test]
    fn comparisons_keep_the_index() {
        let (_dir, eng) = engine();
        for sql in [
            "SELECT id FROM m WHERE b = true",
            "SELECT id FROM m WHERE b = 'false'",
            "SELECT id FROM m WHERE b = ?1",
            "SELECT id FROM m WHERE n = 1 AND b = ?1",
        ] {
            let p = plan(&eng, sql);
            assert!(p.contains("INDEX m_b (b=?)"), "{sql}: {p}");
        }
    }

    #[test]
    fn forms() {
        assert_eq!(bool_forms_json(None), "[null]");
        assert!(bool_forms_json(Some(false)).starts_with("[0,\"f\",\"false\""));
        for w in TRUE_FORMS {
            assert_eq!(parse_bool(w), Some(true), "{w}");
        }
        for w in FALSE_FORMS {
            assert_eq!(parse_bool(w), Some(false), "{w}");
        }
    }
}
