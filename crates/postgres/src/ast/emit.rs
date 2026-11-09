//! The printer: a Postgres parse tree written as SQLite SQL.
//!
//! It knows a slice of the grammar and answers `Err` for anything else, so that the
//! statement goes to the text passes instead. Nothing here guesses: a node it has not
//! been taught is unsupported, whatever its children.
//!
//! Parentheses come from SQLite's precedence, not from the source: the tree has none.
//! Postgres binds `||` looser than `+` and SQLite tighter, so `a + b || c` has to be
//! written `(a + b) || c` for SQLite to read what Postgres read.

use crate::ast::env::Context;
use crate::ast::labels::{Label, figure_colname};
use crate::ast::types::{self, Base, Ty, Typer};
use overlite_core::text::{quote_ident, quote_literal};
use pg_query::NodeEnum;
use pg_query::protobuf::{
    self as pb, BoolExprType, CteMaterialize, JoinType, LimitOption, NullTestType, SetOperation, SortByDir,
    SortByNulls, SubLinkType, a_const::Val,
};
use std::rc::Rc;

/// Why a statement is left to the text passes: the first node the printer does not know.
pub(crate) type Unsupported = &'static str;

/// The refusals that depend on the catalog, not on the statement's shape: `%` whose
/// operands' types are not known, and a boolean column that may hold text.
pub(crate) const TYPE_REFUSAL_OPERANDS: Unsupported = "% of operands of unknown type";
pub(crate) const TYPE_REFUSAL_BOOLEAN: Unsupported = "boolean column holding text";

type Out = Result<(), Unsupported>;

/// How tightly an expression binds in SQLite, loosest first. An operand is written in
/// parentheses when it binds looser than its place asks.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Prec {
    Or,
    And,
    Not,
    /// `=`, `<>`, IS, IN, BETWEEN, LIKE, IS NULL.
    Equality,
    /// `<`, `>`, `<=`, `>=`.
    Compare,
    Add,
    Mul,
    /// `||`.
    Concat,
    Unary,
    Atom,
}

/// Functions the text passes leave as they are, by name: a call to one of them is
/// written as it came. None is named by any pass (each was looked for in the rewrite,
/// types and session code), and the differential test holds those the corpus calls.
const PLAIN_FUNCTIONS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "lower",
    "upper",
    "length",
    "abs",
    "round",
    "nullif",
    "coalesce",
    // Strings.
    "replace",
    "substr",
    "strpos",
    "split_part",
    "lpad",
    "rpad",
    "left",
    "right",
    "reverse",
    "repeat",
    "concat",
    "char_length",
    "octet_length",
    "initcap",
    "chr",
    "ascii",
    "md5",
    "hex",
    "typeof",
    // Numbers.
    "floor",
    "ceil",
    "ceiling",
    "sign",
    "sqrt",
    "power",
    "mod",
    "exp",
    "ln",
    "pi",
    "sin",
    "cos",
    "tan",
    "cot",
    "asin",
    "acos",
    "atan",
    "atan2",
    "degrees",
    "radians",
    "dec_add",
    "dec_sub",
    "dec_mul",
    "dec_div",
    "dec_cmp",
    // Aggregates.
    "any_value",
    "bool_and",
    "bool_or",
    "bit_and",
    "bit_or",
    "bit_xor",
    "stddev_pop",
    "stddev_samp",
    "var_pop",
    "var_samp",
    "covar_pop",
    "covar_samp",
    "corr",
    "regr_avgx",
    "regr_avgy",
    "regr_count",
    "regr_intercept",
    "regr_r2",
    "regr_slope",
    "regr_sxx",
    "regr_sxy",
    "regr_syy",
    // Time, on what the engine's own functions read.
    "date_trunc",
    "to_char",
    "age",
];

/// Writes a statement of the slice, or says which node it stopped at: the untyped
/// translation, which the engine's type passes finish.
pub(crate) fn statement(stmt: &NodeEnum) -> Result<String, Unsupported> {
    statement_labelled(stmt, None)
}

/// Writes a statement of the slice whose result columns take the given labels, one
/// per item of its result list (None: as the statement itself names it), when the
/// list has as many items. The session expands some calls before translating
/// (`current_user` into its value, `nextval('s')` into a number), and the labels are
/// those of the statement as the client sent it (`labels::result_labels`).
pub(crate) fn statement_labelled(stmt: &NodeEnum, labels: Option<&[Option<String>]>) -> Result<String, Unsupported> {
    Ok(print(stmt, labels, None)?.sql)
}

/// A statement printed with the type environment, and what it found on the way.
#[derive(Debug)]
pub(crate) struct Printed {
    /// The SQLite SQL, ending in the engine's mark (`overlite_engine::TYPED_MARK`) when
    /// typed: no type pass of the engine is to read it again.
    pub sql: String,
    /// The type each parameter meets where the environment knows it, `$1` first.
    pub params: Vec<Ty>,
}

/// Writes a statement of the slice with the type environment of ctx: what depends on
/// an operand's type (numeric exactness, date and time arithmetic and comparison, the
/// session's TimeZone, booleans, tsvector, `%`) is written from the types, and the
/// output is final.
pub(crate) fn typed(
    stmt: &NodeEnum,
    labels: Option<&[Option<String>]>,
    ctx: Rc<Context>,
) -> Result<Printed, Unsupported> {
    print(stmt, labels, Some(Typer::new(ctx)))
}

fn print(stmt: &NodeEnum, labels: Option<&[Option<String>]>, typer: Option<Typer>) -> Result<Printed, Unsupported> {
    let typed = typer.is_some();
    let mut e = Emitter {
        out: String::with_capacity(128),
        labels,
        item_casts: typed && matches!(stmt, NodeEnum::SelectStmt(s) if s.with_clause.is_none()),
        typer,
        params: Vec::new(),
        in_form: false,
    };
    match stmt {
        NodeEnum::SelectStmt(s) => e.select(s, true)?,
        NodeEnum::InsertStmt(s) => e.insert(s)?,
        NodeEnum::UpdateStmt(s) => e.update(s)?,
        NodeEnum::DeleteStmt(s) => e.delete(s)?,
        _ => return Err("statement kind"),
    }
    if let Some(t) = &mut e.typer {
        // The result columns' integer widths, which a declared INTEGER does not tell
        // the protocol (the engine's mark carries them).
        let marks: Vec<&str> = result_cols(t, stmt)
            .iter()
            .map(|c| if c.rowid { "" } else { c.ty.mark() })
            .collect();
        // A boolean column that may hold text keeps the text passes' readings of both
        // forms (resolve/booleans.rs).
        if t.mixed {
            return Err(TYPE_REFUSAL_BOOLEAN);
        }
        e.out.push(' ');
        e.out.push_str(overlite_engine::TYPED_MARK);
        if marks.iter().any(|m| !m.is_empty()) {
            e.out.push(' ');
            e.out.push_str(&marks.join(","));
        }
        e.out.push_str("*/");
    }
    Ok(Printed {
        sql: e.out,
        params: e.params,
    })
}

/// The columns of a statement's result: a SELECT's, a set operation's where its arms
/// agree, a write's RETURNING.
fn result_cols(t: &mut Typer, stmt: &NodeEnum) -> Vec<types::Col> {
    fn select(t: &mut Typer, s: &pb::SelectStmt) -> Vec<types::Col> {
        let (Some(l), Some(r)) = (&s.larg, &s.rarg) else {
            return t.select_cols(s);
        };
        let ctes = t.push_with(s.with_clause.as_ref());
        let mut cols = select(t, l);
        let right = select(t, r);
        t.pop_with(ctes);
        for (c, o) in cols.iter_mut().zip(&right) {
            if c.ty != o.ty || o.rowid {
                c.ty = Ty::ANY;
            }
        }
        cols
    }
    let (rel, ret) = match stmt {
        NodeEnum::SelectStmt(s) => return select(t, s),
        NodeEnum::InsertStmt(s) => (s.relation.as_ref(), s.returning_clause.as_ref()),
        NodeEnum::UpdateStmt(s) => (s.relation.as_ref(), s.returning_clause.as_ref()),
        NodeEnum::DeleteStmt(s) => (s.relation.as_ref(), s.returning_clause.as_ref()),
        _ => return Vec::new(),
    };
    let (Some(rel), Some(ret)) = (rel, ret) else {
        return Vec::new();
    };
    t.push_target(rel);
    let cols = t.targets(&ret.exprs);
    t.pop_scope();
    cols
}

struct Emitter<'a> {
    out: String,
    /// Labels for the result list in place of its own (`statement_labelled`).
    labels: Option<&'a [Option<String>]>,
    /// The type environment; None for the untyped translation.
    typer: Option<Typer>,
    /// The type each parameter meets, as far as the printer has read.
    params: Vec<Ty>,
    /// Inside a numeric written in the form a numeric column has: nothing in it is
    /// written in it again.
    in_form: bool,
    /// The outermost SELECT's list is still to come: an item whose type only Postgres
    /// knows (MIN, MAX, COALESCE, SUM of numerics, DATE_TRUNC, a scalar subquery...) is
    /// cast to it there, so that the protocol sends it as one.
    item_casts: bool,
}

fn node(n: &pb::Node) -> Result<&NodeEnum, Unsupported> {
    n.node.as_ref().ok_or("empty node")
}

fn boxed(n: Option<&pb::Node>) -> Result<&NodeEnum, Unsupported> {
    n.and_then(|n| n.node.as_ref()).ok_or("empty node")
}

/// The text of a String node.
fn string(n: &pb::Node) -> Result<&str, Unsupported> {
    match node(n)? {
        NodeEnum::String(s) => Ok(&s.sval),
        _ => Err("name part"),
    }
}

