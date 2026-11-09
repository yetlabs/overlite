//! The column each parameter meets, read from the tree with its scopes.
//!
//! The text reader (`overlite_engine::param_columns`) puts every table of the
//! statement in one list, so a bare column in a subquery is looked for in the outer
//! query's tables too, and when they disagree the parameter stays untyped. Here a
//! column is looked up in the scope it stands in, innermost first, as Postgres does.
//! A table found that way is named by a key of its own (`#<n>`), which the consumer
//! matches as an alias, so the same alias in two scopes cannot be confused.

use overlite_engine::{Meets, NamedTable, ParamColumn};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, SubLinkType};

/// The tables and the parameter columns of a statement, None when it does not parse.
pub(crate) fn param_columns(sql: &str) -> Option<(Vec<NamedTable>, Vec<ParamColumn>)> {
    let parsed = crate::ast::parse::shared(sql).ok()?;
    let [raw] = parsed.stmts.as_slice() else {
        return None;
    };
    let mut w = Walker::default();
    w.statement(raw.stmt.as_deref()?.node.as_ref()?);
    Some((w.tables, w.out))
}

/// Where a parameter of an INSERT goes: a column of its list, or a place in the row
/// of one without a list, which the table's column order names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slot {
    Column(String),
    Position(usize),
}

/// Everything the tree says of a statement's parameters, for Describe.
#[derive(Debug, Default)]
pub(crate) struct Facts {
    /// The tables and the column each parameter meets, as `param_columns` gives them.
    pub tables: Vec<NamedTable>,
    pub refs: Vec<ParamColumn>,
    /// An INSERT's table, as SQL text, and where each parameter that is a whole value
    /// of a VALUES row goes.
    pub insert: Option<(String, Vec<(usize, Slot)>)>,
    /// Parameters that are a row count: LIMIT, OFFSET, FETCH FIRST.
    pub row_counts: Vec<usize>,
}

/// The facts of a statement the AST path translates, None for any other: those keep
/// the text readers until their stage (docs/adr/0001-sql-ast.md).
pub(crate) fn facts(sql: &str) -> Option<Facts> {
    let parsed = crate::ast::parse::shared(sql).ok()?;
    let [raw] = parsed.stmts.as_slice() else {
        return None;
    };
    let stmt = raw.stmt.as_deref()?.node.as_ref()?;
    crate::ast::emit::statement(stmt).ok()?;
    let mut w = Walker::default();
    w.statement(stmt);
    let insert = match stmt {
        NodeEnum::InsertStmt(i) => insert_slots(i),
        _ => None,
    };
    Some(Facts {
        tables: w.tables,
        refs: w.out,
        insert,
        row_counts: w.row_counts,
    })
}

/// An INSERT's table and the slot of each parameter standing alone in a VALUES row.
fn insert_slots(i: &pb::InsertStmt) -> Option<(String, Vec<(usize, Slot)>)> {
    let r = i.relation.as_ref()?;
    let mut table = String::new();
    if !r.schemaname.is_empty() {
        table.push_str(&crate::ast::emit::ident(&r.schemaname));
        table.push('.');
    }
    table.push_str(&crate::ast::emit::ident(&r.relname));
    let cols: Vec<&str> = i
        .cols
        .iter()
        .filter_map(|c| match n(c) {
            Some(NodeEnum::ResTarget(t)) => Some(t.name.as_str()),
            _ => None,
        })
        .collect();
    let mut slots = Vec::new();
    if let Some(NodeEnum::SelectStmt(q)) = nb(i.select_stmt.as_deref()) {
        for row in &q.values_lists {
            let Some(NodeEnum::List(row)) = n(row) else { continue };
            for (k, item) in row.items.iter().enumerate() {
                let Some(NodeEnum::ParamRef(p)) = n(item) else { continue };
                let Ok(p) = usize::try_from(p.number) else { continue };
                if cols.is_empty() {
                    slots.push((p, Slot::Position(k)));
                } else if let Some(c) = cols.get(k) {
                    slots.push((p, Slot::Column((*c).to_string())));
                }
            }
        }
    }
    Some((table, slots))
}

#[derive(Default)]
struct Walker {
    tables: Vec<NamedTable>,
    /// Per scope, innermost last: (name a qualifier uses, index into tables), and the
    /// CTE names visible there, which are no tables.
    scopes: Vec<Vec<(String, usize)>>,
    ctes: Vec<String>,
    out: Vec<ParamColumn>,
    row_counts: Vec<usize>,
}

fn n(x: &pb::Node) -> Option<&NodeEnum> {
    x.node.as_ref()
}

fn nb(x: Option<&pb::Node>) -> Option<&NodeEnum> {
    x.and_then(n)
}

