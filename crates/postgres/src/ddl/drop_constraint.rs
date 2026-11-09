//! ALTER TABLE ... DROP CONSTRAINT name. SQLite cannot drop a table constraint in
//! place ("constraint may not be dropped"), so the table is rebuilt from its
//! CREATE TABLE with that constraint taken out, as ADD CONSTRAINT puts one in.
//! Prisma writes it in every migration that drops a relation or a table with one
//! (DROP CONSTRAINT "Template_instanceId_fkey"; DROP TABLE "Template").
//!
//! The constraint is looked for among those of the table and those of its columns,
//! under the name it was given or, unnamed, the one Postgres would have given it,
//! which is what pg_constraint shows. A UNIQUE constraint added with ALTER TABLE may
//! live as a unique index of the same name (alter_add_unique), so dropping one drops
//! that index. Anything else is not there, and as in Postgres that is an error
//! unless the statement says IF EXISTS. That includes a constraint accepted without
//! being kept, by the no-op path for what SQLite could not hold.
//!
//! A key that a foreign key stands on cannot just go: SQLite would answer "foreign
//! key mismatch" to every write on the referencing table from then on. As in
//! Postgres it is refused, and CASCADE takes the foreign keys with it, rebuilding
//! those tables first so that all of it happens or none of it does.

use crate::auth::rls::text_or_empty;
use crate::ddl::tree::{CodeTok, CodeTokKind, code_tokens};
use crate::error::{Result, errf};
use crate::session::Session;
use crate::types::param_types::unquote;
use overlite_core::SqlState;
use overlite_core::lazy_regex;
use overlite_core::text::{eq_fold, quote_ident, quote_literal};
use overlite_engine::{relation_name, split_stored_name, unquote_sql_ident};
use std::ops::Range;

lazy_regex!(
    RE_DROP_CONSTRAINT,
    r#"(?is)\bDROP\s+CONSTRAINT\s+(IF\s+EXISTS\s+)?("(?:[^"]|"")+"|[\w$]+)\s*(?:(CASCADE)|RESTRICT)?\s*;?\s*$"#
);

/// What an ALTER TABLE ... DROP CONSTRAINT asks for.
#[derive(Debug)]
pub(crate) struct DropConstraint {
    name: String,
    if_exists: bool,
    cascade: bool,
}

pub(crate) fn parse_drop_constraint(sql: &str) -> Option<DropConstraint> {
    let m = RE_DROP_CONSTRAINT.captures(sql)?;
    Some(DropConstraint {
        name: unquote(m.get(2).map_or("", |x| x.as_str())),
        if_exists: m.get(1).is_some(),
        cascade: m.get(3).is_some(),
    })
}

impl Session {
    /// Removes a named constraint from table. table_if_exists is the statement's
    /// ALTER TABLE IF EXISTS.
    pub(crate) fn alter_drop_constraint(
        &mut self,
        table: &str,
        drop: &DropConstraint,
        table_if_exists: bool,
    ) -> Result<()> {
        let ddl = self.table_ddl(table);
        if ddl.is_empty() {
            if table_if_exists {
                return Ok(());
            }
            return Err(errf!(SqlState::UNDEFINED_TABLE, "no such table: {table}"));
        }
        if self.drop_exclusion(table, &drop.name)? {
            return Ok(());
        }
        let (kind, cols, index) = match find_constraint(&ddl, table, &drop.name) {
            Some(con) => (con.kind, con.cols, None),
            None => match self.unique_index_of(table, &drop.name)? {
                Some(index) => (ConstraintKind::Unique, self.index_columns(&index)?, Some(index)),
                None if drop.if_exists => return Ok(()),
                None => {
                    let (_, relation) = split_stored_name(table);
                    return Err(errf!(
                        SqlState::UNDEFINED_OBJECT,
                        "constraint \"{}\" of relation \"{relation}\" does not exist",
                        drop.name
                    ));
                }
            },
        };
        let dependents = self.dependent_tables(table, kind, &cols)?;
        if dependents.is_empty() {
            return self.remove_constraint(table, &drop.name, index.as_deref());
        }
        if !drop.cascade {
            let (_, relation) = split_stored_name(table);
            return Err(errf!(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop constraint {} on table {relation} because other objects depend on it",
                drop.name
            ));
        }
        self.exec("SAVEPOINT _dc", &[])?;
        let mut done = Ok(());
        for (child, ddl) in &dependents {
            done = self.rebuild_table(child, ddl);
            if done.is_err() {
                break;
            }
        }
        // After them: a rebuild of this table with the key gone would trip on them,
        // and it may be one of them, referencing itself.
        let done = done.and_then(|()| self.remove_constraint(table, &drop.name, index.as_deref()));
        if done.is_err() {
            let _ = self.exec("ROLLBACK TO _dc", &[]);
        }
        let _ = self.exec("RELEASE _dc", &[]);
        done
    }

