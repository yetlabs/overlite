//! DDL on the tree (docs/adr/0001-sql-ast.md, stage 5b): CREATE TABLE, CREATE INDEX,
//! ALTER TABLE, DROP, RENAME, CREATE VIEW and CREATE TABLE AS, read from Postgres' parse
//! tree and written for SQLite.
//!
//! A table's DDL is what SQLite keeps in sqlite_master and what the catalog reads back
//! (catalog/constraints.rs and the declared types of every column), so the statement
//! is kept as the client wrote it wherever SQLite takes it, and only what SQLite does
//! not take is written anew: the tree says what each part of the statement is, and
//! Postgres' own scanner where it ends, so nothing in a literal, a quoted name or a
//! comment is ever read as code. What changes:
//!
//! - a column's type: serial as INTEGER (a key's with AUTOINCREMENT: the rowid, which a
//!   sequence's values never reuse), numeric as `DECIMALTEXT(p,s) COLLATE DECIMAL`,
//!   citext with NOCASE, an array's sizes dropped and a modified element type quoted
//!   (`"numeric(10,2)[]"`), `pg_catalog.` dropped;
//! - a COLLATE clause as rewrite_collate maps it;
//! - a DEFAULT, a CHECK, a generated column's expression, an index's expressions and
//!   WHERE through the printer (`emit::expression`); a default that is not a literal
//!   in the parentheses SQLite requires, a cast of a literal folded to the literal;
//! - a key's deferral as the comment pg_constraint reads (`/*overlite deferrable*/`),
//!   TABLESPACE, an index's CONCURRENTLY, WITH (...) and USING dropped, its method,
//!   INCLUDE and NULLS NOT DISTINCT in the comments the catalog reads, a non-default
//!   operator class in a comment after its column;
//! - ALTER TABLE's IF EXISTS and ONLY (the session has settled both), a DROP's list as
//!   one DROP each, its CASCADE, a DROP TRIGGER's table.

use crate::ast::emit::{self, Unsupported, ident};
use overlite_core::text::quote_literal;
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, ConstrType, ObjectType, a_const::Val};

/// What a key column of a NULLS NOT DISTINCT index is wrapped in: a NULL becomes a
/// blob no column holds, so two NULLs are one key, as Postgres has them.
pub(crate) const NULL_KEY: &str = "x'6f7665726c6974652d6e756c6c'";

/// Whether statements of this kind are this module's.
pub(crate) fn takes(stmt: &NodeEnum) -> bool {
    matches!(
        stmt,
        NodeEnum::CreateStmt(_)
            | NodeEnum::IndexStmt(_)
            | NodeEnum::AlterTableStmt(_)
            | NodeEnum::DropStmt(_)
            | NodeEnum::RenameStmt(_)
            | NodeEnum::ViewStmt(_)
            | NodeEnum::CreateTableAsStmt(_)
            | NodeEnum::CreateFunctionStmt(_)
            | NodeEnum::ConstraintsSetStmt(_)
            | NodeEnum::VacuumStmt(_)
    )
}

/// The statement sql, whose tree is stmt, as SQLite SQL.
///
/// # Errors
///
/// The first part of it that has no translation here.
pub(crate) fn translate(sql: &str, stmt: &NodeEnum) -> Result<String, Unsupported> {
    match stmt {
        NodeEnum::CreateStmt(s) => create_table(&Src::new(sql)?, s),
        NodeEnum::IndexStmt(s) => create_index(&Src::new(sql)?, s),
        NodeEnum::AlterTableStmt(s) => alter_table(&Src::new(sql)?, s),
        NodeEnum::DropStmt(s) => drop(sql, s),
        NodeEnum::RenameStmt(s) => rename(&Src::new(sql)?, s),
        NodeEnum::ViewStmt(s) => {
            if s.with_check_option != pb::ViewCheckOption::NoCheckOption as i32 {
                return Err("view check option");
            }
            with_query(&Src::new(sql)?, s.view.as_ref(), s.query.as_deref())
        }
        NodeEnum::CreateTableAsStmt(s) => {
            let into = s.into.as_deref().ok_or("CREATE TABLE AS target")?;
            if s.is_select_into || s.objtype != ObjectType::ObjectTable as i32 || into.skip_data {
                return Err("CREATE TABLE AS form");
            }
            with_query(&Src::new(sql)?, into.rel.as_ref(), s.query.as_deref())
        }
        // SQLite or the engine takes these as written: a function's DDL is the
        // engine's (sql_funcs), SET CONSTRAINTS the engine's (deferred.rs), VACUUM
        // SQLite's.
        NodeEnum::CreateFunctionStmt(_) | NodeEnum::ConstraintsSetStmt(_) | NodeEnum::VacuumStmt(_) => {
            Ok(sql.to_string())
        }
        _ => Err("statement kind"),
    }
}

/// A token of the statement, by Postgres' scanner, and the depth of parentheses and
/// brackets it is at (a parenthesis at its own outside).
struct Tok {
    start: usize,
    end: usize,
    depth: u32,
}

/// The statement's text and its tokens, comments left out.
struct Src<'a> {
    sql: &'a str,
    toks: Vec<Tok>,
}

