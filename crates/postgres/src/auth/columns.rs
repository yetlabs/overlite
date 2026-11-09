//! The columns of each table a statement reads and writes, read from its parse tree
//! (the AST path, docs/adr/0001-sql-ast.md), for column privileges.
//!
//! What Postgres collects at parse analysis for each table of a statement (its
//! RTEPermissionInfo) is collected here: which privileges it needs, and the columns it
//! selects, inserts and updates. A column is looked up in its own query level first,
//! then outward, as Postgres resolves it; a `*` or a whole-row reference takes every
//! column of the table.
//!
//! Anything this reader does not know (a node it does not walk, a reference it cannot
//! place) makes [`accesses`] answer None, and the caller then holds the statement to
//! table-level grants alone, so a role with only column privileges is refused: the
//! reader may ask for more columns than Postgres would, never fewer.

use overlite_core::text::eq_fold;
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, OnConflictAction, SetOperation};
use std::collections::BTreeSet;

/// Where the tables of a statement are found.
pub(crate) trait Relations {
    /// The stored name of the table `schema.name` (schema empty when unqualified) and
    /// its columns, None for its columns when they cannot be read (no such table).
    fn relation(&mut self, schema: &str, name: &str) -> (String, Option<Vec<String>>);
}

/// Columns a privilege is needed on: `all` of the table's, or those in `names`
/// (lower-cased). Neither means any one column will do, as Postgres asks of a table
/// read without naming a column (`SELECT count(*) FROM t`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Cols {
    pub(crate) all: bool,
    pub(crate) names: BTreeSet<String>,
}

/// What a statement needs on one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Access {
    /// Its stored name.
    pub(crate) table: String,
    /// Its columns, None when they could not be read.
    pub(crate) columns: Option<Vec<String>>,
    pub(crate) select: Option<Cols>,
    pub(crate) insert: Option<Cols>,
    pub(crate) update: Option<Cols>,
    pub(crate) delete: bool,
}

/// What sql needs on each table it touches, None when the statement is not one
/// statement of SELECT, INSERT, UPDATE or DELETE that this reader follows whole.
pub(crate) fn accesses(sql: &str, rels: &mut dyn Relations) -> Option<Vec<Access>> {
    let parsed = crate::ast::parse::parse(sql).ok()?;
    let [raw] = parsed.stmts.as_slice() else {
        return None;
    };
    let stmt = raw.stmt.as_deref()?.node.as_ref()?;
    let mut w = Walker {
        rels,
        out: Vec::new(),
        levels: Vec::new(),
        ctes: Vec::new(),
    };
    w.statement(stmt).ok()?;
    Some(w.out)
}

/// Not followed: the caller falls back to table-level grants.
#[derive(Debug)]
struct Unknown;

type Walk = Result<(), Unknown>;

/// A FROM item: what a qualifier names, and the table it reads, if it is one.
#[derive(Debug)]
struct Item {
    /// The alias, else the table's name, lower-cased as Postgres folds it.
    name: String,
    /// The schema written, for a reference qualified by it; empty under an alias.
    schema: String,
    /// Index into out; None for a subquery, a CTE, a function or a catalog.
    table: Option<usize>,
}

#[derive(Debug, Default)]
struct Level {
    items: Vec<Item>,
    /// The output names of the select list, which ORDER BY prefers.
    outputs: Vec<String>,
}

struct Walker<'r> {
    rels: &'r mut dyn Relations,
    out: Vec<Access>,
    levels: Vec<Level>,
    /// The CTE names visible, innermost last.
    ctes: Vec<String>,
}

fn n(x: &pb::Node) -> Option<&NodeEnum> {
    x.node.as_ref()
}

fn nb(x: Option<&pb::Node>) -> Option<&NodeEnum> {
    x.and_then(n)
}

fn sval(x: &pb::Node) -> Option<&str> {
    match n(x) {
        Some(NodeEnum::String(s)) => Some(s.sval.as_str()),
        _ => None,
    }
}

/// A catalog or internal table, which no grant governs.
fn is_system(schema: &str, name: &str) -> bool {
    let low = name.to_ascii_lowercase();
    eq_fold(schema, "pg_catalog")
        || eq_fold(schema, "information_schema")
        || low.starts_with("pg_")
        || low.starts_with("sqlite_")
        || low.starts_with("_overlite")
        || low == "information_schema"
        || low.starts_with("information_schema.")
}