    fn remove_constraint(&mut self, table: &str, name: &str, index: Option<&str>) -> Result<()> {
        if let Some(index) = index {
            self.exec(&("DROP INDEX ".to_string() + &quote_ident(index)), &[])?;
            return Ok(());
        }
        let ddl = self.table_ddl(table);
        match find_constraint(&ddl, table, name) {
            Some(con) => self.rebuild_table(table, &without_constraint(&ddl, &con)),
            None => Ok(()),
        }
    }

    /// The tables with a foreign key on this key, each with its CREATE TABLE without
    /// those foreign keys.
    fn dependent_tables(
        &mut self,
        table: &str,
        kind: ConstraintKind,
        cols: &[String],
    ) -> Result<Vec<(String, String)>> {
        if !matches!(kind, ConstraintKind::Primary | ConstraintKind::Unique) {
            return Ok(Vec::new());
        }
        let rs = self.exec(
            "SELECT name, sql FROM main.sqlite_master WHERE type = 'table' AND sql LIKE '%references%'",
            &[],
        )?;
        let mut out = Vec::new();
        for row in &rs.rows {
            let (child, ddl) = (text_or_empty(&row[0]), text_or_empty(&row[1]));
            let mut kept = ddl.clone();
            while let Some(fk) = table_constraints(&kept, &child)
                .into_iter()
                .find(|c| c.stands_on(table, kind, cols))
            {
                kept = without_constraint(&kept, &fk);
            }
            if kept != ddl {
                out.push((child, kept));
            }
        }
        Ok(out)
    }

    fn index_columns(&mut self, index: &str) -> Result<Vec<String>> {
        let rs = self.exec(
            &("SELECT name FROM pragma_index_info(".to_string() + &quote_literal(index) + ")"),
            &[],
        )?;
        Ok(rs.rows.iter().map(|row| text_or_empty(&row[0])).collect())
    }
}