impl<'a> Src<'a> {
    fn new(sql: &'a str) -> Result<Self, Unsupported> {
        let scanned = pg_query::scan_tokens(sql).map_err(|_| "scan error")?;
        let mut toks = Vec::with_capacity(scanned.len());
        let mut depth = 0u32;
        for t in scanned {
            let (Ok(start), Ok(end)) = (usize::try_from(t.start), usize::try_from(t.end)) else {
                return Err("scan error");
            };
            let text = sql.get(start..end).ok_or("scan error")?;
            if text.starts_with("/*") || text.starts_with("--") {
                continue;
            }
            match text {
                "(" | "[" => {
                    toks.push(Tok { start, end, depth });
                    depth += 1;
                }
                ")" | "]" => {
                    depth = depth.saturating_sub(1);
                    toks.push(Tok { start, end, depth });
                }
                _ => toks.push(Tok { start, end, depth }),
            }
        }
        Ok(Src { sql, toks })
    }

    fn text(&self, i: usize) -> &'a str {
        self.toks.get(i).map_or("", |t| &self.sql[t.start..t.end])
    }

    /// Whether token i is the word w (a keyword or a bare name; never a literal or a
    /// quoted name, whose text has its quotes).
    fn is(&self, i: usize, w: &str) -> bool {
        self.text(i).eq_ignore_ascii_case(w)
    }

    /// The first token that starts at or after byte pos.
    fn at(&self, pos: i32) -> Result<usize, Unsupported> {
        let pos = usize::try_from(pos).map_err(|_| "location")?;
        Ok(self.toks.partition_point(|t| t.start < pos))
    }

    /// The token closing the parenthesis or bracket at open.
    fn close(&self, open: usize) -> Result<usize, Unsupported> {
        let d = self.toks.get(open).ok_or("parenthesis")?.depth;
        (open + 1..self.toks.len())
            .find(|&j| self.toks[j].depth == d && matches!(self.text(j), ")" | "]"))
            .ok_or("parenthesis")
    }

    /// The first token from `from` at depth d that is w, before `to`.
    fn find(&self, from: usize, to: usize, d: u32, w: &str) -> Option<usize> {
        (from..to.min(self.toks.len())).find(|&j| self.toks[j].depth == d && self.is(j, w))
    }

    /// The items between the parenthesis at open and its close, split at its commas:
    /// each a range of tokens.
    fn items(&self, open: usize) -> Result<Vec<(usize, usize)>, Unsupported> {
        let close = self.close(open)?;
        let d = self.toks[open].depth + 1;
        let mut out = Vec::new();
        let mut at = open + 1;
        for j in open + 1..close {
            if self.toks[j].depth == d && self.text(j) == "," {
                out.push((at, j));
                at = j + 1;
            }
        }
        if at < close {
            out.push((at, close));
        }
        Ok(out)
    }

    /// The text of tokens a..b, as written.
    fn span(&self, a: usize, b: usize) -> &'a str {
        if a >= b || b > self.toks.len() {
            return "";
        }
        &self.sql[self.toks[a].start..self.toks[b - 1].end]
    }

    /// The end of the statement's last token before a closing semicolon.
    fn body_end(&self) -> usize {
        let mut n = self.toks.len();
        while n > 0 && self.text(n - 1) == ";" {
            n -= 1;
        }
        n
    }
}

/// Changes to a statement's text: each a byte range and what takes its place.
#[derive(Default)]
struct Edits(Vec<(usize, usize, String)>);

impl Edits {
    /// Tokens a..b replaced by text.
    fn replace(&mut self, src: &Src, a: usize, b: usize, text: impl Into<String>) {
        if a < b && b <= src.toks.len() {
            self.0.push((src.toks[a].start, src.toks[b - 1].end, text.into()));
        }
    }

    /// Tokens a..b taken out with the space before them.
    fn remove(&mut self, src: &Src, a: usize, b: usize) {
        if a < b && b <= src.toks.len() {
            let start = if a == 0 { 0 } else { src.toks[a - 1].end };
            self.0.push((start, src.toks[b - 1].end, String::new()));
        }
    }

    /// text put right after token i.
    fn after(&mut self, src: &Src, i: usize, text: impl Into<String>) {
        if let Some(t) = src.toks.get(i) {
            self.0.push((t.end, t.end, text.into()));
        }
    }

    fn apply(mut self, sql: &str) -> Result<String, Unsupported> {
        self.0.sort_by_key(|e| (e.0, e.1));
        let mut out = String::with_capacity(sql.len() + 32);
        let mut at = 0;
        for (a, b, text) in &self.0 {
            if *a < at {
                return Err("overlapping changes");
            }
            out.push_str(&sql[at..*a]);
            out.push_str(text);
            at = *b;
        }
        out.push_str(&sql[at..]);
        Ok(out)
    }
}

/// Comments put at the end of a statement, before a closing semicolon.
fn with_comments(body: String, comments: &[String]) -> String {
    if comments.is_empty() {
        return body;
    }
    let trimmed = body.trim_end();
    let (body, semi) = trimmed.strip_suffix(';').map_or((trimmed, ""), |b| (b.trim_end(), ";"));
    let mut out = body.to_string();
    for c in comments {
        out.push(' ');
        out.push_str(c);
    }
    out.push_str(semi);
    out
}

fn node(n: &pb::Node) -> Result<&NodeEnum, Unsupported> {
    n.node.as_ref().ok_or("empty node")
}

/// The text of a String node.
fn string(n: &pb::Node) -> Result<&str, Unsupported> {
    match node(n)? {
        NodeEnum::String(s) => Ok(&s.sval),
        _ => Err("name part"),
    }
}

// ---- CREATE TABLE and ALTER TABLE ----