/// A name SQLite reads as the row's id when no column takes it, and ctid, which the
/// engine reads as the rowid. The rowid of a table whose key is an INTEGER PRIMARY KEY
/// is that column, so it asks for every column, as a system column asks Postgres for
/// a privilege on the table.
fn is_rowid(name: &str) -> bool {
    ["rowid", "_rowid_", "oid", "ctid"].iter().any(|r| eq_fold(r, name))
}

/// The privilege a column is marked for.
#[derive(Clone, Copy)]
enum Mark {
    Select,
    Insert,
    Update,
}

impl Walker<'_> {
    fn statement(&mut self, s: &NodeEnum) -> Walk {
        match s {
            NodeEnum::SelectStmt(q) => self.select(q),
            NodeEnum::InsertStmt(i) => self.insert(i),
            NodeEnum::UpdateStmt(u) => self.update(u),
            NodeEnum::DeleteStmt(d) => self.delete(d),
            _ => Err(Unknown),
        }
    }

    /// A statement nested in another (a subquery, a CTE), as a level of its own.
    fn nested(&mut self, s: Option<&NodeEnum>) -> Walk {
        self.statement(s.ok_or(Unknown)?)
    }

    /// Makes the CTEs of a WITH visible, walking their queries; returns how many
    /// names to drop when the statement ends. A CTE is visible to the ones after it,
    /// and under RECURSIVE to all of them, its own query included: before that, a
    /// table of the same name is the table.
    fn with(&mut self, w: Option<&pb::WithClause>) -> Result<usize, Unknown> {
        let Some(w) = w else { return Ok(0) };
        let mut names = Vec::new();
        for c in &w.ctes {
            let Some(NodeEnum::CommonTableExpr(c)) = n(c) else {
                return Err(Unknown);
            };
            if c.search_clause.is_some() || c.cycle_clause.is_some() {
                return Err(Unknown);
            }
            names.push(c.ctename.to_lowercase());
        }
        let before = self.ctes.len();
        if w.recursive {
            self.ctes.extend(names.iter().cloned());
        }
        for (c, name) in w.ctes.iter().zip(&names) {
            let Some(NodeEnum::CommonTableExpr(c)) = n(c) else {
                return Err(Unknown);
            };
            self.nested(nb(c.ctequery.as_deref()))?;
            if !w.recursive {
                self.ctes.push(name.clone());
            }
        }
        Ok(self.ctes.len() - before)
    }

    fn end_with(&mut self, count: usize) {
        self.ctes.truncate(self.ctes.len() - count);
    }

    fn select(&mut self, q: &pb::SelectStmt) -> Walk {
        if q.into_clause.is_some() {
            return Err(Unknown);
        }
        let ctes = self.with(q.with_clause.as_ref())?;
        let done = self.select_body(q);
        self.end_with(ctes);
        done
    }

    fn select_body(&mut self, q: &pb::SelectStmt) -> Walk {
        if q.op != SetOperation::SetopNone as i32 {
            let (Some(l), Some(r)) = (&q.larg, &q.rarg) else {
                return Err(Unknown);
            };
            self.select(l)?;
            self.select(r)?;
            // The ORDER BY of a set operation names its output columns.
            for k in &q.sort_clause {
                match n(k) {
                    Some(NodeEnum::SortBy(k)) => match nb(k.node.as_deref()) {
                        Some(NodeEnum::ColumnRef(_) | NodeEnum::AConst(_)) => {}
                        _ => return Err(Unknown),
                    },
                    _ => return Err(Unknown),
                }
            }
            self.levels.push(Level::default());
            let done = self.limits(q);
            self.levels.pop();
            return done;
        }
        self.levels.push(Level::default());
        let done = self.select_level(q);
        self.levels.pop();
        done
    }

    fn limits(&mut self, q: &pb::SelectStmt) -> Walk {
        self.opt_expr(q.limit_count.as_deref())?;
        self.opt_expr(q.limit_offset.as_deref())
    }

    fn select_level(&mut self, q: &pb::SelectStmt) -> Walk {
        for row in &q.values_lists {
            self.expr(row)?;
        }
        for f in &q.from_clause {
            self.range_item(f)?;
        }
        let mut outputs = Vec::new();
        for t in &q.target_list {
            let Some(NodeEnum::ResTarget(t)) = n(t) else {
                return Err(Unknown);
            };
            if !t.indirection.is_empty() {
                return Err(Unknown);
            }
            if !t.name.is_empty() {
                outputs.push(t.name.to_lowercase());
            }
            self.opt_expr(t.val.as_deref())?;
        }
        if let Some(level) = self.levels.last_mut() {
            level.outputs = outputs;
        }
        self.opt_expr(q.where_clause.as_deref())?;
        for g in &q.group_clause {
            self.expr(g)?;
        }
        self.opt_expr(q.having_clause.as_deref())?;
        for w in &q.window_clause {
            self.expr(w)?;
        }
        for d in &q.distinct_clause {
            // DISTINCT alone is a list holding one empty node.
            if n(d).is_some() {
                self.expr(d)?;
            }
        }
        for k in &q.sort_clause {
            let Some(NodeEnum::SortBy(k)) = n(k) else {
                return Err(Unknown);
            };
            // A bare name that is an output column is that column, not the table's.
            if let Some(NodeEnum::ColumnRef(c)) = nb(k.node.as_deref())
                && let [one] = c.fields.as_slice()
                && let Some(name) = sval(one)
                && self.levels.last().is_some_and(|l| l.outputs.iter().any(|o| o == name))
            {
                continue;
            }
            self.opt_expr(k.node.as_deref())?;
        }
        self.limits(q)
    }

    /// The table a reference names, as an index into out.
    fn access(&mut self, r: &pb::RangeVar) -> Option<usize> {
        if is_system(&r.schemaname, &r.relname) || !r.catalogname.is_empty() {
            return None;
        }
        if r.schemaname.is_empty() && self.ctes.iter().any(|c| c == &r.relname.to_lowercase()) {
            return None;
        }
        let (table, columns) = self.rels.relation(&r.schemaname, &r.relname);
        if let Some(i) = self.out.iter().position(|a| a.table == table) {
            return Some(i);
        }
        self.out.push(Access {
            table,
            columns,
            select: None,
            insert: None,
            update: None,
            delete: false,
        });
        Some(self.out.len() - 1)
    }

    fn push_item(&mut self, r: &pb::RangeVar, table: Option<usize>) {
        let (name, schema) = match &r.alias {
            Some(a) => (a.aliasname.to_lowercase(), String::new()),
            None => (r.relname.to_lowercase(), r.schemaname.to_lowercase()),
        };
        if let Some(level) = self.levels.last_mut() {
            level.items.push(Item { name, schema, table });
        }
    }

    fn range_item(&mut self, f: &pb::Node) -> Walk {
        match n(f) {
            Some(NodeEnum::RangeVar(r)) => {
                if r.alias.as_ref().is_some_and(|a| !a.colnames.is_empty()) {
                    return Err(Unknown); // t AS x (c1, c2) renames the columns
                }
                let table = self.access(r);
                if let Some(i) = table {
                    self.out[i].select.get_or_insert_with(Cols::default);
                }
                self.push_item(r, table);
                Ok(())
            }
            Some(NodeEnum::JoinExpr(j)) => {
                if j.alias.is_some() || j.join_using_alias.is_some() {
                    return Err(Unknown);
                }
                let first = self.levels.last().map_or(0, |l| l.items.len());
                self.range_item(j.larg.as_deref().ok_or(Unknown)?)?;
                self.range_item(j.rarg.as_deref().ok_or(Unknown)?)?;
                let joined: Vec<usize> = self
                    .levels
                    .last()
                    .map_or_else(Vec::new, |l| l.items[first..].iter().filter_map(|i| i.table).collect());
                if j.is_natural {
                    // The columns the two sides share, and more: every column.
                    for t in joined {
                        self.mark_all(t, Mark::Select);
                    }
                } else {
                    for u in &j.using_clause {
                        let name = sval(u).ok_or(Unknown)?;
                        for &t in &joined {
                            self.mark(t, name, Mark::Select);
                        }
                    }
                }
                self.opt_expr(j.quals.as_deref())
            }
            Some(NodeEnum::RangeSubselect(s)) => {
                self.nested(nb(s.subquery.as_deref()))?;
                self.push_opaque(s.alias.as_ref(), "");
                Ok(())
            }
            Some(NodeEnum::RangeFunction(f)) => {
                if f.is_rowsfrom {
                    return Err(Unknown);
                }
                let mut name = String::new();
                for item in &f.functions {
                    let Some(NodeEnum::List(l)) = n(item) else {
                        return Err(Unknown);
                    };
                    if let Some(NodeEnum::FuncCall(c)) = l.items.first().and_then(n) {
                        name = c.funcname.last().and_then(sval).unwrap_or_default().to_lowercase();
                    }
                    if let Some(call) = l.items.first() {
                        self.expr(call)?;
                    }
                }
                self.push_opaque(f.alias.as_ref(), &name);
                Ok(())
            }
            _ => Err(Unknown),
        }
    }

    fn push_opaque(&mut self, alias: Option<&pb::Alias>, fallback: &str) {
        let name = alias.map_or_else(|| fallback.to_string(), |a| a.aliasname.to_lowercase());
        if let Some(level) = self.levels.last_mut() {
            level.items.push(Item {
                name,
                schema: String::new(),
                table: None,
            });
        }
    }

    /// The column of table t named name, as the table spells it, if it has one.
    fn column_of(&self, t: usize, name: &str) -> Option<String> {
        self.out[t]
            .columns
            .as_ref()?
            .iter()
            .find(|c| eq_fold(c, name))
            .map(|c| c.to_lowercase())
    }

    fn cols(&mut self, t: usize, mark: Mark) -> &mut Cols {
        let a = &mut self.out[t];
        let slot = match mark {
            Mark::Select => &mut a.select,
            Mark::Insert => &mut a.insert,
            Mark::Update => &mut a.update,
        };
        slot.get_or_insert_with(Cols::default)
    }

    /// Marks a column of t. One t does not have is not marked, unless t's columns are
    /// unknown, which then asks for all of them.
    fn mark(&mut self, t: usize, name: &str, mark: Mark) {
        if self.out[t].columns.is_none() {
            self.mark_all(t, mark);
            return;
        }
        if let Some(c) = self.column_of(t, name) {
            self.cols(t, mark).names.insert(c);
        } else if is_rowid(name) {
            self.mark_all(t, mark);
        }
    }

    fn mark_all(&mut self, t: usize, mark: Mark) {
        self.cols(t, mark).all = true;
    }

    /// Whether t may have a column of that name: it has, or its columns are unknown.
    fn may_have(&self, t: usize, name: &str) -> bool {
        self.out[t].columns.is_none() || self.column_of(t, name).is_some() || is_rowid(name)
    }

    /// The item a qualifier names, innermost level first.
    fn item(&self, schema: &str, name: &str) -> Option<&Item> {
        self.levels
            .iter()
            .rev()
            .flat_map(|level| level.items.iter().rev())
            .find(|it| it.name == name && (schema.is_empty() || it.schema == schema))
    }

    fn column_ref(&mut self, c: &pb::ColumnRef) -> Walk {
        // None stands for `*`.
        let mut fields: Vec<Option<String>> = Vec::with_capacity(c.fields.len());
        for f in &c.fields {
            match n(f) {
                Some(NodeEnum::String(s)) => fields.push(Some(s.sval.to_lowercase())),
                Some(NodeEnum::AStar(_)) => fields.push(None),
                _ => return Err(Unknown),
            }
        }
        match fields.as_slice() {
            [None] => {
                let tables: Vec<usize> = self
                    .levels
                    .last()
                    .map_or_else(Vec::new, |l| l.items.iter().filter_map(|i| i.table).collect());
                for t in tables {
                    self.mark_all(t, Mark::Select);
                }
                Ok(())
            }
            [Some(name)] => {
                self.bare(name);
                Ok(())
            }
            [Some(q), last] => self.qualified("", q, last.as_deref()),
            [Some(s), Some(q), last] => self.qualified(s, q, last.as_deref()),
            _ => Err(Unknown),
        }
    }

    /// `q.col` or `q.*`. A qualifier that is no FROM item is not followed.
    fn qualified(&mut self, schema: &str, q: &str, col: Option<&str>) -> Walk {
        if let Some(t) = self.item(schema, q).ok_or(Unknown)?.table {
            match col {
                None => self.mark_all(t, Mark::Select),
                Some(c) => self.mark(t, c, Mark::Select),
            }
        }
        Ok(())
    }

    /// A bare name: the column of the innermost level whose tables have it, every
    /// such table there; else a FROM item's whole row; else an output name, or what
    /// the statement will fail on anyway.
    fn bare(&mut self, name: &str) {
        for depth in (0..self.levels.len()).rev() {
            let hits: Vec<usize> = self.levels[depth]
                .items
                .iter()
                .filter_map(|i| i.table)
                .filter(|&t| self.may_have(t, name))
                .collect();
            if !hits.is_empty() {
                for t in hits {
                    self.mark(t, name, Mark::Select);
                }
                return;
            }
        }
        if let Some(t) = self.item("", name).and_then(|i| i.table) {
            self.mark_all(t, Mark::Select);
        }
    }

    fn opt_expr(&mut self, x: Option<&pb::Node>) -> Walk {
        match x {
            Some(x) => self.expr(x),
            None => Ok(()),
        }
    }

    fn exprs(&mut self, xs: &[pb::Node]) -> Walk {
        xs.iter().try_for_each(|x| self.expr(x))
    }

    fn expr(&mut self, x: &pb::Node) -> Walk {
        let Some(x) = n(x) else { return Err(Unknown) };
        match x {
            NodeEnum::ColumnRef(c) => self.column_ref(c),
            NodeEnum::ParamRef(_)
            | NodeEnum::AConst(_)
            | NodeEnum::SqlvalueFunction(_)
            | NodeEnum::SetToDefault(_)
            | NodeEnum::String(_)
            | NodeEnum::Integer(_) => Ok(()),
            NodeEnum::TypeCast(c) => self.opt_expr(c.arg.as_deref()),
            NodeEnum::AExpr(e) => {
                self.opt_expr(e.lexpr.as_deref())?;
                self.opt_expr(e.rexpr.as_deref())
            }
            NodeEnum::BoolExpr(e) => self.exprs(&e.args),
            NodeEnum::List(l) => self.exprs(&l.items),
            NodeEnum::CoalesceExpr(e) => self.exprs(&e.args),
            NodeEnum::MinMaxExpr(e) => self.exprs(&e.args),
            NodeEnum::RowExpr(e) => self.exprs(&e.args),
            NodeEnum::AArrayExpr(e) => self.exprs(&e.elements),
            NodeEnum::NullTest(e) => self.opt_expr(e.arg.as_deref()),
            NodeEnum::BooleanTest(e) => self.opt_expr(e.arg.as_deref()),
            NodeEnum::CollateClause(e) => self.opt_expr(e.arg.as_deref()),
            NodeEnum::NamedArgExpr(e) => self.opt_expr(e.arg.as_deref()),
            NodeEnum::ResTarget(t) => self.opt_expr(t.val.as_deref()),
            NodeEnum::SortBy(s) => self.opt_expr(s.node.as_deref()),
            NodeEnum::MultiAssignRef(m) => self.opt_expr(m.source.as_deref()),
            NodeEnum::GroupingSet(g) => self.exprs(&g.content),
            NodeEnum::FuncCall(f) => {
                self.exprs(&f.args)?;
                self.exprs(&f.agg_order)?;
                self.opt_expr(f.agg_filter.as_deref())?;
                if let Some(w) = &f.over {
                    self.window(w)?;
                }
                Ok(())
            }
            NodeEnum::WindowDef(w) => self.window(w),
            NodeEnum::SubLink(s) => {
                self.opt_expr(s.testexpr.as_deref())?;
                self.nested(nb(s.subselect.as_deref()))
            }
            NodeEnum::CaseExpr(c) => {
                self.opt_expr(c.arg.as_deref())?;
                self.exprs(&c.args)?;
                self.opt_expr(c.defresult.as_deref())
            }
            NodeEnum::CaseWhen(w) => {
                self.opt_expr(w.expr.as_deref())?;
                self.opt_expr(w.result.as_deref())
            }
            NodeEnum::AIndirection(i) => {
                self.opt_expr(i.arg.as_deref())?;
                for x in &i.indirection {
                    match n(x) {
                        Some(NodeEnum::AIndices(ix)) => {
                            self.opt_expr(ix.lidx.as_deref())?;
                            self.opt_expr(ix.uidx.as_deref())?;
                        }
                        Some(NodeEnum::String(_)) => {}
                        _ => return Err(Unknown),
                    }
                }
                Ok(())
            }
            _ => Err(Unknown),
        }
    }

    fn window(&mut self, w: &pb::WindowDef) -> Walk {
        self.exprs(&w.partition_clause)?;
        self.exprs(&w.order_clause)?;
        self.opt_expr(w.start_offset.as_deref())?;
        self.opt_expr(w.end_offset.as_deref())
    }

    fn returning(&mut self, r: Option<&pb::ReturningClause>) -> Walk {
        let Some(r) = r else { return Ok(()) };
        if !r.options.is_empty() {
            return Err(Unknown); // RETURNING WITH (OLD AS o, NEW AS n)
        }
        self.exprs(&r.exprs)
    }

    /// The target of an INSERT, UPDATE or DELETE, as a FROM item of a new level.
    fn target(&mut self, r: Option<&pb::RangeVar>) -> Result<usize, Unknown> {
        let r = r.ok_or(Unknown)?;
        let t = self.access(r).ok_or(Unknown)?;
        self.levels.push(Level::default());
        self.push_item(r, Some(t));
        Ok(t)
    }

    fn insert(&mut self, i: &pb::InsertStmt) -> Walk {
        let ctes = self.with(i.with_clause.as_ref())?;
        let done = self.insert_body(i);
        self.end_with(ctes);
        done
    }

    fn insert_body(&mut self, i: &pb::InsertStmt) -> Walk {
        // The rows come from a query that cannot see the target.
        let source = nb(i.select_stmt.as_deref());
        if let Some(s) = source {
            self.statement(s)?;
        }
        let t = self.target(i.relation.as_ref())?;
        let done = self.insert_target(i, t, source);
        self.levels.pop();
        done
    }

    fn insert_target(&mut self, i: &pb::InsertStmt, t: usize, source: Option<&NodeEnum>) -> Walk {
        let mut named = Vec::new();
        for c in &i.cols {
            let Some(NodeEnum::ResTarget(c)) = n(c) else {
                return Err(Unknown);
            };
            if !c.indirection.is_empty() {
                return Err(Unknown);
            }
            named.push(c.name.clone());
        }
        self.cols(t, Mark::Insert);
        if !named.is_empty() {
            for c in &named {
                self.mark(t, c, Mark::Insert);
            }
        } else if let Some(width) = source.map(row_width) {
            // Without a list, a row fills the table's first columns.
            match (width, self.out[t].columns.clone()) {
                (Some(k), Some(cols)) => {
                    for c in cols.iter().take(k) {
                        self.mark(t, c, Mark::Insert);
                    }
                }
                _ => self.mark_all(t, Mark::Insert),
            }
        }
        if let Some(oc) = &i.on_conflict_clause {
            self.on_conflict(oc, t)?;
        }
        self.returning(i.returning_clause.as_ref())
    }

    fn on_conflict(&mut self, oc: &pb::OnConflictClause, t: usize) -> Walk {
        // EXCLUDED reads as the target does: Postgres asks SELECT on its columns too.
        if let Some(level) = self.levels.last_mut() {
            level.items.push(Item {
                name: "excluded".to_string(),
                schema: String::new(),
                table: Some(t),
            });
        }
        if let Some(infer) = &oc.infer {
            if !infer.conname.is_empty() {
                // ON CONFLICT ON CONSTRAINT: its columns are the constraint's.
                self.mark_all(t, Mark::Select);
            }
            for e in &infer.index_elems {
                let Some(NodeEnum::IndexElem(e)) = n(e) else {
                    return Err(Unknown);
                };
                if e.name.is_empty() {
                    self.opt_expr(e.expr.as_deref())?;
                } else {
                    self.mark(t, &e.name, Mark::Select);
                }
            }
            self.opt_expr(infer.where_clause.as_deref())?;
        }
        if oc.action == OnConflictAction::OnconflictUpdate as i32 {
            self.set_list(&oc.target_list, t)?;
            self.opt_expr(oc.where_clause.as_deref())?;
        } else if oc.action != OnConflictAction::OnconflictNothing as i32 {
            return Err(Unknown);
        }
        Ok(())
    }

    /// UPDATE's SET list: each column is updated, and what it is set to is read.
    fn set_list(&mut self, targets: &[pb::Node], t: usize) -> Walk {
        self.cols(t, Mark::Update);
        for x in targets {
            let Some(NodeEnum::ResTarget(r)) = n(x) else {
                return Err(Unknown);
            };
            if !r.indirection.is_empty() {
                return Err(Unknown);
            }
            self.mark(t, &r.name, Mark::Update);
            self.opt_expr(r.val.as_deref())?;
        }
        Ok(())
    }

    fn update(&mut self, u: &pb::UpdateStmt) -> Walk {
        let ctes = self.with(u.with_clause.as_ref())?;
        let done = self.update_body(u);
        self.end_with(ctes);
        done
    }

    fn update_body(&mut self, u: &pb::UpdateStmt) -> Walk {
        let t = self.target(u.relation.as_ref())?;
        let done = (|| {
            for f in &u.from_clause {
                self.range_item(f)?;
            }
            self.set_list(&u.target_list, t)?;
            self.opt_expr(u.where_clause.as_deref())?;
            self.returning(u.returning_clause.as_ref())
        })();
        self.levels.pop();
        done
    }

    fn delete(&mut self, d: &pb::DeleteStmt) -> Walk {
        let ctes = self.with(d.with_clause.as_ref())?;
        let done = self.delete_body(d);
        self.end_with(ctes);
        done
    }

    fn delete_body(&mut self, d: &pb::DeleteStmt) -> Walk {
        let t = self.target(d.relation.as_ref())?;
        self.out[t].delete = true;
        let done = (|| {
            for f in &d.using_clause {
                self.range_item(f)?;
            }
            self.opt_expr(d.where_clause.as_deref())?;
            self.returning(d.returning_clause.as_ref())
        })();
        self.levels.pop();
        done
    }
}