impl Session {
    /// The stored name of table's unique index called name. In a schema it carries
    /// the schema's prefix, as prefix_schema_index_name gave it. A plain index is
    /// not a constraint, in Postgres either.
    fn unique_index_of(&mut self, table: &str, name: &str) -> Result<Option<String>> {
        let (schema, _) = split_stored_name(table);
        let stored = if schema.is_empty() {
            name.to_string()
        } else {
            schema + "." + name
        };
        let rs = self.exec(
            &("SELECT name FROM main.sqlite_master WHERE type = 'index' AND lower(tbl_name) = lower(".to_string()
                + &quote_literal(table)
                + ") AND lower(name) = lower("
                + &quote_literal(&stored)
                + ") AND sql LIKE 'CREATE UNIQUE %'"),
            &[],
        )?;
        Ok(rs.rows.first().map(|row| text_or_empty(&row[0])))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ConstraintKind {
    Primary,
    Unique,
    Foreign,
    Check,
    NotNull,
}

/// A constraint of a CREATE TABLE.
#[derive(Debug)]
struct TableConstraint {
    /// As given, or what Postgres would call it; empty for an unnamed NOT NULL, which
    /// has no name to be dropped by.
    name: String,
    kind: ConstraintKind,
    /// The columns of a key, or those a foreign key is on.
    cols: Vec<String>,
    /// What a foreign key references: the table as written and its columns, none
    /// meaning the primary key.
    references: Option<(String, Vec<String>)>,
    /// Its text in the statement, with the comma before it when it is a whole item.
    span: Range<usize>,
}

impl TableConstraint {
    /// Whether this is a foreign key on that key of table.
    fn stands_on(&self, table: &str, kind: ConstraintKind, cols: &[String]) -> bool {
        let Some((to, to_cols)) = &self.references else {
            return false;
        };
        if !eq_fold(&relation_name(to), table) {
            return false;
        }
        if to_cols.is_empty() {
            return kind == ConstraintKind::Primary;
        }
        let sorted = |cols: &[String]| {
            let mut v: Vec<String> = cols.iter().map(|c| c.to_lowercase()).collect();
            v.sort();
            v
        };
        sorted(to_cols) == sorted(cols)
    }
}

/// Whether a table's definition has a constraint of this name.
pub(crate) fn has_constraint(ddl: &str, table: &str, name: &str) -> bool {
    find_constraint(ddl, table, name).is_some()
}

fn find_constraint(ddl: &str, table: &str, name: &str) -> Option<TableConstraint> {
    let mut cons = table_constraints(ddl, table);
    if let Some(i) = cons.iter().position(|c| eq_fold(&c.name, name)) {
        return Some(cons.swap_remove(i));
    }
    // pg_constraint shows a UNIQUE as <table>_<first column>_key, whatever it was
    // named: SQLite does not report the name, and a client drops what it reads there.
    let (_, relation) = split_stored_name(table);
    let shown = |c: &TableConstraint| {
        c.kind == ConstraintKind::Unique
            && c.cols
                .first()
                .is_some_and(|col| eq_fold(&format!("{relation}_{col}_key"), name))
    };
    let i = cons.iter().position(shown)?;
    Some(cons.swap_remove(i))
}

/// Lists the constraints of a CREATE TABLE, those of the table and of its columns.
fn table_constraints(ddl: &str, table: &str) -> Vec<TableConstraint> {
    let toks = code_tokens(ddl);
    let Some(open) = toks.iter().position(|t| t.is_punct(ddl, "(")) else {
        return Vec::new();
    };
    let close = group_end(ddl, &toks, open) - 1;
    let scan = DdlScan { ddl, toks: &toks };
    let (_, relation) = split_stored_name(table);

    let mut items = Vec::new();
    let mut start = open + 1;
    #[allow(clippy::needless_range_loop)] // i == close ends the last item without reading toks[close]
    for i in open + 1..=close {
        if i == close || (toks[i].depth == 1 && toks[i].is_punct(ddl, ",")) {
            if i > start {
                items.push(start..i);
            }
            start = i + 1;
        }
    }
    let columns: Vec<String> = items
        .iter()
        .filter(|item| !scan.is_table_constraint(item.start))
        .map(|item| scan.ident(item.start))
        .collect();

    let mut out = Vec::new();
    for item in items {
        if scan.is_table_constraint(item.start) {
            out.extend(scan.table_constraint(item, &relation, &columns));
        } else {
            scan.column_constraints(item, &relation, &mut out);
        }
    }
    out
}

/// Takes a constraint out of the CREATE TABLE it was listed from.
fn without_constraint(ddl: &str, con: &TableConstraint) -> String {
    [&ddl[..con.span.start], &ddl[con.span.end..]].concat()
}

/// The token after the group that toks[open] opens.
fn group_end(sql: &str, toks: &[CodeTok], open: usize) -> usize {
    (open + 1..toks.len())
        .find(|&i| toks[i].depth == toks[open].depth && toks[i].is_punct(sql, ")"))
        .map_or(toks.len(), |i| i + 1)
}

#[derive(Debug)]
struct DdlScan<'a> {
    ddl: &'a str,
    toks: &'a [CodeTok],
}

impl DdlScan<'_> {
    fn word(&self, i: usize, words: &[&str]) -> bool {
        i < self.toks.len() && words.iter().any(|w| self.toks[i].is_word(self.ddl, w))
    }

    fn opens_group(&self, i: usize) -> bool {
        i < self.toks.len() && self.toks[i].is_punct(self.ddl, "(")
    }

    fn ident(&self, i: usize) -> String {
        unquote_sql_ident(&self.ddl[self.toks[i].span.clone()])
    }