fn create_table(src: &Src, s: &pb::CreateStmt) -> Result<String, Unsupported> {
    let rel = s.relation.as_ref().ok_or("CREATE TABLE target")?;
    let mut edits = Edits::default();
    let from = src.at(rel.location)?;
    if rel.relpersistence == "u" {
        // UNLOGGED: how Postgres writes the table's pages; a table all the same.
        let u = src.find(0, from, 0, "unlogged").ok_or("UNLOGGED")?;
        edits.remove(src, u, u + 1);
    }
    let mut after = from + 1;
    if let Some(open) = (from..src.toks.len()).find(|&i| src.toks[i].depth == 0 && src.text(i) == "(") {
        let items = src.items(open)?;
        if items.len() != s.table_elts.len() {
            return Err("CREATE TABLE elements");
        }
        for (&(a, b), elt) in items.iter().zip(&s.table_elts) {
            match node(elt)? {
                NodeEnum::ColumnDef(c) => column(src, a, b, c, &mut edits)?,
                NodeEnum::Constraint(c) => table_constraint(src, a, b, c, &mut edits)?,
                // LIKE, as the text passes left it.
                NodeEnum::TableLikeClause(_) => {}
                _ => return Err("CREATE TABLE element"),
            }
        }
        after = src.close(open)? + 1;
    } else if !s.table_elts.is_empty() {
        return Err("CREATE TABLE elements");
    }
    if !s.tablespacename.is_empty() {
        // TABLESPACE name: a table lives in its database's file.
        let t = src.find(after, src.toks.len(), 0, "tablespace").ok_or("TABLESPACE")?;
        edits.remove(src, t, t + 2);
    }
    edits.apply(src.sql)
}

/// What qualifies a column after its type, in the order written.
enum Qual<'a> {
    Constraint(&'a pb::Constraint),
    Collate(&'a pb::CollateClause),
    /// ALTER COLUMN TYPE's USING, written above.
    Using,
}

fn contype(c: &pb::Constraint) -> ConstrType {
    ConstrType::try_from(c.contype).unwrap_or(ConstrType::Undefined)
}

/// The column definition of tokens a..b.
fn column(src: &Src, a: usize, b: usize, c: &pb::ColumnDef, edits: &mut Edits) -> Result<(), Unsupported> {
    column_def(src, a, b, c, true, edits)
}

/// The column definition of tokens a..b, which a CREATE TABLE or an ADD COLUMN makes
/// (creating), or the type an ALTER COLUMN TYPE gives it, which keeps a numeric and
/// a serial as written, as the text passes did.
fn column_def(
    src: &Src,
    a: usize,
    b: usize,
    c: &pb::ColumnDef,
    creating: bool,
    edits: &mut Edits,
) -> Result<(), Unsupported> {
    let tn = c.type_name.as_ref().ok_or("column type")?;
    let mut quals: Vec<(usize, Qual)> = Vec::new();
    if let Some(using) = c.raw_default.as_deref().and_then(|n| n.node.as_ref()) {
        // ALTER COLUMN ... TYPE ... USING expr.
        let at = (a..b)
            .find(|&j| src.toks[j].depth == src.toks[a].depth && src.is(j, "using"))
            .ok_or("USING")?;
        edits.replace(src, at + 1, b, kept(src, at + 1, b, emit::expression(using, true)?));
        quals.push((at, Qual::Using));
    }
    for n in &c.constraints {
        let NodeEnum::Constraint(k) = node(n)? else {
            return Err("column constraint");
        };
        quals.push((src.at(k.location)?, Qual::Constraint(k)));
    }
    if let Some(coll) = c.coll_clause.as_deref() {
        quals.push((src.at(coll.location)?, Qual::Collate(coll)));
    }
    quals.sort_by_key(|q| q.0);
    let ts = src.at(tn.location)?;
    let te = quals.first().map_or(b, |q| q.0);
    if ts < a || te > b || ts >= te {
        return Err("column type");
    }
    let key = c.constraints.iter().find_map(|n| match node(n) {
        Ok(NodeEnum::Constraint(k)) if contype(k) == ConstrType::ConstrPrimary => Some(k),
        _ => None,
    });
    let serial = column_type(src, ts, te, tn, creating, edits)?;
    let col_type = src.span(ts, te);
    let mut owner = ConstrType::Undefined;
    let mut i = 0;
    while i < quals.len() {
        let qs = quals[i].0;
        let qe = quals.get(i + 1).map_or(b, |q| q.0);
        match &quals[i].1 {
            Qual::Using => {}
            Qual::Collate(coll) => {
                let names: Vec<&str> = coll.collname.iter().map(string).collect::<Result<_, _>>()?;
                let clause = collation(names.last().copied().ok_or("collation")?);
                if clause.is_empty() {
                    edits.remove(src, qs, qe);
                } else {
                    edits.replace(src, qs, qe, clause.trim_start());
                }
            }
            Qual::Constraint(k) => match contype(k) {
                ConstrType::ConstrAttrDeferrable
                | ConstrType::ConstrAttrNotDeferrable
                | ConstrType::ConstrAttrDeferred
                | ConstrType::ConstrAttrImmediate => {
                    // The run of attributes, which belongs to the constraint before it.
                    let mut j = i;
                    let (mut deferrable, mut deferred, mut not) = (false, false, false);
                    while let Some((_, Qual::Constraint(k))) = quals.get(j) {
                        match contype(k) {
                            ConstrType::ConstrAttrDeferrable => deferrable = true,
                            ConstrType::ConstrAttrNotDeferrable => not = true,
                            ConstrType::ConstrAttrDeferred => deferred = true,
                            ConstrType::ConstrAttrImmediate => {}
                            _ => break,
                        }
                        j += 1;
                    }
                    let end = quals.get(j).map_or(b, |q| q.0);
                    if matches!(owner, ConstrType::ConstrPrimary | ConstrType::ConstrUnique) {
                        edits.replace(src, qs, end, deferral(deferrable, deferred, not));
                    }
                    i = j;
                    continue;
                }
                kind => {
                    owner = kind;
                    constraint(src, qs, qe, k, col_type, edits)?;
                    if kind == ConstrType::ConstrPrimary && serial && key.is_some() {
                        let k = src.find(qs, qe, src.toks[qs].depth, "key").ok_or("PRIMARY KEY")?;
                        let at = if src.is(k + 1, "asc") || src.is(k + 1, "desc") {
                            k + 1
                        } else {
                            k
                        };
                        edits.after(src, at, " AUTOINCREMENT");
                    }
                }
            },
        }
        i += 1;
    }
    Ok(())
}

/// The comment a key's deferral is kept in, which pg_constraint reads; empty for NOT
/// DEFERRABLE, the default.
fn deferral(deferrable: bool, deferred: bool, not: bool) -> &'static str {
    if not || !(deferrable || deferred) {
        ""
    } else if deferred {
        "/*overlite deferrable initially deferred*/"
    } else {
        "/*overlite deferrable*/"
    }
}