impl Walker {
    fn statement(&mut self, s: &NodeEnum) {
        match s {
            NodeEnum::SelectStmt(q) => self.select(q),
            NodeEnum::InsertStmt(i) => {
                self.with(i.with_clause.as_ref());
                if let Some(q) = nb(i.select_stmt.as_deref()) {
                    self.statement(q);
                }
            }
            NodeEnum::UpdateStmt(u) => {
                self.with(u.with_clause.as_ref());
                self.scopes.push(Vec::new());
                if let Some(r) = &u.relation {
                    self.range_var(r);
                }
                for f in &u.from_clause {
                    self.table_ref(n(f));
                }
                for t in &u.target_list {
                    if let Some(NodeEnum::ResTarget(t)) = n(t)
                        && let Some(v) = nb(t.val.as_deref())
                    {
                        // SET col = $n: the column of the written table.
                        if let NodeEnum::ParamRef(p) = v {
                            self.push_param(p.number, std::slice::from_ref(&t.name), Meets::Value, true);
                        } else {
                            self.expr(v);
                        }
                    }
                }
                self.opt_expr(u.where_clause.as_deref());
                self.scopes.pop();
            }
            NodeEnum::DeleteStmt(d) => {
                self.with(d.with_clause.as_ref());
                self.scopes.push(Vec::new());
                if let Some(r) = &d.relation {
                    self.range_var(r);
                }
                for f in &d.using_clause {
                    self.table_ref(n(f));
                }
                self.opt_expr(d.where_clause.as_deref());
                self.scopes.pop();
            }
            NodeEnum::PrepareStmt(p) => {
                if let Some(q) = nb(p.query.as_deref()) {
                    self.statement(q);
                }
            }
            NodeEnum::DeclareCursorStmt(d) => {
                if let Some(q) = nb(d.query.as_deref()) {
                    self.statement(q);
                }
            }
            _ => {}
        }
    }

    fn with(&mut self, w: Option<&pb::WithClause>) {
        let Some(w) = w else { return };
        for c in &w.ctes {
            if let Some(NodeEnum::CommonTableExpr(c)) = n(c) {
                self.ctes.push(c.ctename.clone());
                if let Some(q) = nb(c.ctequery.as_deref()) {
                    self.statement(q);
                }
            }
        }
    }

    fn select(&mut self, q: &pb::SelectStmt) {
        self.with(q.with_clause.as_ref());
        for count in [&q.limit_count, &q.limit_offset] {
            if let Some(NodeEnum::ParamRef(p)) = nb(count.as_deref())
                && let Ok(p) = usize::try_from(p.number)
            {
                self.row_counts.push(p);
            }
        }
        if let (Some(l), Some(r)) = (&q.larg, &q.rarg) {
            self.select(l);
            self.select(r);
            return;
        }
        self.scopes.push(Vec::new());
        for f in &q.from_clause {
            self.table_ref(n(f));
        }
        for t in &q.target_list {
            if let Some(NodeEnum::ResTarget(t)) = n(t) {
                self.opt_expr(t.val.as_deref());
            }
        }
        self.opt_expr(q.where_clause.as_deref());
        self.opt_expr(q.having_clause.as_deref());
        for g in &q.group_clause {
            self.opt_expr(Some(g));
        }
        for k in &q.sort_clause {
            if let Some(NodeEnum::SortBy(k)) = n(k) {
                self.opt_expr(k.node.as_deref());
            }
        }
        for row in &q.values_lists {
            if let Some(NodeEnum::List(l)) = n(row) {
                for x in &l.items {
                    self.opt_expr(Some(x));
                }
            }
        }
        self.scopes.pop();
    }

    fn table_ref(&mut self, f: Option<&NodeEnum>) {
        match f {
            Some(NodeEnum::RangeVar(r)) => self.range_var(r),
            Some(NodeEnum::JoinExpr(j)) => {
                self.table_ref(nb(j.larg.as_deref()));
                self.table_ref(nb(j.rarg.as_deref()));
                self.opt_expr(j.quals.as_deref());
            }
            Some(NodeEnum::RangeSubselect(s)) => {
                if let Some(q) = nb(s.subquery.as_deref()) {
                    self.statement(q);
                }
            }
            _ => {}
        }
    }

    fn range_var(&mut self, r: &pb::RangeVar) {
        if r.schemaname.is_empty() && self.ctes.contains(&r.relname) {
            return;
        }
        let i = self.tables.len();
        let mut parts = Vec::new();
        let mut text = String::new();
        if !r.schemaname.is_empty() {
            parts.push(r.schemaname.clone());
            text.push_str(&crate::ast::emit::ident(&r.schemaname));
            text.push('.');
        }
        parts.push(r.relname.clone());
        text.push_str(&crate::ast::emit::ident(&r.relname));
        self.tables.push(NamedTable {
            text,
            parts,
            alias: Some(format!("#{i}")),
        });
        let name = r
            .alias
            .as_ref()
            .map_or_else(|| r.relname.clone(), |a| a.aliasname.clone());
        if let Some(s) = self.scopes.last_mut() {
            s.push((name, i));
        }
    }