/// How many columns the rows of an INSERT's query have, None when that takes
/// knowing more than the tree (a `*`, a set operation).
fn row_width(source: &NodeEnum) -> Option<usize> {
    let NodeEnum::SelectStmt(q) = source else { return None };
    if q.op != SetOperation::SetopNone as i32 {
        return None;
    }
    if !q.values_lists.is_empty() {
        return q
            .values_lists
            .iter()
            .map(|row| match n(row) {
                Some(NodeEnum::List(l)) => Some(l.items.len()),
                _ => None,
            })
            .try_fold(0, |m, k| k.map(|k| m.max(k)));
    }
    let star = q.target_list.iter().any(|t| match n(t) {
        Some(NodeEnum::ResTarget(t)) => match nb(t.val.as_deref()) {
            Some(NodeEnum::ColumnRef(c)) => c.fields.iter().any(|f| matches!(n(f), Some(NodeEnum::AStar(_)))),
            _ => false,
        },
        _ => true,
    });
    (!star).then_some(q.target_list.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// t (a, b, c), u (id, s), and s.v (x); anything else does not exist.
    struct Fixed;

    impl Relations for Fixed {
        fn relation(&mut self, schema: &str, name: &str) -> (String, Option<Vec<String>>) {
            let cols = |c: &[&str]| Some(c.iter().map(ToString::to_string).collect());
            match (schema, name) {
                ("", "t") => ("t".into(), cols(&["a", "b", "c"])),
                ("", "u") => ("u".into(), cols(&["id", "s"])),
                ("s", "v") => ("s.v".into(), cols(&["x"])),
                ("", n) => (n.into(), None),
                (s, n) => (format!("{s}.{n}"), None),
            }
        }
    }

    /// Each table's needs, written `table select/insert/update/delete`, with a
    /// column set as `a,b`, all as `*`, any as `-`, and none as `.`.
    fn needs(sql: &str) -> Option<Vec<String>> {
        let show = |c: &Option<Cols>| match c {
            None => ".".to_string(),
            Some(c) if c.all => "*".to_string(),
            Some(c) if c.names.is_empty() => "-".to_string(),
            Some(c) => c.names.iter().cloned().collect::<Vec<_>>().join(","),
        };
        let acc = accesses(sql, &mut Fixed)?;
        Some(
            acc.iter()
                .map(|a| {
                    format!(
                        "{} {}/{}/{}/{}",
                        a.table,
                        show(&a.select),
                        show(&a.insert),
                        show(&a.update),
                        if a.delete { "d" } else { "." }
                    )
                })
                .collect(),
        )
    }

    #[track_caller]
    fn is(sql: &str, want: &[&str]) {
        assert_eq!(
            needs(sql).unwrap_or_else(|| panic!("not followed: {sql}")),
            want,
            "{sql}"
        );
    }

    #[test]
    fn selects() {
        is("SELECT a FROM t", &["t a/././."]);
        is("SELECT count(*) FROM t", &["t -/././."]);
        is("SELECT * FROM t", &["t */././."]);
        is("SELECT t FROM t", &["t */././."]);
        is("SELECT x.* FROM t x", &["t */././."]);
        is("SELECT a AS b FROM t ORDER BY b", &["t a/././."]);
        is("SELECT a FROM t ORDER BY b", &["t a,b/././."]);
        is("SELECT a FROM t GROUP BY a HAVING max(c) > ''", &["t a,c/././."]);
        is(
            "SELECT a FROM t WHERE a IN (SELECT id FROM u WHERE s = t.c)",
            &["t a,c/././.", "u id,s/././."],
        );
        is("SELECT t.a FROM t JOIN u ON u.id = t.a", &["t a/././.", "u id/././."]);
        is("SELECT 1 FROM t JOIN u USING (id)", &["t -/././.", "u id/././."]);
        is("SELECT 1 FROM t NATURAL JOIN u", &["t */././.", "u */././."]);
        is("WITH w AS (SELECT a FROM t) SELECT * FROM w", &["t a/././."]);
        // Not recursive: inside its own query, t is the table.
        is("WITH t AS (SELECT b FROM t) SELECT * FROM t", &["t b/././."]);
        is("SELECT x.a FROM (SELECT * FROM t) x", &["t */././."]);
        is(
            "SELECT a FROM t UNION SELECT id FROM u ORDER BY 1",
            &["t a/././.", "u id/././."],
        );
        is("SELECT x FROM s.v", &["s.v x/././."]);
        is("SELECT s.v.x FROM s.v", &["s.v x/././."]);
        is("SELECT relname FROM pg_class", &[]);
        is(r#"SELECT * FROM "information_schema.tables""#, &[]);
        is("SELECT a FROM nope", &["nope */././."]);
        is("SELECT a FROM t WHERE a = $1 LIMIT $2", &["t a/././."]);
        is(
            "SELECT row_number() OVER (PARTITION BY b ORDER BY c) FROM t",
            &["t b,c/././."],
        );
        is("SELECT rowid FROM t", &["t */././."]);
        is("SELECT t.ctid, a FROM t", &["t */././."]);
        is(
            "SELECT (SELECT max(a) FROM t), (SELECT 1 FROM u)",
            &["t a/././.", "u -/././."],
        );
    }

    #[test]
    fn writes() {
        is("INSERT INTO t (a) VALUES (1)", &["t ./a/./."]);
        is("INSERT INTO t VALUES (1, 2)", &["t ./a,b/./."]);
        is("INSERT INTO t DEFAULT VALUES", &["t ./-/./."]);
        is("INSERT INTO t SELECT * FROM u", &["u */././.", "t ./*/./."]);
        is("INSERT INTO t (a) VALUES (1) RETURNING b", &["t b/a/./."]);
        is(
            "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = excluded.b",
            &["t a,b/a,b/b/."],
        );
        is("INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING", &["t ./a/./."]);
        is("UPDATE t SET c = 'y' WHERE a = 1", &["t a/./c/."]);
        is("UPDATE t SET c = c || 'z'", &["t c/./c/."]);
        is(
            "UPDATE t SET c = u.s FROM u WHERE u.id = t.a",
            &["t a/./c/.", "u id,s/././."],
        );
        is(
            "UPDATE t SET (a, b) = (SELECT id, 1 FROM u)",
            &["t ././a,b/.", "u id/././."],
        );
        is("DELETE FROM t WHERE b = 1 RETURNING a", &["t a,b/././d"]);
        is("DELETE FROM t USING u WHERE u.id = t.a", &["t a/././d", "u id/././."]);
    }

    #[test]
    fn what_it_does_not_follow() {
        for sql in [
            "SELECT a FROM t LIMIT 0, 1",
            "SELECT 1; SELECT 2",
            "SELECT a INTO x FROM t",
            "SELECT a FROM t TABLESAMPLE BERNOULLI (10)",
            "SELECT * FROM ROWS FROM (generate_series(1, 2))",
            "SELECT * FROM (t JOIN u ON true) AS j",
            "SELECT x.* FROM t AS x (p, q)",
            "UPDATE t SET a[1] = 2",
            "SELECT xmlelement(name x, a) FROM t",
            "SELECT json_object('k': a) FROM t",
            "SELECT nope.a FROM t",
            "TRUNCATE t",
            "COPY t TO STDOUT",
            "EXPLAIN SELECT b FROM t",
        ] {
            assert_eq!(needs(sql), None, "{sql}");
        }
    }
}