/// A COLLATE clause as SQLite takes it (rewrite_collate's map): a collation the engine
/// provides under its name, NOCASE for a case-insensitive one, nothing for C, POSIX,
/// default and what is not known.
pub(crate) fn collation(name: &str) -> String {
    if matches!(name.to_lowercase().as_str(), "decimal" | "binary" | "nocase" | "rtrim") {
        return [" COLLATE ", name].concat();
    }
    if overlite_engine::collation_known(name) {
        return [" COLLATE \"", &name.replace('"', "\"\""), "\""].concat();
    }
    if crate::ast::emit::collation_is_ci(name) {
        return " COLLATE NOCASE".to_string();
    }
    String::new()
}

const SERIALS: &[&str] = &["serial", "bigserial", "smallserial", "serial4", "serial8", "serial2"];

/// A column's type, tokens ts..te, as SQLite keeps it. True for a serial.
fn column_type(
    src: &Src,
    ts: usize,
    te: usize,
    tn: &pb::TypeName,
    creating: bool,
    edits: &mut Edits,
) -> Result<bool, Unsupported> {
    let names: Vec<&str> = tn.names.iter().map(string).collect::<Result<_, _>>()?;
    let mut start = ts;
    if src.is(ts, "pg_catalog") && src.text(ts + 1) == "." {
        start = ts + 2;
    }
    let d = src.toks[ts].depth;
    let bracket = (start..te).find(|&j| src.toks[j].depth == d && src.text(j) == "[");
    let new = if !tn.array_bounds.is_empty() {
        let Some(open) = bracket else {
            // `int ARRAY`, as written.
            return Ok(false).inspect(|_| edits.replace(src, ts, te, src.span(start, te)));
        };
        let dims = (open..te)
            .filter(|&j| src.toks[j].depth == d && src.text(j) == "[")
            .count();
        let elem = src.span(start, open);
        if tn.typmods.is_empty() {
            [elem, &"[]".repeat(dims)].concat()
        } else {
            // SQLite takes parentheses last: a quoted name is a type to it, and the
            // catalog reads the modifier from it.
            ["\"", elem, &"[]".repeat(dims), "\""].concat()
        }
    } else if let [name] = names.as_slice()
        && creating
        && SERIALS.iter().any(|s| s.eq_ignore_ascii_case(name))
    {
        edits.replace(src, ts, te, "INTEGER");
        return Ok(true);
    } else if creating && names.last() == Some(&"numeric") && (src.is(start, "numeric") || src.is(start, "decimal")) {
        [
            "DECIMALTEXT",
            &src.sql[src.toks[start].end..src.toks[te - 1].end],
            " COLLATE DECIMAL",
        ]
        .concat()
    } else if names.as_slice() == ["citext"] {
        [src.span(start, te), " COLLATE NOCASE"].concat()
    } else if start == ts {
        return Ok(false);
    } else {
        src.span(start, te).to_string()
    };
    edits.replace(src, ts, te, new);
    Ok(false)
}

/// A constraint of tokens a..b, of a column whose type is written col_type.
fn constraint(
    src: &Src,
    a: usize,
    b: usize,
    k: &pb::Constraint,
    col_type: &str,
    edits: &mut Edits,
) -> Result<(), Unsupported> {
    let d = src.toks[a].depth;
    match contype(k) {
        ConstrType::ConstrDefault => {
            let at = src.find(a, b, d, "default").ok_or("DEFAULT")?;
            let expr = k.raw_expr.as_deref().and_then(|n| n.node.as_ref()).ok_or("DEFAULT")?;
            edits.replace(src, at + 1, b, kept(src, at + 1, b, default_expr(expr, col_type)?));
        }
        ConstrType::ConstrCheck => check(src, a, b, d, "check", k, edits)?,
        ConstrType::ConstrGenerated => check(src, a, b, d, "as", k, edits)?,
        ConstrType::ConstrPrimary | ConstrType::ConstrUnique => key(src, a, b, k, edits)?,
        _ => {}
    }
    Ok(())
}

/// The expression of a CHECK (or a generated column), in the parentheses after word.
fn check(
    src: &Src,
    a: usize,
    b: usize,
    d: u32,
    word: &str,
    k: &pb::Constraint,
    edits: &mut Edits,
) -> Result<(), Unsupported> {
    let at = src.find(a, b, d, word).ok_or("CHECK")?;
    if src.text(at + 1) != "(" {
        return Err("CHECK");
    }
    let close = src.close(at + 1)?;
    let expr = k.raw_expr.as_deref().and_then(|n| n.node.as_ref()).ok_or("CHECK")?;
    let printed = emit::expression(expr, true)?;
    edits.replace(src, at + 2, close, kept(src, at + 2, close, printed));
    Ok(())
}