    fn is_table_constraint(&self, i: usize) -> bool {
        self.word(i, &["constraint", "primary", "unique", "check", "foreign"])
    }

    /// The columns of a parenthesized list, each the first word of its entry.
    fn column_list(&self, open: usize) -> Vec<String> {
        let depth = self.toks[open].depth + 1;
        let mut out = Vec::new();
        for i in open + 1..group_end(self.ddl, self.toks, open) {
            let first = self.toks[i - 1].is_punct(self.ddl, ",") || i == open + 1;
            if first && self.toks[i].depth == depth && self.toks[i].kind != CodeTokKind::Punct {
                out.push(self.ident(i));
            }
        }
        out
    }

    fn constraint_kind(&self, i: usize) -> Option<ConstraintKind> {
        if self.word(i, &["primary"]) {
            Some(ConstraintKind::Primary)
        } else if self.word(i, &["unique"]) {
            Some(ConstraintKind::Unique)
        } else if self.word(i, &["foreign", "references"]) {
            Some(ConstraintKind::Foreign)
        } else if self.word(i, &["check"]) {
            Some(ConstraintKind::Check)
        } else if self.word(i, &["not"]) && self.word(i + 1, &["null"]) {
            Some(ConstraintKind::NotNull)
        } else {
            None
        }
    }

    fn table_constraint(&self, item: Range<usize>, relation: &str, columns: &[String]) -> Option<TableConstraint> {
        let named = self.word(item.start, &["constraint"]);
        let at = if named { item.start + 2 } else { item.start };
        let kind = self.constraint_kind(at)?;
        let open = (at..item.end).find(|&i| self.opens_group(i));
        let cols = open.map(|o| self.column_list(o)).unwrap_or_default();
        let name = if named {
            self.ident(item.start + 1)
        } else if kind == ConstraintKind::Check {
            // Postgres goes by the columns the expression mentions, if it is just one.
            let mentioned: Vec<&String> = columns
                .iter()
                .filter(|c| (at..item.end).any(|i| self.toks[i].kind != CodeTokKind::Punct && self.ident(i) == **c))
                .collect();
            match mentioned[..] {
                [col] => format!("{relation}_{col}_check"),
                _ => format!("{relation}_check"),
            }
        } else {
            default_constraint_name(relation, kind, &cols)
        };
        // A table constraint is never the first item: SQLite wants a column first.
        let comma = self.toks[item.start - 1].span.start;
        let references = (at..item.end)
            .find(|&i| self.word(i, &["references"]))
            .map(|i| self.references(i + 1));
        Some(TableConstraint {
            name,
            kind,
            cols,
            references,
            span: comma..self.toks[item.end - 1].span.end,
        })
    }

    fn column_constraints(&self, item: Range<usize>, relation: &str, out: &mut Vec<TableConstraint>) {
        let column = self.ident(item.start);
        let mut i = item.start + 1;
        while i < item.end {
            if self.toks[i].depth != 1 {
                i += 1;
                continue;
            }
            let named = self.word(i, &["constraint"]);
            let at = if named { i + 2 } else { i };
            let end = self.clause_end(at, item.end);
            // Anything else is a word of the type or a clause Postgres does not name.
            if let Some(kind) = self.constraint_kind(at) {
                let name = match (named, kind) {
                    (true, _) => self.ident(i + 1),
                    (_, ConstraintKind::NotNull) => String::new(),
                    (_, _) => default_constraint_name(relation, kind, std::slice::from_ref(&column)),
                };
                out.push(TableConstraint {
                    name,
                    kind,
                    cols: vec![column.clone()],
                    references: (kind == ConstraintKind::Foreign).then(|| self.references(at + 1)),
                    span: self.toks[i].span.start..self.toks[end - 1].span.end,
                });
            }
            i = end;
        }
    }