/// An identifier as SQLite reads it: bare when it is a lowercase word no dialect
/// reserves, quoted otherwise.
pub(crate) fn ident(name: &str) -> String {
    let plain = name
        .bytes()
        .enumerate()
        .all(|(i, c)| c == b'_' || c.is_ascii_lowercase() || (i > 0 && c.is_ascii_digit()))
        && !name.is_empty();
    if plain && !is_reserved(name) {
        name.to_string()
    } else {
        quote_ident(name)
    }
}

/// Words SQLite or Postgres reserves, which a name must be quoted to be.
fn is_reserved(w: &str) -> bool {
    const WORDS: &[&str] = &[
        "abort",
        "action",
        "add",
        "after",
        "all",
        "alter",
        "always",
        "analyze",
        "and",
        "any",
        "array",
        "as",
        "asc",
        "asymmetric",
        "attach",
        "autoincrement",
        "before",
        "begin",
        "between",
        "both",
        "by",
        "cascade",
        "case",
        "cast",
        "check",
        "collate",
        "column",
        "commit",
        "conflict",
        "constraint",
        "create",
        "cross",
        "current",
        "current_catalog",
        "current_date",
        "current_role",
        "current_time",
        "current_timestamp",
        "current_user",
        "database",
        "default",
        "deferrable",
        "deferred",
        "delete",
        "desc",
        "detach",
        "distinct",
        "do",
        "drop",
        "each",
        "else",
        "end",
        "escape",
        "except",
        "exclude",
        "exclusive",
        "exists",
        "explain",
        "fail",
        "false",
        "fetch",
        "filter",
        "first",
        "following",
        "for",
        "foreign",
        "from",
        "full",
        "generated",
        "glob",
        "grant",
        "group",
        "groups",
        "having",
        "if",
        "ignore",
        "immediate",
        "in",
        "index",
        "indexed",
        "initially",
        "inner",
        "insert",
        "instead",
        "intersect",
        "into",
        "is",
        "isnull",
        "join",
        "key",
        "last",
        "lateral",
        "leading",
        "left",
        "like",
        "limit",
        "localtime",
        "localtimestamp",
        "match",
        "materialized",
        "natural",
        "no",
        "not",
        "nothing",
        "notnull",
        "null",
        "nulls",
        "of",
        "offset",
        "on",
        "only",
        "or",
        "order",
        "others",
        "outer",
        "over",
        "partition",
        "placing",
        "plan",
        "pragma",
        "preceding",
        "primary",
        "query",
        "raise",
        "range",
        "recursive",
        "references",
        "regexp",
        "reindex",
        "release",
        "rename",
        "replace",
        "restrict",
        "returning",
        "right",
        "rollback",
        "row",
        "rows",
        "savepoint",
        "select",
        "session_user",
        "set",
        "some",
        "symmetric",
        "table",
        "temp",
        "temporary",
        "then",
        "ties",
        "to",
        "trailing",
        "transaction",
        "trigger",
        "true",
        "unbounded",
        "union",
        "unique",
        "update",
        "user",
        "using",
        "vacuum",
        "values",
        "variadic",
        "view",
        "virtual",
        "when",
        "where",
        "window",
        "with",
        "without",
    ];
    WORDS.contains(&w)
}