/// A PRIMARY KEY or UNIQUE of tokens a..b: its deferral as the comment pg_constraint
/// reads, `USING INDEX TABLESPACE` dropped. A column's deferral is a constraint of its
/// own, read by `column`.
fn key(src: &Src, a: usize, b: usize, k: &pb::Constraint, edits: &mut Edits) -> Result<(), Unsupported> {
    let d = src.toks[a].depth;
    if !k.indexspace.is_empty() {
        // USING INDEX TABLESPACE name: an index lives in its table's file.
        let t = (a + 2..b)
            .find(|&j| src.is(j, "tablespace") && src.is(j - 1, "index") && src.is(j - 2, "using"))
            .ok_or("USING INDEX TABLESPACE")?;
        edits.remove(src, t - 2, t + 2);
    }
    let mut i = a;
    while i < b {
        let start = i;
        let (mut deferrable, mut deferred, mut not) = (false, false, false);
        loop {
            let top = src.toks.get(i).is_some_and(|t| t.depth == d) && i < b;
            if top && src.is(i, "not") && src.is(i + 1, "deferrable") {
                not = true;
                i += 2;
            } else if top && src.is(i, "deferrable") {
                deferrable = true;
                i += 1;
            } else if top && src.is(i, "initially") && (src.is(i + 1, "deferred") || src.is(i + 1, "immediate")) {
                deferred = src.is(i + 1, "deferred");
                i += 2;
            } else {
                break;
            }
        }
        if i > start {
            edits.replace(src, start, i, deferral(deferrable, deferred, not));
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// A constraint of the table, tokens a..b.
fn table_constraint(src: &Src, a: usize, b: usize, k: &pb::Constraint, edits: &mut Edits) -> Result<(), Unsupported> {
    let d = src.toks[a].depth;
    match contype(k) {
        ConstrType::ConstrCheck => check(src, a, b, d, "check", k, edits),
        ConstrType::ConstrPrimary | ConstrType::ConstrUnique => key(src, a, b, k, edits),
        _ => Ok(()),
    }
}

/// What tokens a..b become, printed as printed: the text as written when it is the same
/// SQL token for token, which keeps a table's DDL as the client wrote it where SQLite
/// reads it alike.
fn kept(src: &Src, a: usize, b: usize, printed: String) -> String {
    let written = src.span(a, b);
    if same_tokens(written, &printed) {
        written.to_string()
    } else {
        printed
    }
}

/// A column's DEFAULT as SQLite takes it: a literal bare, anything else in the
/// parentheses SQLite requires; a cast of a literal the literal it makes, and the text
/// of a timestamp with an offset, for a timestamptz column, that instant in UTC.
fn default_expr(expr: &NodeEnum, col_type: &str) -> Result<String, Unsupported> {
    if let NodeEnum::AConst(k) = expr
        && let Some(Val::Sval(s)) = &k.val
        && crate::types::timestamptz::is_timestamptz_decl(col_type)
        && let Some(utc) = crate::types::timestamptz::utc_default(&s.sval)
    {
        return Ok(quote_literal(&utc));
    }
    let out = emit::expression(expr, true)?;
    if is_literal(&out) {
        return Ok(out);
    }
    Ok(["(", &out, ")"].concat())
}

/// Whether sql is one literal, which SQLite takes as a default without parentheses:
/// a string, a number with its sign, NULL, TRUE, FALSE, CURRENT_TIMESTAMP and its kin.
fn is_literal(sql: &str) -> bool {
    if let Some(body) = sql.strip_prefix('\'').and_then(|b| b.strip_suffix('\'')) {
        return !body.replace("''", "").contains('\'');
    }
    let digits = sql.strip_prefix(['-', '+']).unwrap_or(sql);
    if !digits.is_empty()
        && digits.bytes().next().is_some_and(|c| c.is_ascii_digit() || c == b'.')
        && digits
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return true;
    }
    matches!(
        sql.to_ascii_uppercase().as_str(),
        "NULL" | "TRUE" | "FALSE" | "CURRENT_TIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIME"
    )
}

// ---- CREATE INDEX ----

/// The operator classes Postgres gives a btree column of a type by default, which
/// pg_get_indexdef leaves out.
const DEFAULT_OPCLASSES: &[&str] = &[
    "bool_ops",
    "bpchar_ops",
    "bytea_ops",
    "char_ops",
    "date_ops",
    "float4_ops",
    "float8_ops",
    "int2_ops",
    "int4_ops",
    "int8_ops",
    "interval_ops",
    "jsonb_ops",
    "name_ops",
    "numeric_ops",
    "oid_ops",
    "text_ops",
    "time_ops",
    "timestamp_ops",
    "timestamptz_ops",
    "uuid_ops",
    "varchar_ops",
];

fn create_index(src: &Src, s: &pb::IndexStmt) -> Result<String, Unsupported> {
    let rel = s.relation.as_ref().ok_or("CREATE INDEX target")?;
    let mut edits = Edits::default();
    let on = src.at(rel.location)?;
    if let Some(c) = src.find(0, on, 0, "concurrently") {
        edits.remove(src, c, c + 1);
    }
    let open = (on..src.toks.len())
        .find(|&i| src.toks[i].depth == 0 && src.text(i) == "(")
        .ok_or("CREATE INDEX keys")?;
    if let Some(u) = src.find(on, open, 0, "using") {
        edits.remove(src, u, open);
    }
    let close = src.close(open)?;
    let items = src.items(open)?;
    if items.len() != s.index_params.len() {
        return Err("CREATE INDEX keys");
    }
    for (&(a, b), p) in items.iter().zip(&s.index_params) {
        let NodeEnum::IndexElem(e) = node(p)? else {
            return Err("index key");
        };
        index_elem(src, a, b, e, s.nulls_not_distinct, &mut edits)?;
    }
    // After the keys: INCLUDE, NULLS [NOT] DISTINCT, WITH (...), TABLESPACE, WHERE.
    let end = src.body_end();
    let mut comments = Vec::new();
    if s.nulls_not_distinct {
        comments.push("/*overlite nulls not distinct*/".to_string());
    }
    let mut i = close + 1;
    while i < end {
        if src.is(i, "include") && src.text(i + 1) == "(" {
            let c = src.close(i + 1)?;
            comments.push(["/*overlite include ", &src.span(i + 2, c).replace("*/", ""), "*/"].concat());
            edits.remove(src, i, c + 1);
            i = c + 1;
        } else if src.is(i, "nulls") && (src.is(i + 1, "distinct") || src.is(i + 2, "distinct")) {
            let n = if src.is(i + 1, "distinct") { 2 } else { 3 };
            edits.remove(src, i, i + n);
            i += n;
        } else if src.is(i, "with") && src.text(i + 1) == "(" {
            let c = src.close(i + 1)?;
            edits.remove(src, i, c + 1);
            i = c + 1;
        } else if src.is(i, "tablespace") {
            let n = if src.text(i + 2) == "." { 4 } else { 2 };
            edits.remove(src, i, i + n);
            i += n;
        } else if src.is(i, "where") {
            let w = s
                .where_clause
                .as_deref()
                .and_then(|n| n.node.as_ref())
                .ok_or("index WHERE")?;
            edits.replace(src, i + 1, end, kept(src, i + 1, end, emit::expression(w, false)?));
            i = end;
        } else {
            return Err("CREATE INDEX clause");
        }
    }
    let method = s.access_method.to_ascii_lowercase();
    if !method.is_empty() && method != "btree" {
        comments.push(["/*overlite using ", &method, "*/"].concat());
    }
    Ok(with_comments(edits.apply(src.sql)?, &comments))
}

/// A key of an index, tokens a..b: an expression through the printer, a non-default
/// operator class in a comment after it, the column of a NULLS NOT DISTINCT index in
/// ifnull with a blob no column holds, so that two NULLs are one key.
fn index_elem(
    src: &Src,
    a: usize,
    b: usize,
    e: &pb::IndexElem,
    nulls_not_distinct: bool,
    edits: &mut Edits,
) -> Result<(), Unsupported> {
    let d = src.toks[a].depth;
    // Where the key itself ends: at COLLATE, the operator class, ASC, DESC or NULLS.
    let key_end = if let Some(expr) = e.expr.as_deref().and_then(|n| n.node.as_ref()) {
        let end = if src.text(a) == "(" {
            src.close(a)? + 1
        } else {
            let open = (a..b).find(|&j| src.text(j) == "(").ok_or("index expression")?;
            src.close(open)? + 1
        };
        let printed = emit::expression(expr, false)?;
        if src.text(a) == "(" {
            edits.replace(src, a + 1, end - 1, kept(src, a + 1, end - 1, printed));
        } else {
            edits.replace(src, a, end, kept(src, a, end, printed));
        }
        end
    } else {
        let end = if src.text(a + 1) == "." { a + 3 } else { a + 1 };
        if nulls_not_distinct {
            let name = src.span(a, end);
            edits.replace(src, a, end, ["ifnull(", name, ", ", NULL_KEY, ")"].concat());
        }
        end
    };
    if let Some(class) = e.opclass.last() {
        let class = string(class)?.to_ascii_lowercase();
        let mut at = key_end;
        if src.is(at, "collate") {
            at += if src.text(at + 2) == "." { 4 } else { 2 };
        }
        let end = if src.text(at + 1) == "." { at + 3 } else { at + 1 };
        if end > b || !src.text(end - 1).trim_matches('"').eq_ignore_ascii_case(&class) {
            return Err("operator class");
        }
        if src.text(end) == "(" && src.toks.get(end).is_some_and(|t| t.depth == d) {
            return Err("operator class options");
        }
        if DEFAULT_OPCLASSES.contains(&class.as_str()) {
            edits.remove(src, at, end);
        } else {
            edits.replace(src, at, end, ["/*", &class, "*/"].concat());
        }
    }
    Ok(())
}

// ---- ALTER TABLE, RENAME ----

/// `ALTER TABLE [IF EXISTS] [ONLY] name` as `ALTER TABLE name`: the session has settled
/// IF EXISTS by now, and ONLY is about inheritance, which there is none of.
fn alter_head(src: &Src, rel: &pb::RangeVar, edits: &mut Edits) -> Result<(), Unsupported> {
    if !(src.is(0, "alter") && src.is(1, "table")) {
        return Ok(());
    }
    let at = src.at(rel.location)?;
    if at > 2 {
        edits
            .0
            .push((src.toks[0].start, src.toks[at].start, "ALTER TABLE ".to_string()));
    }
    Ok(())
}

fn alter_table(src: &Src, s: &pb::AlterTableStmt) -> Result<String, Unsupported> {
    let rel = s.relation.as_ref().ok_or("ALTER TABLE target")?;
    let mut edits = Edits::default();
    alter_head(src, rel, &mut edits)?;
    if s.objtype != ObjectType::ObjectTable as i32 {
        return edits.apply(src.sql);
    }
    // The actions after the table's name, split at their commas.
    let end = src.body_end();
    let mut first = src.at(rel.location)? + 1;
    while src.text(first) == "." {
        first += 2;
    }
    let mut actions = Vec::with_capacity(s.cmds.len());
    let mut at = first;
    for j in first..end {
        if src.toks[j].depth == 0 && src.text(j) == "," {
            actions.push((at, j));
            at = j + 1;
        }
    }
    actions.push((at, end));
    if actions.len() != s.cmds.len() {
        return Err("ALTER TABLE actions");
    }
    for (&(a, b), cmd) in actions.iter().zip(&s.cmds) {
        let NodeEnum::AlterTableCmd(cmd) = node(cmd)? else {
            return Err("ALTER TABLE action");
        };
        let def = cmd.def.as_deref().and_then(|n| n.node.as_ref());
        match (pb::AlterTableType::try_from(cmd.subtype), def) {
            (Ok(pb::AlterTableType::AtAddColumn), Some(NodeEnum::ColumnDef(c))) => {
                column_def(src, src.at(c.location)?, b, c, true, &mut edits)?;
            }
            (Ok(pb::AlterTableType::AtAlterColumnType), Some(NodeEnum::ColumnDef(c))) => {
                let tn = c.type_name.as_ref().ok_or("column type")?;
                let ts = src.at(tn.location)?;
                // From the type on, as a column's definition is read.
                column_def(src, ts, b, c, false, &mut edits)?;
            }
            (Ok(pb::AlterTableType::AtColumnDefault), Some(expr)) => {
                let d = src.find(a, b, 0, "default").ok_or("SET DEFAULT")?;
                edits.replace(src, d + 1, b, kept(src, d + 1, b, default_expr(expr, "")?));
            }
            (Ok(pb::AlterTableType::AtAddConstraint), Some(NodeEnum::Constraint(k))) => match contype(k) {
                ConstrType::ConstrCheck => check(src, a, b, 0, "check", k, &mut edits)?,
                // ddl/exclude reads the constraint as the client wrote it.
                ConstrType::ConstrExclusion => return Err("EXCLUDE"),
                _ => {}
            },
            _ => {}
        }
    }
    edits.apply(src.sql)
}

fn rename(src: &Src, s: &pb::RenameStmt) -> Result<String, Unsupported> {
    let mut edits = Edits::default();
    if let Some(rel) = &s.relation {
        alter_head(src, rel, &mut edits)?;
    }
    edits.apply(src.sql)
}

// ---- DROP ----

/// A DROP of tables, views or indexes as one DROP for each, which is all SQLite's DROP
/// takes (the engine runs the statements of a text one after another), without
/// CASCADE or RESTRICT, which SQLite has neither of, nor CONCURRENTLY; a DROP TRIGGER
/// without its table, which SQLite does not name. Any other is the engine's or
/// SQLite's as written.
fn drop(sql: &str, s: &pb::DropStmt) -> Result<String, Unsupported> {
    let kind = match ObjectType::try_from(s.remove_type) {
        Ok(ObjectType::ObjectTable) => "TABLE",
        Ok(ObjectType::ObjectView) => "VIEW",
        Ok(ObjectType::ObjectIndex) => "INDEX",
        Ok(ObjectType::ObjectTrigger) => "TRIGGER",
        _ => return Ok(sql.to_string()),
    };
    let head = ["DROP ", kind, if s.missing_ok { " IF EXISTS " } else { " " }].concat();
    let mut out = Vec::with_capacity(s.objects.len());
    for o in &s.objects {
        let NodeEnum::List(l) = node(o)? else {
            return Err("DROP object");
        };
        let mut names: Vec<String> = l.items.iter().map(|n| string(n).map(ident)).collect::<Result<_, _>>()?;
        if kind == "TRIGGER" {
            // [schema.]table.trigger: SQLite names a trigger alone.
            names = names.split_off(names.len().saturating_sub(1));
        }
        if names.is_empty() {
            return Err("DROP object");
        }
        out.push([head.as_str(), &names.join(".")].concat());
    }
    Ok(out.join("; "))
}

// ---- CREATE VIEW, CREATE TABLE AS ----

/// A statement made of a head, as written, and a query after its AS, printed.
fn with_query(src: &Src, rel: Option<&pb::RangeVar>, query: Option<&pb::Node>) -> Result<String, Unsupported> {
    let rel = rel.ok_or("target")?;
    let query = query.and_then(|n| n.node.as_ref()).ok_or("query")?;
    let from = src.at(rel.location)?;
    let mut end = src.body_end();
    // CREATE TABLE AS's WITH DATA, as written.
    if end >= 2 && src.is(end - 1, "data") && src.toks[end - 1].depth == 0 && src.is(end - 2, "with") {
        end -= 2;
    }
    let as_ = (from..end)
        .find(|&i| src.toks[i].depth == 0 && src.is(i, "as"))
        .ok_or("AS")?;
    let mut edits = Edits::default();
    edits.replace(src, as_ + 1, end, emit::statement(query)?);
    edits.apply(src.sql)
}

/// Whether two texts are the same SQL token for token, as Postgres' scanner reads them:
/// spacing, the case of keywords, an optional AS and the quotes of a name that needs
/// none apart. False for a text the scanner refuses.
fn same_tokens(a: &str, b: &str) -> bool {
    fn tokens(sql: &str) -> Option<Vec<String>> {
        use pg_query::protobuf::{KeywordKind, Token};
        let scanned = pg_query::scan(sql).ok()?;
        let mut out = Vec::with_capacity(scanned.tokens.len());
        for t in &scanned.tokens {
            let text = sql.get(usize::try_from(t.start).ok()?..usize::try_from(t.end).ok()?)?;
            let token = Token::try_from(t.token).unwrap_or(Token::Nul);
            if matches!(token, Token::Ascii59 | Token::SqlComment | Token::CComment) {
                continue;
            }
            if KeywordKind::try_from(t.keyword_kind).unwrap_or(KeywordKind::NoKeyword) != KeywordKind::NoKeyword {
                let w = text.to_ascii_lowercase();
                if !matches!(w.as_str(), "as" | "inner" | "outer") {
                    out.push(w);
                }
                continue;
            }
            out.push(match token {
                Token::Ident if text.starts_with('"') => {
                    let name = &text[1..text.len() - 1];
                    let plain = !name.is_empty()
                        && name
                            .bytes()
                            .all(|c| c == b'_' || c.is_ascii_lowercase() || c.is_ascii_digit());
                    if plain { name.to_string() } else { text.to_string() }
                }
                Token::Ident => text.to_ascii_lowercase(),
                Token::NotEquals => "<>".to_string(),
                _ => text.to_string(),
            });
        }
        Some(out)
    }
    matches!((tokens(a), tokens(b)), (Some(x), Some(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::translate;

    fn t(sql: &str) -> String {
        let parsed = crate::ast::parse::parse(sql).unwrap();
        let stmt = parsed.stmts[0].stmt.as_ref().unwrap().node.as_ref().unwrap();
        translate(sql, stmt).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    #[test]
    fn create_table_keeps_what_sqlite_takes() {
        for (sql, want) in [
            (
                "CREATE TABLE t (id serial PRIMARY KEY, n numeric(10,2), c citext NOT NULL)",
                "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, n DECIMALTEXT(10,2) COLLATE DECIMAL, c citext COLLATE NOCASE NOT NULL)",
            ),
            (
                "CREATE TABLE t (id serial NOT NULL PRIMARY KEY, a int[3][3], v varchar(20)[])",
                "CREATE TABLE t (id INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT, a int[][], v \"varchar(20)[]\")",
            ),
            (
                "CREATE TABLE t (a int UNIQUE DEFERRABLE INITIALLY DEFERRED, b text COLLATE \"C\" DEFAULT 'x', CONSTRAINT u UNIQUE (a) DEFERRABLE) TABLESPACE pg_default",
                "CREATE TABLE t (a int UNIQUE /*overlite deferrable initially deferred*/, b text DEFAULT 'x', CONSTRAINT u UNIQUE (a) /*overlite deferrable*/)",
            ),
            (
                "CREATE TABLE t (n int DEFAULT '5'::integer, u uuid DEFAULT '08dc6092358649c8b4108813ba44b39e'::uuid, d int DEFAULT 1 + 2)",
                "CREATE TABLE t (n int DEFAULT 5, u uuid DEFAULT '08dc6092-3586-49c8-b410-8813ba44b39e', d int DEFAULT (1 + 2))",
            ),
            (
                "CREATE TABLE \"s\".\"t\" (\"serial\" integer, x text /* numeric */ DEFAULT 'numeric(1)')",
                "CREATE TABLE \"s\".\"t\" (\"serial\" integer, x text /* numeric */ DEFAULT 'numeric(1)')",
            ),
        ] {
            assert_eq!(t(sql), want, "{sql}");
        }
    }

    #[test]
    fn create_index_keeps_what_the_catalog_reads() {
        for (sql, want) in [
            (
                "CREATE INDEX CONCURRENTLY \"i\" ON \"t\" USING gin (\"tags\") WITH (fastupdate = off)",
                "CREATE INDEX \"i\" ON \"t\" (\"tags\") /*overlite using gin*/",
            ),
            (
                "CREATE UNIQUE INDEX i ON t (h, w DESC) INCLUDE (x) NULLS NOT DISTINCT WHERE a > 1",
                "CREATE UNIQUE INDEX i ON t (ifnull(h, x'6f7665726c6974652d6e756c6c'), ifnull(w, x'6f7665726c6974652d6e756c6c') DESC) WHERE a > 1 /*overlite nulls not distinct*/ /*overlite include x*/",
            ),
            (
                "CREATE INDEX i ON t (a pg_catalog.text_pattern_ops DESC, b int4_ops)",
                "CREATE INDEX i ON t (a /*text_pattern_ops*/ DESC, b)",
            ),
        ] {
            assert_eq!(t(sql), want, "{sql}");
        }
    }

    #[test]
    fn drops_and_alters() {
        for (sql, want) in [
            ("DROP TABLE a, \"b c\" CASCADE", "DROP TABLE a; DROP TABLE \"b c\""),
            ("DROP INDEX CONCURRENTLY IF EXISTS \"i\"", "DROP INDEX IF EXISTS i"),
            ("DROP TRIGGER IF EXISTS x ON t", "DROP TRIGGER IF EXISTS x"),
            (
                "ALTER TABLE IF EXISTS ONLY t ADD COLUMN n numeric",
                "ALTER TABLE t ADD COLUMN n DECIMALTEXT COLLATE DECIMAL",
            ),
            (
                "ALTER TABLE ONLY public.t RENAME COLUMN c TO d",
                "ALTER TABLE public.t RENAME COLUMN c TO d",
            ),
        ] {
            assert_eq!(t(sql), want, "{sql}");
        }
    }
}