    /// The token after the column clause starting at i, which is i + 1 for what is
    /// not one, such as a word of the type.
    fn clause_end(&self, i: usize, end: usize) -> usize {
        let group = |n: usize| {
            if self.opens_group(n) {
                group_end(self.ddl, self.toks, n)
            } else {
                n
            }
        };
        let conflict = |n: usize| {
            if self.word(n, &["on"]) && self.word(n + 1, &["conflict"]) {
                n + 3
            } else {
                n
            }
        };
        let storage = |n: usize| n + self.word(n, &["stored", "virtual"]) as usize;
        let next = if self.word(i, &["primary"]) {
            let n = i + 2 + self.word(i + 2, &["asc", "desc"]) as usize;
            let n = conflict(n);
            n + self.word(n, &["autoincrement"]) as usize
        } else if self.word(i, &["unique", "null"]) {
            conflict(i + 1)
        } else if self.constraint_kind(i) == Some(ConstraintKind::NotNull) {
            conflict(i + 2)
        } else if self.word(i, &["check"]) {
            group(i + 1)
        } else if self.word(i, &["references"]) {
            self.references_end(i + 1)
        } else if self.word(i, &["collate"]) {
            i + 2
        } else if self.word(i, &["default"]) {
            // A value, a signed one, a call or an expression in parentheses.
            let signed = self
                .toks
                .get(i + 1)
                .is_some_and(|t| t.is_punct(self.ddl, "+") || t.is_punct(self.ddl, "-"));
            let n = i + 1 + signed as usize;
            if self.opens_group(n) { group(n) } else { group(n + 1) }
        } else if self.word(i, &["generated"]) {
            storage(group(i + 3))
        } else if self.word(i, &["as"]) {
            storage(group(i + 1))
        } else {
            i + 1
        };
        next.min(end)
    }

    /// The token after the table a REFERENCES names, given where that starts.
    fn references_table_end(&self, i: usize) -> usize {
        let mut n = i + 1;
        while n + 1 < self.toks.len() && self.toks[n].is_punct(self.ddl, ".") {
            n += 2;
        }
        n
    }

    /// The table and the columns of a REFERENCES, given the token after that word.
    fn references(&self, i: usize) -> (String, Vec<String>) {
        let n = self.references_table_end(i);
        let table = self.ddl[self.toks[i].span.start..self.toks[n - 1].span.end].to_string();
        let cols = if self.opens_group(n) {
            self.column_list(n)
        } else {
            Vec::new()
        };
        (table, cols)
    }

    /// The token after a REFERENCES clause, given the one after that word.
    fn references_end(&self, i: usize) -> usize {
        let mut n = self.references_table_end(i);
        if self.opens_group(n) {
            n = group_end(self.ddl, self.toks, n);
        }
        loop {
            if self.word(n, &["on"]) {
                n += 2; // DELETE or UPDATE
                n += if self.word(n, &["set", "no"]) { 2 } else { 1 };
            } else if self.word(n, &["match"]) {
                n += 2;
            } else if self.word(n, &["deferrable"]) || (self.word(n, &["not"]) && self.word(n + 1, &["deferrable"])) {
                n += if self.word(n, &["not"]) { 2 } else { 1 };
                if self.word(n, &["initially"]) {
                    n += 2;
                }
            } else {
                return n;
            }
        }
    }
}

/// What Postgres calls an unnamed constraint.
fn default_constraint_name(relation: &str, kind: ConstraintKind, cols: &[String]) -> String {
    let cols = cols.join("_");
    match kind {
        ConstraintKind::Primary => format!("{relation}_pkey"),
        ConstraintKind::Unique => format!("{relation}_{cols}_key"),
        ConstraintKind::Foreign => format!("{relation}_{cols}_fkey"),
        ConstraintKind::Check => format!("{relation}_{cols}_check"),
        ConstraintKind::NotNull => String::new(),
    }
}

// --- DROP COLUMN ------------------------------------------------------------

// SQLite refuses to drop a column an index, a UNIQUE, a key or a CHECK of another
// column names. Postgres takes the indexes and the table's constraints that involve
// it along, a multi-column one whole, and refuses only when another table's foreign
// key stands on it, unless CASCADE.

lazy_regex!(
    RE_DROP_COLUMN,
    r#"(?is)^\s*ALTER\s+TABLE\s+.*?\bDROP\s+(?:COLUMN\s+)?("(?:[^"]|"")+"|[\w$]+)\s*(?:(CASCADE)|RESTRICT)?\s*;?\s*$"#
);