impl Emitter<'_> {
    fn push(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn list(&mut self, items: &[pb::Node], f: fn(&mut Self, &NodeEnum) -> Out) -> Out {
        for (i, n) in items.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            f(self, node(n)?)?;
        }
        Ok(())
    }

    fn expr_node(&mut self, n: &NodeEnum) -> Out {
        self.expr(n, Prec::Or)
    }

    // ---- the type environment ----

    fn typed(&self) -> bool {
        self.typer.is_some()
    }

    /// n's type, Any without the environment.
    fn ty(&mut self, n: &NodeEnum) -> Ty {
        match &mut self.typer {
            Some(t) => t.ty(n),
            None => Ty::ANY,
        }
    }

    /// Records that the parameter n, if it is one, meets a value of type ty.
    fn note_param(&mut self, n: &NodeEnum, ty: Ty) {
        if !self.typed() || !ty.known() {
            return;
        }
        if let NodeEnum::ParamRef(p) = n
            && let Ok(i) = usize::try_from(p.number - 1)
        {
            if self.params.len() <= i {
                self.params.resize(i + 1, Ty::UNKNOWN);
            }
            if !self.params[i].known() {
                self.params[i] = ty;
            }
        }
    }

    fn push_from(&mut self, from: &[pb::Node]) {
        if let Some(t) = &mut self.typer {
            t.push_from(from);
        }
    }

    fn push_target(&mut self, r: &pb::RangeVar) {
        if let Some(t) = &mut self.typer {
            t.push_target(r);
        }
    }

    fn pop_scope(&mut self) {
        if let Some(t) = &mut self.typer {
            t.pop_scope();
        }
    }

    // ---- statements ----

    /// A SELECT; top says its list is the result's, whose unnamed items get Postgres'
    /// labels.
    fn select(&mut self, s: &pb::SelectStmt, top: bool) -> Out {
        if s.into_clause.is_some() {
            return Err("SELECT INTO");
        }
        if !s.locking_clause.is_empty() {
            return Err("FOR UPDATE");
        }
        if !s.window_clause.is_empty() {
            return Err("WINDOW");
        }
        let (ctes, outputs) = match &mut self.typer {
            Some(t) => (t.push_with(s.with_clause.as_ref()), t.outputs.take()),
            None => (0, None),
        };
        if let Some(w) = &s.with_clause {
            self.with(w)?;
        }
        let op = SetOperation::try_from(s.op).map_err(|_| "set operation")?;
        match op {
            SetOperation::SetopNone | SetOperation::Undefined => {
                self.push_from(&s.from_clause);
                self.select_core(s, top)?;
                if !s.sort_clause.is_empty()
                    && let Some(t) = &mut self.typer
                {
                    t.outputs = Some(t.targets(&s.target_list));
                }
                self.order_limit(s)?;
                self.pop_scope();
            }
            SetOperation::SetopUnion | SetOperation::SetopIntersect | SetOperation::SetopExcept => {
                let (Some(l), Some(r)) = (&s.larg, &s.rarg) else {
                    return Err("set operation arms");
                };
                // SQLite reads a compound left to right with one precedence; Postgres binds
                // INTERSECT tighter and keeps a parenthesized right arm whole. A set
                // operation on the right is a grouping SQLite cannot write.
                if r.op != SetOperation::SetopNone as i32 {
                    return Err("set operation on the right of a set operation");
                }
                self.set_member(l, top)?;
                self.push(match op {
                    SetOperation::SetopUnion => " UNION ",
                    SetOperation::SetopIntersect => " INTERSECT ",
                    _ => " EXCEPT ",
                });
                if s.all {
                    self.push("ALL ");
                }
                self.set_member(r, false)?;
                // The ORDER BY of a set operation reads its output columns alone.
                if !s.sort_clause.is_empty()
                    && let Some(t) = &mut self.typer
                {
                    let cols = t.select_cols(l);
                    t.push_from(&[]);
                    t.outputs = Some(cols);
                }
                self.order_limit(s)?;
                if !s.sort_clause.is_empty() {
                    self.pop_scope();
                }
            }
        }
        if let Some(t) = &mut self.typer {
            t.pop_with(ctes);
            t.outputs = outputs;
        }
        Ok(())
    }

    /// A member of a UNION: SQLite takes no parentheses there, so one with its own
    /// ORDER BY, LIMIT or WITH is not written.
    fn set_member(&mut self, s: &pb::SelectStmt, top: bool) -> Out {
        if !s.sort_clause.is_empty() || s.limit_count.is_some() || s.limit_offset.is_some() || s.with_clause.is_some() {
            return Err("set member with ORDER BY or LIMIT");
        }
        self.select(s, top)
    }

    fn with(&mut self, w: &pb::WithClause) -> Out {
        self.push("WITH ");
        if w.recursive {
            self.push("RECURSIVE ");
        }
        for (i, c) in w.ctes.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            let NodeEnum::CommonTableExpr(c) = node(c)? else {
                return Err("CTE");
            };
            if c.search_clause.is_some() || c.cycle_clause.is_some() {
                return Err("CTE SEARCH or CYCLE");
            }
            self.push(&ident(&c.ctename));
            if !c.aliascolnames.is_empty() {
                self.push("(");
                self.names(&c.aliascolnames)?;
                self.push(")");
            }
            self.push(" AS ");
            match CteMaterialize::try_from(c.ctematerialized) {
                Ok(CteMaterialize::Always) => self.push("MATERIALIZED "),
                Ok(CteMaterialize::Never) => self.push("NOT MATERIALIZED "),
                _ => {}
            }
            self.push("(");
            match boxed(c.ctequery.as_deref())? {
                NodeEnum::SelectStmt(q) => self.select(q, false)?,
                _ => return Err("CTE that writes"),
            }
            self.push(")");
        }
        self.push(" ");
        Ok(())
    }

    fn names(&mut self, items: &[pb::Node]) -> Out {
        for (i, n) in items.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            let name = string(n)?;
            self.push(&ident(name));
        }
        Ok(())
    }

    fn select_core(&mut self, s: &pb::SelectStmt, top: bool) -> Out {
        if !s.values_lists.is_empty() {
            self.push("VALUES ");
            for (i, row) in s.values_lists.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                let NodeEnum::List(row) = node(row)? else {
                    return Err("VALUES row");
                };
                self.push("(");
                self.list(&row.items, Self::expr_node)?;
                self.push(")");
            }
            return Ok(());
        }
        self.push("SELECT ");
        match s.distinct_clause.as_slice() {
            [] => {}
            [only] if only.node.is_none() => self.push("DISTINCT "),
            _ => return Err("DISTINCT ON"),
        }
        self.targets(&s.target_list, top)?;
        if !s.from_clause.is_empty() {
            self.push(" FROM ");
            self.list(&s.from_clause, Self::table_ref)?;
        }
        if let Some(w) = &s.where_clause {
            self.push(" WHERE ");
            self.expr(node(w)?, Prec::Or)?;
        }
        if !s.group_clause.is_empty() {
            if s.group_distinct {
                return Err("GROUP BY DISTINCT");
            }
            self.push(" GROUP BY ");
            self.list(&s.group_clause, Self::expr_node)?;
        }
        if let Some(h) = &s.having_clause {
            self.push(" HAVING ");
            self.expr(node(h)?, Prec::Or)?;
        }
        Ok(())
    }

    /// A select list or a RETURNING list. With labels, an item that names nothing is
    /// given the label Postgres gives it.
    fn targets(&mut self, items: &[pb::Node], labels: bool) -> Out {
        // Given labels go to the result list, once: a set operation's first arm.
        let given = if labels {
            self.labels.take().filter(|l| l.len() == items.len())
        } else {
            None
        };
        let casts = labels && std::mem::take(&mut self.item_casts);
        for (i, t) in items.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            let NodeEnum::ResTarget(t) = node(t)? else {
                return Err("target");
            };
            if !t.indirection.is_empty() {
                return Err("target indirection");
            }
            let val = boxed(t.val.as_deref())?;
            match casts.then(|| self.item_cast(val)).flatten() {
                Some((ty, name)) => self.cast_item(val, ty, name)?,
                None => self.expr(val, Prec::Or)?,
            }
            if !t.name.is_empty() {
                self.push(" AS ");
                self.push(&quote_ident(&t.name));
            } else if let Some(Some(l)) = given.map(|g| &g[i]) {
                self.push(" AS ");
                self.push(&quote_ident(l));
            } else if labels {
                match figure_colname(val) {
                    Label::Itself => {}
                    Label::Named(l) => {
                        self.push(" AS ");
                        self.push(&quote_ident(&l));
                    }
                }
            }
        }
        Ok(())
    }

    /// The type a select-list item is cast to: one whose value is an argument's or
    /// follows from it (MIN, MAX, COALESCE, GREATEST, LEAST, NULLIF, SUM and AVG of
    /// numerics and intervals, ABS and ROUND of numerics, DATE_TRUNC), or a scalar
    /// subquery, of a date or time type or a numeric.
    fn item_cast(&mut self, val: &NodeEnum) -> Option<(Ty, &'static str)> {
        let passes = match val {
            NodeEnum::CoalesceExpr(_) | NodeEnum::MinMaxExpr(_) => true,
            NodeEnum::AExpr(a) => a.kind == pb::AExprKind::AexprNullif as i32,
            NodeEnum::SubLink(s) => s.sub_link_type == SubLinkType::ExprSublink as i32,
            NodeEnum::FuncCall(f) => matches!(
                func_name(f),
                Some(
                    "min"
                        | "max"
                        | "any_value"
                        | "sum"
                        | "avg"
                        | "abs"
                        | "round"
                        | "ceil"
                        | "ceiling"
                        | "floor"
                        | "date_trunc"
                )
            ),
            _ => false,
        };
        if !passes {
            return None;
        }
        let ty = self.ty(val);
        let name = ty.cast_name()?;
        if let NodeEnum::FuncCall(f) = val {
            match func_name(f) {
                Some("sum" | "avg") if !ty.is_numeric() && !ty.is(Base::Interval) => return None,
                Some("abs" | "round" | "ceil" | "ceiling" | "floor") if !ty.is_numeric() => return None,
                _ => {}
            }
        }
        Some((ty, name))
    }

    /// A select-list item cast to the type item_cast found: ABS and ROUND of a numeric
    /// exact, SQLite's being a double's.
    fn cast_item(&mut self, val: &NodeEnum, ty: Ty, name: &str) -> Out {
        let form = ty.is_numeric() && !self.in_form;
        if form {
            self.push("(CAST(overlite_numeric(");
            self.in_form = true;
        }
        self.push("overlite_cast(");
        match val {
            NodeEnum::FuncCall(f) if ty.is_numeric() && matches!(func_name(f), Some("abs" | "round")) => {
                self.push("overlite_");
                self.push(func_name(f).unwrap_or(""));
                self.push("_exact(");
                self.list(&f.args, Self::expr_node)?;
                self.push(")");
            }
            _ => self.expr(val, Prec::Or)?,
        }
        self.push(", '");
        self.push(name);
        self.push("')");
        if form {
            self.in_form = false;
            self.push(") AS DECIMALTEXT) COLLATE DECIMAL)");
        }
        Ok(())
    }

    fn order_limit(&mut self, s: &pb::SelectStmt) -> Out {
        if !s.sort_clause.is_empty() {
            self.push(" ORDER BY ");
            for (i, k) in s.sort_clause.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                let NodeEnum::SortBy(k) = node(k)? else {
                    return Err("sort key");
                };
                let key = boxed(k.node.as_deref())?;
                self.expr(key, Prec::Or)?;
                // An interval sorts by its span (`1 mon` after `29 days`).
                if self.typed() && self.ty(key).is(Base::Interval) {
                    self.push(" COLLATE INTERVAL");
                }
                let desc = match SortByDir::try_from(k.sortby_dir) {
                    Ok(SortByDir::SortbyDesc) => {
                        self.push(" DESC");
                        true
                    }
                    Ok(SortByDir::SortbyAsc) => {
                        self.push(" ASC");
                        false
                    }
                    Ok(SortByDir::SortbyDefault | SortByDir::Undefined) => false,
                    _ => return Err("ORDER BY USING"),
                };
                // Postgres sorts NULL above every value, SQLite below: say it.
                let first = match SortByNulls::try_from(k.sortby_nulls) {
                    Ok(SortByNulls::SortbyNullsFirst) => true,
                    Ok(SortByNulls::SortbyNullsLast) => false,
                    _ => desc,
                };
                self.push(if first { " NULLS FIRST" } else { " NULLS LAST" });
            }
            if let Some(t) = &mut self.typer {
                t.outputs = None;
            }
        }
        match LimitOption::try_from(s.limit_option) {
            Ok(LimitOption::WithTies) => return Err("FETCH WITH TIES"),
            Ok(_) => {}
            Err(_) => return Err("limit option"),
        }
        // LIMIT ALL is a NULL count; SQLite's no-limit is -1, and it takes OFFSET only
        // after a LIMIT.
        let count = s.limit_count.as_deref().map(node).transpose()?;
        let count = count.filter(|c| !matches!(c, NodeEnum::AConst(k) if k.isnull));
        let offset = s.limit_offset.as_deref().map(node).transpose()?;
        if count.is_some() || offset.is_some() {
            self.push(" LIMIT ");
            match count {
                Some(c) => {
                    self.note_param(c, Ty::of(Base::Int8));
                    self.expr(c, Prec::Or)?;
                }
                None => self.push("-1"),
            }
            if let Some(o) = offset {
                self.note_param(o, Ty::of(Base::Int8));
                self.push(" OFFSET ");
                self.expr(o, Prec::Or)?;
            }
        }
        Ok(())
    }

    fn table_ref(&mut self, n: &NodeEnum) -> Out {
        match n {
            NodeEnum::RangeVar(r) => self.range_var(r, true),
            NodeEnum::JoinExpr(j) => {
                let jt = JoinType::try_from(j.jointype).map_err(|_| "join type")?;
                if j.join_using_alias.is_some() || j.alias.is_some() {
                    return Err("join alias");
                }
                self.table_ref(boxed(j.larg.as_deref())?)?;
                if j.is_natural {
                    self.push(" NATURAL");
                }
                let cross =
                    jt == JoinType::JoinInner && j.quals.is_none() && j.using_clause.is_empty() && !j.is_natural;
                self.push(match jt {
                    _ if cross => " CROSS JOIN ",
                    JoinType::JoinInner => " JOIN ",
                    JoinType::JoinLeft => " LEFT JOIN ",
                    JoinType::JoinRight => " RIGHT JOIN ",
                    JoinType::JoinFull => " FULL JOIN ",
                    _ => return Err("join type"),
                });
                // A join on the right of a join is a group of its own.
                let right = boxed(j.rarg.as_deref())?;
                if matches!(right, NodeEnum::JoinExpr(_)) {
                    self.push("(");
                    self.table_ref(right)?;
                    self.push(")");
                } else {
                    self.table_ref(right)?;
                }
                if !j.using_clause.is_empty() {
                    self.push(" USING (");
                    self.names(&j.using_clause)?;
                    self.push(")");
                }
                if let Some(q) = &j.quals {
                    self.push(" ON ");
                    self.expr(node(q)?, Prec::Or)?;
                }
                Ok(())
            }
            NodeEnum::RangeSubselect(r) => {
                if r.lateral {
                    return Err("LATERAL");
                }
                let NodeEnum::SelectStmt(q) = boxed(r.subquery.as_deref())? else {
                    return Err("subquery");
                };
                self.push("(");
                self.select(q, false)?;
                self.push(")");
                if let Some(a) = &r.alias {
                    if !a.colnames.is_empty() {
                        return Err("derived column aliases");
                    }
                    self.push(" AS ");
                    self.push(&ident(&a.aliasname));
                }
                Ok(())
            }
            _ => Err("FROM item"),
        }
    }

    /// A table: its schema as written, except the two the text passes resolve.
    fn range_var(&mut self, r: &pb::RangeVar, alias: bool) -> Out {
        if !r.inh {
            return Err("ONLY");
        }
        if !r.catalogname.is_empty() {
            return Err("catalog-qualified name");
        }
        // Catalog names are views and functions the text passes rewrite; a table
        // named like one is theirs too.
        if matches!(r.schemaname.as_str(), "pg_catalog" | "information_schema")
            || r.relname.starts_with("pg_")
            || r.relname.starts_with("_overlite")
        {
            return Err("catalog relation");
        }
        // A stored name of another schema is `schema.table` in SQLite: the text passes
        // read a quoted name with a dot.
        if r.relname.contains('.') {
            return Err("name with a dot");
        }
        if !r.schemaname.is_empty() {
            self.push(&ident(&r.schemaname));
            self.push(".");
        }
        self.push(&ident(&r.relname));
        if let Some(a) = &r.alias {
            if !alias {
                return Err("alias of a written table");
            }
            if !a.colnames.is_empty() {
                return Err("table column aliases");
            }
            self.push(" AS ");
            self.push(&ident(&a.aliasname));
        }
        Ok(())
    }

    fn insert(&mut self, s: &pb::InsertStmt) -> Out {
        if s.with_clause.is_some() {
            return Err("WITH INSERT");
        }
        if s.on_conflict_clause.is_some() {
            return Err("ON CONFLICT");
        }
        // OVERRIDING SYSTEM or USER VALUE decides what an identity column gets; the
        // printer has no way to say it, so it is not dropped but left to the passes.
        if s.r#override > pb::OverridingKind::OverridingNotSet as i32 {
            return Err("OVERRIDING");
        }
        self.push("INSERT INTO ");
        let r = s.relation.as_ref().ok_or("INSERT target")?;
        self.range_var(r, false)?;
        if !s.cols.is_empty() {
            self.push(" (");
            for (i, c) in s.cols.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                let NodeEnum::ResTarget(t) = node(c)? else {
                    return Err("INSERT column");
                };
                if !t.indirection.is_empty() {
                    return Err("INSERT column indirection");
                }
                self.push(&ident(&t.name));
            }
            self.push(")");
        }
        match s.select_stmt.as_deref() {
            None => self.push(" DEFAULT VALUES"),
            Some(q) => {
                let NodeEnum::SelectStmt(q) = node(q)? else {
                    return Err("INSERT source");
                };
                self.push(" ");
                self.select(q, false)?;
            }
        }
        self.push_target(r);
        self.returning(s.returning_clause.as_ref())?;
        self.pop_scope();
        Ok(())
    }

    fn returning(&mut self, r: Option<&pb::ReturningClause>) -> Out {
        let Some(r) = r else { return Ok(()) };
        if !r.options.is_empty() {
            return Err("RETURNING OLD/NEW");
        }
        if r.exprs.is_empty() {
            return Ok(());
        }
        self.push(" RETURNING ");
        self.targets(&r.exprs, true)
    }

    fn update(&mut self, s: &pb::UpdateStmt) -> Out {
        if s.with_clause.is_some() {
            return Err("WITH UPDATE");
        }
        if !s.from_clause.is_empty() {
            return Err("UPDATE FROM");
        }
        self.push("UPDATE ");
        let r = s.relation.as_ref().ok_or("UPDATE target")?;
        self.range_var(r, false)?;
        self.push_target(r);
        self.push(" SET ");
        for (i, t) in s.target_list.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            let NodeEnum::ResTarget(t) = node(t)? else {
                return Err("SET item");
            };
            if !t.indirection.is_empty() {
                return Err("SET indirection");
            }
            let val = boxed(t.val.as_deref())?;
            if matches!(val, NodeEnum::MultiAssignRef(_)) {
                return Err("SET (a, b) =");
            }
            if let Some(typer) = &mut self.typer {
                let col = pb::Node {
                    node: Some(NodeEnum::String(pb::String { sval: t.name.clone() })),
                };
                let ty = typer.column(std::slice::from_ref(&col)).map_or(Ty::ANY, |c| c.ty);
                self.note_param(val, ty);
            }
            self.push(&ident(&t.name));
            self.push(" = ");
            self.expr(val, Prec::Or)?;
        }
        if let Some(w) = &s.where_clause {
            self.push(" WHERE ");
            self.expr(node(w)?, Prec::Or)?;
        }
        self.returning(s.returning_clause.as_ref())?;
        self.pop_scope();
        Ok(())
    }

    fn delete(&mut self, s: &pb::DeleteStmt) -> Out {
        if s.with_clause.is_some() {
            return Err("WITH DELETE");
        }
        if !s.using_clause.is_empty() {
            return Err("DELETE USING");
        }
        self.push("DELETE FROM ");
        let r = s.relation.as_ref().ok_or("DELETE target")?;
        self.range_var(r, false)?;
        self.push_target(r);
        if let Some(w) = &s.where_clause {
            self.push(" WHERE ");
            self.expr(node(w)?, Prec::Or)?;
        }
        self.returning(s.returning_clause.as_ref())?;
        self.pop_scope();
        Ok(())
    }

    // ---- expressions ----

    /// Writes n where an expression binding at least as tightly as min may stand. With
    /// the environment, a numeric that is not a column is written in the form a numeric
    /// column has (resolve/numeric.rs): `(CAST(overlite_numeric(e) AS DECIMALTEXT)
    /// COLLATE DECIMAL)`, which compares as a number with a number and exactly with
    /// another numeric.
    fn expr(&mut self, n: &NodeEnum, min: Prec) -> Out {
        if self.typed() && !self.in_form && self.wants_form(n) {
            self.push("(CAST(overlite_numeric(");
            self.in_form = true;
            let r = self.expr_bare(n);
            self.in_form = false;
            r?;
            self.push(") AS DECIMALTEXT) COLLATE DECIMAL)");
            return Ok(());
        }
        let p = prec(n);
        if p < min {
            self.push("(");
            self.expr_bare(n)?;
            self.push(")");
            Ok(())
        } else {
            self.expr_bare(n)
        }
    }

    /// Whether n is a numeric written as a call (a cast, a function, COALESCE, CASE,
    /// exact arithmetic, a scalar subquery), which has no affinity of its own.
    fn wants_form(&mut self, n: &NodeEnum) -> bool {
        let call = match n {
            // EXTRACT gives a number already, which compares as one.
            NodeEnum::FuncCall(f) => {
                f.funcformat != pb::CoercionForm::CoerceSqlSyntax as i32
                    && !matches!(
                        func_name(f),
                        Some("dec_add" | "dec_sub" | "dec_mul" | "dec_div" | "dec_round" | "dec_cmp")
                    )
            }
            NodeEnum::TypeCast(_) | NodeEnum::CoalesceExpr(_) | NodeEnum::MinMaxExpr(_) | NodeEnum::CaseExpr(_) => true,
            NodeEnum::SubLink(s) => s.sub_link_type == SubLinkType::ExprSublink as i32,
            NodeEnum::AExpr(a) => {
                a.kind == pb::AExprKind::AexprNullif as i32
                    || (a.kind == pb::AExprKind::AexprOp as i32
                        && a.lexpr.is_some()
                        && matches!(op_name(a), Some("+" | "-" | "*" | "/")))
            }
            _ => false,
        };
        call && self.ty(n).is_numeric()
    }

    fn expr_bare(&mut self, n: &NodeEnum) -> Out {
        match n {
            NodeEnum::ColumnRef(c) => {
                if c.fields.len() > 1 && matches!(string(&c.fields[0]), Ok("pg_catalog" | "information_schema")) {
                    return Err("catalog column");
                }
                if let Some(t) = &mut self.typer
                    && !matches!(c.fields.last().and_then(|f| f.node.as_ref()), Some(NodeEnum::AStar(_)))
                {
                    // Read for what it is: a boolean holding text sends the statement to
                    // the text passes.
                    t.column(&c.fields);
                }
                for (i, f) in c.fields.iter().enumerate() {
                    if i > 0 {
                        self.push(".");
                    }
                    match node(f)? {
                        NodeEnum::String(s) => {
                            if matches!(s.sval.as_str(), "ctid" | "xmin" | "xmax" | "oid" | "tableoid") {
                                return Err("system column");
                            }
                            self.push(&ident(&s.sval));
                        }
                        NodeEnum::AStar(_) => self.push("*"),
                        _ => return Err("column reference"),
                    }
                }
                Ok(())
            }
            NodeEnum::ParamRef(p) => {
                self.push("$");
                self.push(&p.number.to_string());
                Ok(())
            }
            NodeEnum::AConst(k) => self.constant(k),
            NodeEnum::TypeCast(t) => self.cast(t),
            NodeEnum::AExpr(a) => self.a_expr(a),
            NodeEnum::BoolExpr(b) => self.bool_expr(b),
            NodeEnum::NullTest(t) => {
                if t.argisrow {
                    return Err("row IS NULL");
                }
                self.expr(boxed(t.arg.as_deref())?, Prec::Compare)?;
                self.push(match NullTestType::try_from(t.nulltesttype) {
                    Ok(NullTestType::IsNull) => " IS NULL",
                    Ok(NullTestType::IsNotNull) => " IS NOT NULL",
                    _ => return Err("null test"),
                });
                Ok(())
            }
            NodeEnum::CoalesceExpr(c) => {
                self.push("coalesce(");
                self.zoned_args(&c.args)?;
                self.push(")");
                Ok(())
            }
            NodeEnum::CaseExpr(c) => {
                self.push("CASE");
                if let Some(a) = &c.arg {
                    self.push(" ");
                    self.expr(node(a)?, Prec::Or)?;
                }
                for w in &c.args {
                    let NodeEnum::CaseWhen(w) = node(w)? else {
                        return Err("CASE arm");
                    };
                    self.push(" WHEN ");
                    self.expr(boxed(w.expr.as_deref())?, Prec::Or)?;
                    self.push(" THEN ");
                    self.expr(boxed(w.result.as_deref())?, Prec::Or)?;
                }
                if let Some(d) = &c.defresult {
                    self.push(" ELSE ");
                    self.expr(node(d)?, Prec::Or)?;
                }
                self.push(" END");
                Ok(())
            }
            NodeEnum::FuncCall(f) => self.func_call(f),
            NodeEnum::SubLink(s) => self.sub_link(s),
            NodeEnum::MinMaxExpr(m) => {
                // The engine's GREATEST and LEAST skip NULLs, as Postgres' do; over
                // numerics dec_greatest and dec_least compare them as numbers.
                let dec = self.typed() && self.ty(n).is_numeric();
                self.push(match pb::MinMaxOp::try_from(m.op) {
                    Ok(pb::MinMaxOp::IsGreatest) if dec => "dec_greatest(",
                    Ok(pb::MinMaxOp::IsLeast) if dec => "dec_least(",
                    Ok(pb::MinMaxOp::IsGreatest) => "greatest(",
                    Ok(pb::MinMaxOp::IsLeast) => "least(",
                    _ => return Err("GREATEST or LEAST"),
                });
                self.zoned_args(&m.args)?;
                self.push(")");
                Ok(())
            }
            NodeEnum::BooleanTest(t) => {
                use pb::BoolTestType as B;
                // SQLite has IS [NOT] TRUE and FALSE; IS UNKNOWN it has not.
                let word = match B::try_from(t.booltesttype) {
                    Ok(B::IsTrue) => " IS TRUE",
                    Ok(B::IsNotTrue) => " IS NOT TRUE",
                    Ok(B::IsFalse) => " IS FALSE",
                    Ok(B::IsNotFalse) => " IS NOT FALSE",
                    _ => return Err("IS UNKNOWN"),
                };
                self.expr(boxed(t.arg.as_deref())?, Prec::Compare)?;
                self.push(word);
                Ok(())
            }
            NodeEnum::SqlvalueFunction(f) if self.typed() => {
                use pb::SqlValueFunctionOp as O;
                // CURRENT_DATE is the date in the session's zone.
                match O::try_from(f.op) {
                    Ok(O::SvfopCurrentDate) => self.push("overlite_current_date()"),
                    Ok(O::SvfopCurrentTimestamp) => self.push("overlite_now(julianday('now'))"),
                    _ => return Err("SQL value function"),
                }
                Ok(())
            }
            _ => Err("expression"),
        }
    }

    /// The arguments of COALESCE, GREATEST or LEAST: with the environment, beside a
    /// timestamptz a timestamp or a date is read in the session's zone, as Postgres
    /// makes it one.
    fn zoned_args(&mut self, args: &[pb::Node]) -> Out {
        if !self.typed() {
            return self.list(args, Self::expr_node);
        }
        let nodes: Vec<&NodeEnum> = args.iter().map(node).collect::<Result<_, _>>()?;
        let tys: Vec<Ty> = nodes.iter().map(|n| self.ty(n)).collect();
        let common = types::common(&tys);
        for (i, (n, t)) in nodes.iter().zip(&tys).enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.note_param(n, common);
            if common.is(Base::Timestamptz) && (t.is(Base::Timestamp) || t.is(Base::Date)) {
                self.push("overlite_tz_in(");
                self.expr(n, Prec::Or)?;
                self.push(")");
            } else {
                self.value(n, common, Prec::Or)?;
            }
        }
        Ok(())
    }

    fn constant(&mut self, k: &pb::AConst) -> Out {
        if k.isnull {
            self.push("NULL");
            return Ok(());
        }
        match k.val.as_ref().ok_or("constant")? {
            Val::Ival(i) => self.push(&i.ival.to_string()),
            // A number with more digits than a double keeps is a numeric's text.
            Val::Fval(f) if self.typed() && is_long_number(&f.fval) => {
                let (sign, digits) = match f.fval.strip_prefix('-') {
                    Some(d) => ("-", d),
                    None => ("", f.fval.as_str()),
                };
                self.push(sign);
                self.push("(CAST(overlite_numeric(");
                self.push(&quote_literal(digits));
                self.push(") AS DECIMALTEXT) COLLATE DECIMAL)");
            }
            Val::Fval(f) => self.push(&f.fval),
            Val::Boolval(b) => self.push(if b.boolval { "true" } else { "false" }),
            Val::Sval(s) => self.push(&quote_literal(&s.sval)),
            Val::Bsval(_) => return Err("bit string"),
        }
        Ok(())
    }

    /// n as a value of type target, which it meets: an untyped literal or a parameter
    /// read by that type's input function where SQLite would compare its text (a date,
    /// a timestamp, a timestamptz in the session's zone, a boolean), anything else as it
    /// is.
    fn value(&mut self, n: &NodeEnum, target: Ty, min: Prec) -> Out {
        let is_lit = matches!(n, NodeEnum::AConst(k) if !k.isnull && matches!(k.val, Some(Val::Sval(_))));
        let is_param = matches!(n, NodeEnum::ParamRef(_));
        if !self.typed() || !(is_lit || is_param) {
            return self.expr(n, min);
        }
        self.note_param(n, target);
        match target {
            t if t.is(Base::Timestamptz) => {
                self.push("overlite_tz_in(");
                self.expr(n, Prec::Or)?;
                self.push(")");
            }
            t if t.is(Base::Timestamp) || t.is(Base::Date) => {
                self.push("overlite_cast(");
                self.expr(n, Prec::Or)?;
                self.push(if t.is(Base::Date) {
                    ", 'date')"
                } else {
                    ", 'timestamp')"
                });
            }
            t if t.is(Base::Bool) && is_lit => {
                let NodeEnum::AConst(k) = n else {
                    return self.expr(n, min);
                };
                let Some(Val::Sval(s)) = &k.val else {
                    return self.expr(n, min);
                };
                if let Some(b) = overlite_core::parse_bool(&s.sval) {
                    self.push(if b { "1" } else { "0" });
                } else {
                    self.push("pg_bool(");
                    self.push(&quote_literal(&s.sval));
                    self.push(")");
                }
            }
            _ => self.expr(n, min)?,
        }
        Ok(())
    }

    /// A cast: to text SQLite's own CAST, to boolean the engine's boolin, to any other
    /// type the engine knows the engine's input function. With the environment, a
    /// timestamptz cast to text, a date or a time is read in the session's zone, and a
    /// value cast to timestamptz that is not one is a wall clock there.
    fn cast(&mut self, t: &pb::TypeCast) -> Out {
        let ty = t.type_name.as_ref().ok_or("cast type")?;
        let arg = boxed(t.arg.as_deref())?;
        let name = cast_type(ty)?;
        if self.typed() {
            let target = types::of_type_name(ty);
            self.note_param(arg, target);
            let from = self.ty(arg);
            if from.is(Base::Timestamptz) {
                match target.base {
                    Base::Text | Base::Varchar if !target.array => {
                        self.push("overlite_tz_text(");
                        self.expr(arg, Prec::Or)?;
                        self.push(")");
                        return Ok(());
                    }
                    Base::Date | Base::Time | Base::Timestamp if !target.array => {
                        self.push("overlite_cast(overlite_tz_wall(");
                        self.expr(arg, Prec::Or)?;
                        self.push("), ");
                        self.push(&quote_literal(&name));
                        self.push(")");
                        return Ok(());
                    }
                    _ => {}
                }
            } else if target.is(Base::Timestamptz) {
                self.push("overlite_cast(overlite_tz_in(");
                self.expr(arg, Prec::Or)?;
                self.push("), ");
                self.push(&quote_literal(&name));
                self.push(")");
                return Ok(());
            }
        }
        match name.as_str() {
            // hstore is stored as its text: a literal is read by hstore_in, anything
            // else is one already.
            "hstore" => {
                if is_string(arg) {
                    self.push("hstore_in(");
                    self.expr(arg, Prec::Or)?;
                    self.push(")");
                } else {
                    self.expr(arg, Prec::Atom)?;
                }
            }
            // jsonb compares and sorts as jsonb (rewrite_jsonb_collate).
            "jsonb" => {
                self.push("(overlite_cast(");
                self.expr(arg, Prec::Or)?;
                self.push(", 'jsonb') COLLATE JSONB)");
            }
            "text" => {
                self.push("CAST(");
                self.expr(arg, Prec::Or)?;
                self.push(" AS text)");
            }
            "boolean" => {
                self.push("pg_bool(");
                self.expr(arg, Prec::Or)?;
                self.push(")");
            }
            _ => {
                self.push("overlite_cast(");
                self.expr(arg, Prec::Or)?;
                self.push(", ");
                self.push(&quote_literal(&name));
                self.push(")");
            }
        }
        Ok(())
    }

    fn a_expr(&mut self, a: &pb::AExpr) -> Out {
        use pb::AExprKind as K;
        let kind = K::try_from(a.kind).map_err(|_| "operator kind")?;
        let op = match a.name.as_slice() {
            [n] => string(n)?,
            _ => return Err("qualified operator"),
        };
        match kind {
            K::AexprOp => {
                let Some(r) = a.rexpr.as_deref() else {
                    return Err("postfix operator");
                };
                let r = node(r)?;
                let Some(l) = a.lexpr.as_deref() else {
                    if !matches!(op, "-" | "+") {
                        return Err("prefix operator");
                    }
                    self.push(op);
                    // `- -1` written together would open a comment.
                    if prec(r) == Prec::Unary {
                        self.push(" ");
                    }
                    return self.expr(r, Prec::Unary);
                };
                let l = node(l)?;
                if self.typed() {
                    return self.typed_op(a, op, l, r);
                }
                let p = binary_prec(op).ok_or("operator")?;
                self.expr(l, p)?;
                self.push(" ");
                self.push(op);
                self.push(" ");
                self.expr(r, next(p))
            }
            K::AexprDistinct | K::AexprNotDistinct => {
                let (l, r) = (boxed(a.lexpr.as_deref())?, boxed(a.rexpr.as_deref())?);
                let (tl, tr) = (self.ty(l), self.ty(r));
                self.value(l, tr, Prec::Compare)?;
                self.push(if kind == K::AexprDistinct {
                    " IS DISTINCT FROM "
                } else {
                    " IS NOT DISTINCT FROM "
                });
                self.value(r, tl, Prec::Compare)
            }
            K::AexprNullif => {
                self.push("nullif(");
                self.expr(boxed(a.lexpr.as_deref())?, Prec::Or)?;
                self.push(", ");
                self.expr(boxed(a.rexpr.as_deref())?, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            K::AexprIn => {
                let NodeEnum::List(items) = boxed(a.rexpr.as_deref())? else {
                    return Err("IN list");
                };
                let l = boxed(a.lexpr.as_deref())?;
                let tl = self.ty(l);
                self.expr(l, Prec::Compare)?;
                self.push(if op == "<>" { " NOT IN (" } else { " IN (" });
                for (i, x) in items.items.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.value(node(x)?, tl, Prec::Or)?;
                }
                self.push(")");
                Ok(())
            }
            K::AexprLike | K::AexprIlike => {
                // Postgres' LIKE tells case apart and escapes with a backslash unless
                // ESCAPE says otherwise; SQLite's folds ASCII case and has no escape of
                // its own. LIKE is GLOB over the pattern overlite_like_glob writes, and
                // ILIKE SQLite's LIKE over overlite_ilike_pattern's (rewrite/like.rs, which
                // does the same for the text passes).
                let (pattern, escape) = like_pattern(boxed(a.rexpr.as_deref())?)?;
                let (word, func) = match op {
                    "~~" => (" GLOB ", "overlite_like_glob("),
                    "!~~" => (" NOT GLOB ", "overlite_like_glob("),
                    "~~*" => (" LIKE ", "overlite_ilike_pattern("),
                    "!~~*" => (" NOT LIKE ", "overlite_ilike_pattern("),
                    _ => return Err("LIKE operator"),
                };
                let l = boxed(a.lexpr.as_deref())?;
                self.note_param(l, Ty::TEXT);
                self.note_param(pattern, Ty::TEXT);
                self.expr(l, Prec::Compare)?;
                self.push(word);
                self.push(func);
                self.expr(pattern, Prec::Or)?;
                self.push(", ");
                match escape {
                    Some(e) => self.expr(e, Prec::Or)?,
                    None => self.push("'\\'"),
                }
                self.push(")");
                if kind == K::AexprIlike {
                    self.push(" ESCAPE '\\'");
                }
                Ok(())
            }
            K::AexprBetween | K::AexprNotBetween => {
                let NodeEnum::List(b) = boxed(a.rexpr.as_deref())? else {
                    return Err("BETWEEN bounds");
                };
                let [lo, hi] = b.items.as_slice() else {
                    return Err("BETWEEN bounds");
                };
                let l = boxed(a.lexpr.as_deref())?;
                let tl = self.ty(l);
                self.expr(l, Prec::Compare)?;
                self.push(if kind == K::AexprBetween {
                    " BETWEEN "
                } else {
                    " NOT BETWEEN "
                });
                self.value(node(lo)?, tl, Prec::Compare)?;
                self.push(" AND ");
                self.value(node(hi)?, tl, Prec::Compare)
            }
            _ => Err("operator kind"),
        }
    }

    /// A binary operator with the environment: by its operands' types.
    fn typed_op(&mut self, a: &pb::AExpr, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        match op {
            "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" => self.comparison(op, l, r),
            "+" | "-" | "*" | "/" => self.arithmetic(a, op),
            "||" => {
                let (tl, tr) = (self.ty(l), self.ty(r));
                if tl.is(Base::Tsvector) || tr.is(Base::Tsvector) {
                    // Two vectors' lexemes, the right one's positions shifted.
                    self.push("ts_concat(");
                    self.expr(l, Prec::Or)?;
                    self.push(", ");
                    self.expr(r, Prec::Or)?;
                    self.push(")");
                    return Ok(());
                }
                self.note_param(l, Ty::TEXT);
                self.note_param(r, Ty::TEXT);
                self.expr(l, Prec::Concat)?;
                self.push(" || ");
                self.expr(r, Prec::Unary)
            }
            "%" => {
                // pg_trgm's similarity on text, a remainder on numbers: only the types tell.
                let (tl, tr) = (self.ty(l), self.ty(r));
                let text = |t: Ty, n: &NodeEnum| t.is_texty() || is_string(n);
                if text(tl, l) || text(tr, r) {
                    self.push("overlite_trgm_match(");
                    self.expr(l, Prec::Or)?;
                    self.push(", ");
                    self.expr(r, Prec::Or)?;
                    self.push(", '%')");
                    return Ok(());
                }
                if (tl.is_integer() || tl.is_numeric()) && (tr.is_integer() || tr.is_numeric()) {
                    self.expr(l, Prec::Mul)?;
                    self.push(" % ");
                    return self.expr(r, Prec::Concat);
                }
                Err(TYPE_REFUSAL_OPERANDS)
            }
            _ => Err("operator"),
        }
    }

    /// A comparison, by its sides' types: two intervals by their spans, a date with a
    /// timestamp at its midnight, a timestamp with a timestamptz in the session's zone,
    /// an untyped literal or a parameter as the type of the other side.
    fn comparison(&mut self, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        let p = binary_prec(op).ok_or("operator")?;
        let (tl, tr) = (self.ty(l), self.ty(r));
        let (tl, tr) = (self.guess(l, tl, tr), self.guess(r, tr, tl));
        let num = |t: Ty| t.is_number() || t.is_numeric();
        if (tl.is(Base::Interval) || tr.is(Base::Interval)) && !num(tl) && !num(tr) {
            self.note_param(l, tr);
            self.note_param(r, tl);
            self.push("overlite_interval_cmp(");
            self.expr(l, Prec::Or)?;
            self.push(", ");
            self.expr(r, Prec::Or)?;
            self.push(") ");
            self.push(op);
            self.push(" 0");
            return Ok(());
        }
        let stamp = |t: Ty| t.is(Base::Timestamp) || t.is(Base::Timestamptz);
        if tl.is(Base::Date) && stamp(tr) {
            self.widen(l, tr)?;
            self.push(" ");
            self.push(op);
            self.push(" ");
            return self.expr(r, next(p));
        }
        if stamp(tl) && tr.is(Base::Date) {
            self.expr(l, p)?;
            self.push(" ");
            self.push(op);
            self.push(" ");
            return self.widen(r, tl);
        }
        if tl.is(Base::Timestamp) && tr.is(Base::Timestamptz) {
            self.widen(l, tr)?;
        } else {
            self.value(l, tr, p)?;
        }
        self.push(" ");
        self.push(op);
        self.push(" ");
        if tr.is(Base::Timestamp) && tl.is(Base::Timestamptz) {
            self.widen(r, tl)
        } else {
            self.value(r, tl, next(p))
        }
    }

    /// A date or a timestamp made the timestamp or timestamptz it is compared with:
    /// its midnight, a wall clock in the session's zone.
    fn widen(&mut self, n: &NodeEnum, to: Ty) -> Out {
        if to.is(Base::Timestamptz) {
            self.push("overlite_cast(overlite_tz_in(");
            self.expr(n, Prec::Or)?;
            self.push("), 'timestamptz')");
        } else {
            self.push("overlite_cast(");
            self.expr(n, Prec::Or)?;
            self.push(", 'timestamp')");
        }
        Ok(())
    }

    /// The type an untyped literal compared with other is taken for: other's, or where
    /// other's type is not known, an interval when the literal reads as one (`29 days`,
    /// `1 day 02:00`), as the text passes took it.
    fn guess(&self, n: &NodeEnum, t: Ty, other: Ty) -> Ty {
        if t.base != Base::Unknown {
            return t;
        }
        if other.known() {
            return other;
        }
        if let NodeEnum::AConst(k) = n
            && let Some(Val::Sval(s)) = &k.val
        {
            let date_like = s.sval.len() >= 10 && s.sval.as_bytes()[4] == b'-';
            let worded = s.sval.bytes().any(|c| c.is_ascii_alphabetic() || c == b':');
            if worded && !date_like && overlite_engine::interval_parts(&s.sval).is_some() {
                return Ty::of(Base::Interval);
            }
        }
        t
    }

    /// `+ - * /` with the environment: a chain of one of them at one level (`a + b - c`)
    /// is exact (overlite_numeric_op) when a term is a numeric, and over dates, times and
    /// intervals (overlite_tadd, overlite_tsub, overlite_tmul) when a term is one, each
    /// side's kind named; otherwise SQLite's.
    fn arithmetic(&mut self, top: &pb::AExpr, op: &str) -> Out {
        let additive = matches!(op, "+" | "-");
        // The chain: the left spine of operators of the same class.
        let mut ops: Vec<(&str, &NodeEnum)> = Vec::new();
        let mut a = top;
        let first = loop {
            let r = boxed(a.rexpr.as_deref())?;
            let l = boxed(a.lexpr.as_deref())?;
            ops.push((op_name(a).ok_or("operator")?, r));
            match l {
                NodeEnum::AExpr(x)
                    if x.kind == pb::AExprKind::AexprOp as i32
                        && x.lexpr.is_some()
                        && op_name(x).is_some_and(|o| {
                            if additive {
                                matches!(o, "+" | "-")
                            } else {
                                matches!(o, "*" | "/")
                            }
                        }) =>
                {
                    a = x;
                }
                _ => break l,
            }
        };
        ops.reverse();
        let mut terms: Vec<&NodeEnum> = vec![first];
        terms.extend(ops.iter().map(|(_, t)| *t));
        let tys: Vec<Ty> = terms.iter().map(|t| self.ty(t)).collect();
        let temporal = tys.iter().any(|t| t.is_temporal());
        let decimal = tys.iter().any(|t| t.is_numeric());
        if temporal {
            // Each step's kinds: the running result's, and the term's.
            let mut acc = tys[0];
            let mut steps = Vec::new();
            for (k, (o, _)) in ops.iter().enumerate() {
                let next_ty = tys[k + 1];
                if matches!(*o, "+") && next_ty.base == Base::Unknown {
                    // `ts + $1`: Postgres reads the parameter as an interval.
                    if matches!(acc.base, Base::Date | Base::Time | Base::Timestamp | Base::Timestamptz) {
                        self.note_param(terms[k + 1], Ty::of(Base::Interval));
                    }
                }
                steps.push((acc.kind(), next_ty.kind()));
                acc = if additive {
                    types::arithmetic(o, acc, next_ty)
                } else {
                    Ty::of(Base::Interval)
                };
            }
            for (o, _) in ops.iter().rev() {
                self.push(match *o {
                    "+" => "overlite_tadd(",
                    "-" => "overlite_tsub(",
                    _ => "overlite_tmul(",
                });
            }
            self.expr(first, Prec::Or)?;
            for (k, (o, t)) in ops.iter().enumerate() {
                self.push(", ");
                self.expr(t, Prec::Or)?;
                self.push(", '");
                self.push(steps[k].0);
                self.push("', '");
                self.push(steps[k].1);
                self.push("'");
                if !additive {
                    self.push(", '");
                    self.push(o);
                    self.push("'");
                }
                self.push(")");
            }
            return Ok(());
        }
        if decimal {
            for (k, t) in terms.iter().enumerate() {
                let other = tys[usize::from(k == 0)];
                self.note_param(t, other);
            }
            // Exact, at Postgres' scale; the cast says it is a numeric.
            self.push("overlite_cast(");
            for _ in &ops {
                self.push("overlite_numeric_op(");
            }
            self.decimal_term(first)?;
            for (o, t) in &ops {
                self.push(", ");
                self.decimal_term(t)?;
                self.push(", '");
                self.push(o);
                self.push("')");
            }
            self.push(", 'numeric')");
            return Ok(());
        }
        for (k, t) in terms.iter().enumerate() {
            let other = tys[usize::from(k == 0)];
            if other.is_number() {
                self.note_param(t, other);
            }
        }
        let p = binary_prec(op).ok_or("operator")?;
        self.expr(first, p)?;
        for (o, t) in &ops {
            self.push(" ");
            self.push(o);
            self.push(" ");
            self.expr(t, next(p))?;
        }
        Ok(())
    }

    /// A term of exact arithmetic: a literal with a point as its text, not SQLite's double.
    fn decimal_term(&mut self, t: &NodeEnum) -> Out {
        if let NodeEnum::AConst(k) = t
            && let Some(Val::Fval(f)) = &k.val
            && !f.fval.starts_with('-')
            && !f.fval.bytes().all(|c| c.is_ascii_digit())
        {
            self.push(&quote_literal(&f.fval));
            return Ok(());
        }
        self.expr(t, Prec::Or)
    }

    fn bool_expr(&mut self, b: &pb::BoolExpr) -> Out {
        match BoolExprType::try_from(b.boolop) {
            Ok(BoolExprType::NotExpr) => {
                let [arg] = b.args.as_slice() else {
                    return Err("NOT");
                };
                let arg = node(arg)?;
                // NOT (x IN (SELECT ...)) is how Postgres keeps x NOT IN (SELECT ...).
                if let NodeEnum::SubLink(s) = arg
                    && s.sub_link_type == SubLinkType::AnySublink as i32
                    && s.oper_name.is_empty()
                {
                    return self.in_subquery(s, true);
                }
                self.push("NOT ");
                self.expr(arg, Prec::Not)
            }
            Ok(t @ (BoolExprType::AndExpr | BoolExprType::OrExpr)) => {
                let (p, word) = if t == BoolExprType::AndExpr {
                    (Prec::And, " AND ")
                } else {
                    (Prec::Or, " OR ")
                };
                for (i, a) in b.args.iter().enumerate() {
                    if i > 0 {
                        self.push(word);
                    }
                    self.expr(node(a)?, next(p))?;
                }
                Ok(())
            }
            _ => Err("boolean operator"),
        }
    }

    fn func_call(&mut self, f: &pb::FuncCall) -> Out {
        if self.typed()
            && f.funcformat == pb::CoercionForm::CoerceSqlSyntax as i32
            && let [schema, name] = f.funcname.as_slice()
            && string(schema)? == "pg_catalog"
        {
            return self.sql_syntax(string(name)?, f);
        }
        let name = match f.funcname.as_slice() {
            [n] => string(n)?,
            _ => return Err("qualified function"),
        };
        if !PLAIN_FUNCTIONS.contains(&name) && !name.starts_with("overlite_") {
            return Err("function");
        }
        if f.over.is_some()
            || f.agg_filter.is_some()
            || !f.agg_order.is_empty()
            || f.agg_within_group
            || f.func_variadic
        {
            return Err("aggregate or window clause");
        }
        if f.funcformat != pb::CoercionForm::CoerceExplicitCall as i32 {
            return Err("special function syntax");
        }
        if self.typed() {
            let args: Vec<&NodeEnum> = f.args.iter().map(node).collect::<Result<_, _>>()?;
            match (name, args.as_slice()) {
                // bool_and and bool_or are MIN and MAX over the truth values, read as
                // Postgres' boolin reads them.
                ("bool_and" | "bool_or", [x]) if !f.agg_distinct => {
                    self.push(if name == "bool_or" {
                        "pg_bool(max(overlite_bool("
                    } else {
                        "pg_bool(min(overlite_bool("
                    });
                    self.expr(x, Prec::Or)?;
                    self.push(")))");
                    return Ok(());
                }
                // What the session writes for a value stored or cast to a type: one
                // given for a timestamptz is a wall clock in the session's zone, and a
                // timestamptz written on goes with its offset, which no zone misreads.
                ("overlite_assign" | "overlite_cast", [x, target]) if is_string(target) => {
                    return self.session_cast(name, x, target);
                }
                // The length of a tsvector is its count of lexemes.
                ("length", [x]) if self.ty(x).is(Base::Tsvector) => {
                    self.push("ts_length(");
                    self.expr(x, Prec::Or)?;
                    self.push(")");
                    return Ok(());
                }
                // DATE_TRUNC of a timestamptz truncates its wall clock in the session's
                // zone, read back as the instant it is there; of a date, the zone's
                // midnight.
                ("date_trunc", [unit, x]) => match self.ty(x).base {
                    Base::Timestamptz => {
                        self.push("overlite_tz_in(date_trunc(");
                        self.expr(unit, Prec::Or)?;
                        self.push(", overlite_tz_wall(");
                        self.expr(x, Prec::Or)?;
                        self.push(")))");
                        return Ok(());
                    }
                    Base::Date => {
                        self.push("overlite_tz_in(date_trunc(");
                        self.expr(unit, Prec::Or)?;
                        self.push(", ");
                        self.expr(x, Prec::Or)?;
                        self.push("))");
                        return Ok(());
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        self.push(name);
        self.push("(");
        if f.agg_star {
            self.push("*");
        } else {
            if f.agg_distinct {
                self.push("DISTINCT ");
            }
            self.list(&f.args, Self::expr_node)?;
        }
        self.push(")");
        Ok(())
    }

    /// overlite_assign or overlite_cast of x to the type target names, as the session
    /// writes them, with the environment: a value given for a timestamptz is read in the
    /// session's zone; a timestamptz written to text goes as its text there, to a date
    /// or a time as its wall clock there (a cast), and to anything else with its offset.
    fn session_cast(&mut self, name: &str, x: &NodeEnum, target: &NodeEnum) -> Out {
        let NodeEnum::AConst(k) = target else {
            return Err("cast target");
        };
        let Some(Val::Sval(to)) = &k.val else {
            return Err("cast target");
        };
        let (to_ty, from) = (Ty::of_decl(&to.sval), self.ty(x));
        let low = to.sval.to_ascii_lowercase();
        let wall = to_ty.is(Base::Date) || to_ty.is(Base::Time) || to_ty.is(Base::Timestamp);
        let (pre, post) = if !from.is(Base::Timestamptz) {
            if to_ty.is(Base::Timestamptz) {
                ("overlite_tz_in(", ")")
            } else {
                ("", "")
            }
        } else if matches!(low.as_str(), "text" | "varchar" | "character varying") {
            self.push("overlite_tz_text(");
            self.expr(x, Prec::Or)?;
            self.push(")");
            return Ok(());
        } else if name == "overlite_cast" && wall {
            ("overlite_tz_wall(", ")")
        } else if to_ty.is(Base::Timestamptz) {
            ("", "")
        } else {
            ("overlite_tz_text(", ")")
        };
        self.push(name);
        self.push("(");
        self.push(pre);
        self.expr(x, Prec::Or)?;
        self.push(post);
        self.push(", ");
        self.expr(target, Prec::Or)?;
        self.push(")");
        Ok(())
    }

    /// A function of SQL syntax (`x AT TIME ZONE z`, `EXTRACT(f FROM x)`), with the
    /// environment, which knows which way its operand goes.
    fn sql_syntax(&mut self, name: &str, f: &pb::FuncCall) -> Out {
        let args: Vec<&NodeEnum> = f.args.iter().map(node).collect::<Result<_, _>>()?;
        match (name, args.as_slice()) {
            ("timezone", [zone, x]) => {
                let t = self.ty(x);
                // A timestamp, a wall clock, becomes the instant it is in the zone; a
                // timestamptz, an instant, the wall clock it shows there. What the
                // environment does not know is taken for an instant, as before.
                let func = match t.base {
                    _ if t.array => return Err("AT TIME ZONE of an array"),
                    Base::Timestamp => "overlite_from_time_zone(",
                    Base::Timestamptz => "overlite_instant_at_time_zone(",
                    Base::Unknown => {
                        self.note_param(x, Ty::of(Base::Timestamptz));
                        "overlite_instant_at_time_zone("
                    }
                    Base::Any => "overlite_column_at_time_zone(",
                    _ => return Err("AT TIME ZONE of a time"),
                };
                self.note_param(zone, Ty::TEXT);
                self.push(func);
                if t.base == Base::Unknown {
                    self.value(x, Ty::of(Base::Timestamptz), Prec::Or)?;
                } else {
                    self.expr(x, Prec::Or)?;
                }
                self.push(", ");
                self.expr(zone, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            ("extract", [field, x]) if is_string(field) => {
                // A numeric, as since Postgres 14; a timestamptz's fields are its wall
                // clock's in the session's zone.
                let t = self.ty(x);
                self.push("overlite_extract(");
                let NodeEnum::AConst(k) = field else {
                    return Err("EXTRACT field");
                };
                let Some(Val::Sval(s)) = &k.val else {
                    return Err("EXTRACT field");
                };
                self.push(&quote_literal(&s.sval.to_lowercase()));
                self.push(", ");
                if t.is(Base::Timestamptz) {
                    self.push("overlite_tz_wall(");
                    self.expr(x, Prec::Or)?;
                    self.push(")");
                } else {
                    self.expr(x, Prec::Or)?;
                }
                self.push(")");
                Ok(())
            }
            _ => Err("special function syntax"),
        }
    }

    fn sub_link(&mut self, s: &pb::SubLink) -> Out {
        let t = SubLinkType::try_from(s.sub_link_type).map_err(|_| "sublink")?;
        match t {
            SubLinkType::ExistsSublink => {
                self.push("EXISTS ");
                self.subquery(s)
            }
            SubLinkType::ExprSublink => self.subquery(s),
            SubLinkType::AnySublink if s.oper_name.is_empty() => self.in_subquery(s, false),
            _ => Err("sublink kind"),
        }
    }

    fn in_subquery(&mut self, s: &pb::SubLink, not: bool) -> Out {
        let test = boxed(s.testexpr.as_deref())?;
        if matches!(test, NodeEnum::RowExpr(_)) {
            return Err("row IN");
        }
        self.expr(test, Prec::Compare)?;
        self.push(if not { " NOT IN " } else { " IN " });
        self.subquery(s)
    }

    fn subquery(&mut self, s: &pb::SubLink) -> Out {
        let NodeEnum::SelectStmt(q) = boxed(s.subselect.as_deref())? else {
            return Err("subquery");
        };
        self.push("(");
        self.select(q, false)?;
        self.push(")");
        Ok(())
    }
}

/// A function's name, unqualified or in pg_catalog.
fn func_name(f: &pb::FuncCall) -> Option<&str> {
    match f.funcname.as_slice() {
        [n] => string(n).ok(),
        _ => None,
    }
}

/// An operator's name.
fn op_name(a: &pb::AExpr) -> Option<&str> {
    match a.name.as_slice() {
        [n] => string(n).ok(),
        _ => None,
    }
}

/// Whether a number literal has more significant digits than a double keeps: it is a
/// numeric's, written as its text.
fn is_long_number(lit: &str) -> bool {
    let lit = lit.trim_start_matches('-');
    let mantissa = lit.split(['e', 'E']).next().unwrap_or(lit);
    if mantissa.len() != lit.len() {
        return false;
    }
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    digits.trim_start_matches('0').trim_end_matches('0').len() > 15
        || (!mantissa.contains('.') && digits.trim_start_matches('0').len() > 18)
}

/// The precedence of an operator SQLite and Postgres both have, as SQLite binds it.
fn binary_prec(op: &str) -> Option<Prec> {
    Some(match op {
        "=" | "<>" | "!=" => Prec::Equality,
        "<" | ">" | "<=" | ">=" => Prec::Compare,
        "+" | "-" => Prec::Add,
        // `%` is not here: on text it is pg_trgm's similarity, which only the
        // operand's type tells from a remainder.
        "*" | "/" => Prec::Mul,
        "||" => Prec::Concat,
        _ => return None,
    })
}

/// The level a right operand needs: one tighter, as every operator here groups left.
fn next(p: Prec) -> Prec {
    match p {
        Prec::Or => Prec::And,
        Prec::And => Prec::Not,
        Prec::Not => Prec::Equality,
        Prec::Equality => Prec::Compare,
        Prec::Compare => Prec::Add,
        Prec::Add => Prec::Mul,
        Prec::Mul => Prec::Concat,
        Prec::Concat => Prec::Unary,
        Prec::Unary | Prec::Atom => Prec::Atom,
    }
}

/// How tightly n binds as the printer writes it.
fn prec(n: &NodeEnum) -> Prec {
    match n {
        NodeEnum::AExpr(a) => {
            use pb::AExprKind as K;
            match K::try_from(a.kind) {
                Ok(K::AexprOp) => match (&a.lexpr, a.name.as_slice()) {
                    (None, _) => Prec::Unary,
                    (Some(_), [op]) => string(op).ok().and_then(binary_prec).unwrap_or(Prec::Atom),
                    _ => Prec::Atom,
                },
                Ok(K::AexprNullif) => Prec::Atom,
                // ILIKE ends in its ESCAPE clause: an operand of anything is written in
                // parentheses, so that nothing after it is read as the escape.
                Ok(K::AexprIlike) => Prec::Not,
                _ => Prec::Equality,
            }
        }
        NodeEnum::BoolExpr(b) => match BoolExprType::try_from(b.boolop) {
            Ok(BoolExprType::AndExpr) => Prec::And,
            Ok(BoolExprType::OrExpr) => Prec::Or,
            _ => Prec::Not,
        },
        NodeEnum::NullTest(_) => Prec::Equality,
        NodeEnum::SubLink(s) if s.sub_link_type == SubLinkType::AnySublink as i32 => Prec::Equality,
        // A negative number is written with its sign.
        NodeEnum::AConst(k) => match &k.val {
            Some(Val::Ival(i)) if i.ival < 0 => Prec::Unary,
            Some(Val::Fval(f)) if f.fval.starts_with('-') => Prec::Unary,
            _ => Prec::Atom,
        },
        _ => Prec::Atom,
    }
}

/// The name of a cast's type as the engine's input functions read it, for the types
/// the slice takes.
fn cast_type(t: &pb::TypeName) -> Result<String, Unsupported> {
    if t.setof || t.pct_type || !t.array_bounds.is_empty() {
        return Err("array or SETOF type");
    }
    let names: Vec<&str> = t.names.iter().map(string).collect::<Result<_, _>>()?;
    let mods: Vec<i32> = t
        .typmods
        .iter()
        .map(|m| match node(m)? {
            NodeEnum::AConst(k) => match &k.val {
                Some(Val::Ival(i)) => Ok(i.ival),
                _ => Err("type modifier"),
            },
            _ => Err("type modifier"),
        })
        .collect::<Result<_, _>>()?;
    // The grammar writes a type named by keywords (`integer`, `timestamp with time
    // zone`) as pg_catalog's; one written as a plain name (`int4`, `timestamptz`) as
    // it came. Either way, the types the engine's input functions read.
    let b = match names.as_slice() {
        ["pg_catalog", b] | [b] => *b,
        _ => return Err("cast type"),
    };
    let base = match b {
        "int2" | "smallint" => "smallint",
        "int4" | "integer" | "int" => "integer",
        "int8" | "bigint" => "bigint",
        "float4" | "real" => "real",
        "float8" => "double precision",
        "numeric" | "decimal" => "numeric",
        "bool" | "boolean" => "boolean",
        "varchar" => "varchar",
        "bpchar" => "char",
        "text" | "date" | "uuid" | "json" | "jsonb" | "bytea" | "interval" | "name" | "hstore" | "inet" | "cidr"
        | "int4range" | "int8range" | "numrange" | "daterange" | "tsrange" | "tstzrange" => b,
        "time" | "timetz" | "timestamp" | "timestamptz" => b,
        _ => return Err("cast type"),
    };
    Ok(match (base, mods.as_slice()) {
        (_, []) => base.to_string(),
        ("numeric", [p]) => format!("numeric({p})"),
        ("numeric", [p, s]) => format!("numeric({p}, {s})"),
        ("varchar" | "char", [n]) => format!("{base}({n})"),
        ("time" | "timetz" | "timestamp" | "timestamptz", [p]) => format!("{base}({p})"),
        // An interval's modifier holds its fields (`interval '1' day`), which say
        // what an unmarked number counts.
        _ => return Err("type modifier"),
    })
}

/// A string literal, which a cast reads with the type's input function.
fn is_string(n: &NodeEnum) -> bool {
    matches!(n, NodeEnum::AConst(k) if matches!(k.val, Some(Val::Sval(_))))
}

/// The pattern and the escape of a LIKE's right side: `a LIKE p ESCAPE e` is
/// `a ~~ like_escape(p, e)` in the tree.
fn like_pattern(r: &NodeEnum) -> Result<(&NodeEnum, Option<&NodeEnum>), Unsupported> {
    if let NodeEnum::FuncCall(f) = r
        && let [schema, name] = f.funcname.as_slice()
        && string(schema)? == "pg_catalog"
        && string(name)? == "like_escape"
    {
        let [p, e] = f.args.as_slice() else {
            return Err("LIKE ESCAPE");
        };
        return Ok((node(p)?, Some(node(e)?)));
    }
    Ok((r, None))
}