    fn opt_expr(&mut self, e: Option<&pb::Node>) {
        if let Some(e) = nb(e) {
            self.expr(e);
        }
    }

    /// Records that parameter p meets the column named by fields.
    fn push_param(&mut self, p: i32, fields: &[String], meets: Meets, written_table: bool) {
        let Ok(param) = usize::try_from(p) else { return };
        let Some((column, qualifier)) = fields.split_last() else {
            return;
        };
        let key = if written_table {
            self.scopes.last().and_then(|s| s.first()).map(|&(_, i)| i)
        } else {
            self.resolve(qualifier, column)
        };
        self.out.push(ParamColumn {
            param,
            qualifier: key.map_or_else(|| qualifier.to_vec(), |i| vec![format!("#{i}")]),
            column: column.clone(),
            meets,
        });
    }

    /// The table a column reference means: by its qualifier, innermost scope first;
    /// a bare name in the innermost scope with a single table is that table's.
    fn resolve(&self, qualifier: &[String], _column: &str) -> Option<usize> {
        if let Some(q) = qualifier.last() {
            return self
                .scopes
                .iter()
                .rev()
                .find_map(|s| s.iter().find(|(name, _)| name == q).map(|&(_, i)| i));
        }
        match self.scopes.last().map(Vec::as_slice) {
            Some([(_, i)]) => Some(*i),
            _ => None,
        }
    }

    fn column(e: Option<&NodeEnum>) -> Option<Vec<String>> {
        let Some(NodeEnum::ColumnRef(c)) = e else { return None };
        c.fields
            .iter()
            .map(|f| match n(f) {
                Some(NodeEnum::String(s)) => Some(s.sval.clone()),
                _ => None,
            })
            .collect()
    }

    fn expr(&mut self, e: &NodeEnum) {
        match e {
            NodeEnum::AExpr(a) => {
                use pb::AExprKind as K;
                let l = nb(a.lexpr.as_deref());
                let r = nb(a.rexpr.as_deref());
                match K::try_from(a.kind) {
                    Ok(K::AexprOp | K::AexprDistinct | K::AexprNotDistinct) => {
                        if let (Some(cols), Some(NodeEnum::ParamRef(p))) = (Self::column(l), r) {
                            self.push_param(p.number, &cols, Meets::Value, false);
                        } else if let (Some(NodeEnum::ParamRef(p)), Some(cols)) = (l, Self::column(r)) {
                            self.push_param(p.number, &cols, Meets::Value, false);
                        }
                    }
                    Ok(K::AexprOpAny) => {
                        if let (Some(cols), Some(NodeEnum::ParamRef(p))) = (Self::column(l), r) {
                            self.push_param(p.number, &cols, Meets::Array, false);
                        }
                    }
                    Ok(K::AexprIn | K::AexprBetween | K::AexprNotBetween) => {
                        if let (Some(cols), Some(NodeEnum::List(items))) = (Self::column(l), r) {
                            for x in &items.items {
                                if let Some(NodeEnum::ParamRef(p)) = n(x) {
                                    self.push_param(p.number, &cols, Meets::Value, false);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                for x in [l, r].into_iter().flatten() {
                    self.expr(x);
                }
            }
            NodeEnum::BoolExpr(b) => b.args.iter().filter_map(n).for_each(|x| self.expr(x)),
            NodeEnum::List(l) => l.items.iter().filter_map(n).for_each(|x| self.expr(x)),
            NodeEnum::NullTest(t) => self.opt_expr(t.arg.as_deref()),
            NodeEnum::TypeCast(t) => self.opt_expr(t.arg.as_deref()),
            NodeEnum::FuncCall(f) => f.args.iter().filter_map(n).for_each(|x| self.expr(x)),
            NodeEnum::CoalesceExpr(c) => c.args.iter().filter_map(n).for_each(|x| self.expr(x)),
            NodeEnum::CaseExpr(c) => {
                self.opt_expr(c.arg.as_deref());
                for w in &c.args {
                    if let Some(NodeEnum::CaseWhen(w)) = n(w) {
                        self.opt_expr(w.expr.as_deref());
                        self.opt_expr(w.result.as_deref());
                    }
                }
                self.opt_expr(c.defresult.as_deref());
            }
            NodeEnum::SubLink(s) => {
                if s.sub_link_type == SubLinkType::AnySublink as i32 {
                    self.opt_expr(s.testexpr.as_deref());
                }
                if let Some(q) = nb(s.subselect.as_deref()) {
                    self.statement(q);
                }
            }
            _ => {}
        }
    }
}