/// What an ALTER TABLE ... DROP [COLUMN] asks for.
#[derive(Debug)]
pub(crate) struct DropColumn {
    column: String,
    cascade: bool,
}

pub(crate) fn parse_drop_column(sql: &str) -> Option<DropColumn> {
    let m = RE_DROP_COLUMN.captures(sql)?;
    let column = m.get(1)?.as_str();
    // DROP CONSTRAINT x reads as a column called CONSTRAINT here, and IF EXISTS as
    // one called IF: those are the other forms'.
    if ["constraint", "if"].iter().any(|w| eq_fold(column, w)) {
        return None;
    }
    Some(DropColumn {
        column: unquote(column),
        cascade: m.get(2).is_some(),
    })
}

impl Session {
    pub(crate) fn alter_drop_column(&mut self, table: &str, drop: &DropColumn) -> Result<()> {
        let col = &drop.column;
        let (_, relation) = split_stored_name(table);
        let q = "SELECT name, pk FROM pragma_table_info(".to_string()
            + &quote_literal(table)
            + ") WHERE lower(name) = lower("
            + &quote_literal(col)
            + ")";
        let Some(row) = self.exec(&q, &[])?.rows.into_iter().next() else {
            return Err(errf!(
                SqlState::UNDEFINED_COLUMN,
                "column \"{col}\" of relation \"{relation}\" does not exist"
            ));
        };
        let col = text_or_empty(&row[0]);
        let ddl = self.table_ddl(table);
        let cons = table_constraints(&ddl, table);
        let names_col = |c: &TableConstraint| c.cols.iter().any(|x| eq_fold(x, &col));
        // The keys that go with it: its own, and those of the table it is part of.
        let mut keys: Vec<(ConstraintKind, Vec<String>)> = cons
            .iter()
            .filter(|c| matches!(c.kind, ConstraintKind::Primary | ConstraintKind::Unique) && names_col(c))
            .map(|c| (c.kind, c.cols.clone()))
            .collect();
        let indexes = self.indexes_naming(table, &col)?;
        for index in &indexes {
            if self.is_unique_index(index)? {
                keys.push((ConstraintKind::Unique, self.index_columns(index)?));
            }
        }
        let mut dependents: Vec<(String, String)> = Vec::new();
        for (kind, cols) in &keys {
            for (child, ddl) in self.dependent_tables(table, *kind, cols)? {
                if !eq_fold(&child, table) && !dependents.iter().any(|(c, _)| eq_fold(c, &child)) {
                    dependents.push((child, ddl));
                }
            }
        }
        if !dependents.is_empty() && !drop.cascade {
            return Err(errf!(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop column {col} of table {relation} because other objects depend on it"
            ));
        }

        // Out of the CREATE TABLE: the column, and the table's constraints naming it.
        let toks = code_tokens(&ddl);
        let mut kept = ddl.clone();
        let mut gone: Vec<Range<usize>> = cons
            .iter()
            .filter(|c| ddl[c.span.clone()].starts_with(','))
            .filter(|c| {
                if c.kind == ConstraintKind::Check {
                    toks.iter().any(|t| {
                        t.span.start >= c.span.start
                            && t.span.end <= c.span.end
                            && t.kind != CodeTokKind::Punct
                            && eq_fold(&unquote_sql_ident(&ddl[t.span.clone()]), &col)
                    })
                } else {
                    names_col(c)
                }
            })
            .map(|c| c.span.clone())
            .collect();
        // Nothing in the way: SQLite drops it itself, as it always did.
        if gone.is_empty() && indexes.is_empty() && keys.is_empty() {
            let native = "ALTER TABLE ".to_string() + &quote_ident(table) + " DROP COLUMN " + &quote_ident(&col);
            if self.exec(&native, &[]).is_ok() {
                return self.forget_column_default(table, &col);
            }
        }
        gone.extend(column_item(&ddl, &toks, &col));
        gone.sort_by_key(|r| std::cmp::Reverse(r.start));
        for r in gone {
            kept = [&kept[..r.start], &kept[r.end..]].concat();
        }
        // The column was the first item: the comma after it is the one to go.
        if let Some(open) = kept.find('(') {
            let after = kept[open + 1..].trim_start();
            if let Some(rest) = after.strip_prefix(',') {
                kept = kept[..=open].to_string() + rest;
            }
        }

        self.exec("SAVEPOINT _dcol", &[])?;
        let done = (|| {
            for (child, ddl) in &dependents {
                self.rebuild_table(child, ddl)?;
            }
            for index in &indexes {
                self.exec(&("DROP INDEX ".to_string() + &quote_ident(index)), &[])?;
            }
            let copy = self.columns_but(table, &col)?;
            self.rebuild_table_copying(&quote_ident(table), &kept, &copy, "main.sqlite_master", table)?;
            self.forget_column_default(table, &col)
        })();
        if done.is_err() {
            let _ = self.exec("ROLLBACK TO _dcol", &[]);
        }
        let _ = self.exec("RELEASE _dcol", &[]);
        done
    }

    /// A nextval default of the column goes with it, and its grants.
    fn forget_column_default(&mut self, table: &str, col: &str) -> Result<()> {
        self.forget_column_grants(table, col);
        let forget = "DELETE FROM _overlite_defaults WHERE lower(tablename) = lower(".to_string()
            + &quote_literal(table)
            + ") AND lower(colname) = lower("
            + &quote_literal(col)
            + ")";
        self.exec(&forget, &[]).map(|_| ())
    }

    /// The indexes of table whose columns, expressions or WHERE name col.
    fn indexes_naming(&mut self, table: &str, col: &str) -> Result<Vec<String>> {
        let q = "SELECT name, sql FROM main.sqlite_master WHERE type = 'index' AND sql IS NOT NULL AND lower(tbl_name) = lower(".to_string()
            + &quote_literal(table)
            + ")";
        let mut out = Vec::new();
        for row in &self.exec(&q, &[])?.rows {
            let (name, sql) = (text_or_empty(&row[0]), text_or_empty(&row[1]));
            let toks = code_tokens(&sql);
            let Some(open) = toks.iter().position(|t| t.depth == 0 && t.is_punct(&sql, "(")) else {
                continue;
            };
            if toks[open..]
                .iter()
                .any(|t| t.kind != CodeTokKind::Punct && eq_fold(&unquote_sql_ident(&sql[t.span.clone()]), col))
            {
                out.push(name);
            }
        }
        Ok(out)
    }

    fn is_unique_index(&mut self, index: &str) -> Result<bool> {
        let q = "SELECT 1 FROM main.sqlite_master WHERE type = 'index' AND name = ".to_string()
            + &quote_literal(index)
            + " AND sql LIKE 'CREATE UNIQUE %'";
        Ok(!self.exec(&q, &[])?.rows.is_empty())
    }

    fn columns_but(&mut self, table: &str, col: &str) -> Result<Vec<String>> {
        let q = "SELECT name FROM pragma_table_info(".to_string() + &quote_literal(table) + ") ORDER BY cid";
        Ok(self
            .exec(&q, &[])?
            .rows
            .iter()
            .map(|row| text_or_empty(&row[0]))
            .filter(|name| !eq_fold(name, col))
            .collect())
    }
}

/// The span of col's definition in a CREATE TABLE, with the comma before it, or the
/// one after it when it is the first item.
fn column_item(ddl: &str, toks: &[CodeTok], col: &str) -> Option<Range<usize>> {
    let open = toks.iter().position(|t| t.is_punct(ddl, "("))?;
    let close = group_end(ddl, toks, open) - 1;
    let mut start = open + 1;
    for i in open + 1..=close {
        if i == close || (toks[i].depth == 1 && toks[i].is_punct(ddl, ",")) {
            if i > start && eq_fold(&unquote_sql_ident(&ddl[toks[start].span.clone()]), col) {
                let from = toks[start - 1].span.start;
                if toks[start - 1].is_punct(ddl, ",") {
                    return Some(from..toks[i - 1].span.end);
                }
                return Some(toks[start].span.start..toks[i - 1].span.end);
            }
            start = i + 1;
        }
    }
    None
}
