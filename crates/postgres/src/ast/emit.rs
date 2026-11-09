//! The printer: a Postgres parse tree written as SQLite SQL.
//!
//! It knows a slice of the grammar and answers `Err` for anything else, so that the
//! statement goes to the text passes instead. Nothing here guesses: a node it has not
//! been taught is unsupported, whatever its children.
//!
//! Parentheses come from SQLite's precedence, not from the source: the tree has none.
//! Postgres binds `||` looser than `+` and SQLite tighter, so `a + b || c` has to be
//! written `(a + b) || c` for SQLite to read what Postgres read.

use crate::ast::labels::{Label, figure_colname};
use overlite_core::text::{quote_ident, quote_literal};
use pg_query::NodeEnum;
use pg_query::protobuf::{
    self as pb, BoolExprType, CteMaterialize, JoinType, LimitOption, NullTestType, SetOperation, SortByDir,
    SortByNulls, SubLinkType, a_const::Val,
};

/// Why a statement is left to the text passes: the first node the printer does not know.
pub(crate) type Unsupported = &'static str;

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

/// Writes a statement of the slice, or says which node it stopped at.
pub(crate) fn statement(stmt: &NodeEnum) -> Result<String, Unsupported> {
    statement_labelled(stmt, None)
}

/// Writes a statement of the slice whose result columns take the given labels, one
/// per item of its result list (None: as the statement itself names it), when the
/// list has as many items. The session expands some calls before translating
/// (`current_user` into its value, `nextval('s')` into a number), and the labels are
/// those of the statement as the client sent it (`labels::result_labels`).
pub(crate) fn statement_labelled(stmt: &NodeEnum, labels: Option<&[Option<String>]>) -> Result<String, Unsupported> {
    let mut e = Emitter {
        out: String::with_capacity(128),
        labels,
    };
    match stmt {
        NodeEnum::SelectStmt(s) => e.select(s, true)?,
        NodeEnum::InsertStmt(s) => e.insert(s)?,
        NodeEnum::UpdateStmt(s) => e.update(s)?,
        NodeEnum::DeleteStmt(s) => e.delete(s)?,
        _ => return Err("statement kind"),
    }
    Ok(e.out)
}

struct Emitter<'a> {
    out: String,
    /// Labels for the result list in place of its own (`statement_labelled`).
    labels: Option<&'a [Option<String>]>,
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
        if let Some(w) = &s.with_clause {
            self.with(w)?;
        }
        let op = SetOperation::try_from(s.op).map_err(|_| "set operation")?;
        match op {
            SetOperation::SetopNone | SetOperation::Undefined => self.select_core(s, top)?,
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
            }
        }
        self.order_limit(s)
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
            self.expr(val, Prec::Or)?;
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
                self.expr(boxed(k.node.as_deref())?, Prec::Or)?;
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
                Some(c) => self.expr(c, Prec::Or)?,
                None => self.push("-1"),
            }
            if let Some(o) = offset {
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
        self.returning(s.returning_clause.as_ref())
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
        self.range_var(s.relation.as_ref().ok_or("UPDATE target")?, false)?;
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
            self.push(&ident(&t.name));
            self.push(" = ");
            self.expr(val, Prec::Or)?;
        }
        if let Some(w) = &s.where_clause {
            self.push(" WHERE ");
            self.expr(node(w)?, Prec::Or)?;
        }
        self.returning(s.returning_clause.as_ref())
    }

    fn delete(&mut self, s: &pb::DeleteStmt) -> Out {
        if s.with_clause.is_some() {
            return Err("WITH DELETE");
        }
        if !s.using_clause.is_empty() {
            return Err("DELETE USING");
        }
        self.push("DELETE FROM ");
        self.range_var(s.relation.as_ref().ok_or("DELETE target")?, false)?;
        if let Some(w) = &s.where_clause {
            self.push(" WHERE ");
            self.expr(node(w)?, Prec::Or)?;
        }
        self.returning(s.returning_clause.as_ref())
    }

    // ---- expressions ----

    /// Writes n where an expression binding at least as tightly as min may stand.
    fn expr(&mut self, n: &NodeEnum, min: Prec) -> Out {
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

    fn expr_bare(&mut self, n: &NodeEnum) -> Out {
        match n {
            NodeEnum::ColumnRef(c) => {
                if c.fields.len() > 1 && matches!(string(&c.fields[0]), Ok("pg_catalog" | "information_schema")) {
                    return Err("catalog column");
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
                self.list(&c.args, Self::expr_node)?;
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
                // The engine's GREATEST and LEAST skip NULLs, as Postgres' do.
                self.push(match pb::MinMaxOp::try_from(m.op) {
                    Ok(pb::MinMaxOp::IsGreatest) => "greatest(",
                    Ok(pb::MinMaxOp::IsLeast) => "least(",
                    _ => return Err("GREATEST or LEAST"),
                });
                self.list(&m.args, Self::expr_node)?;
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
            _ => Err("expression"),
        }
    }

    fn constant(&mut self, k: &pb::AConst) -> Out {
        if k.isnull {
            self.push("NULL");
            return Ok(());
        }
        match k.val.as_ref().ok_or("constant")? {
            Val::Ival(i) => self.push(&i.ival.to_string()),
            Val::Fval(f) => self.push(&f.fval),
            Val::Boolval(b) => self.push(if b.boolval { "true" } else { "false" }),
            Val::Sval(s) => self.push(&quote_literal(&s.sval)),
            Val::Bsval(_) => return Err("bit string"),
        }
        Ok(())
    }

    /// A cast: to text SQLite's own CAST, to boolean the engine's boolin, to any other
    /// type the engine knows the engine's input function.
    fn cast(&mut self, t: &pb::TypeCast) -> Out {
        let ty = t.type_name.as_ref().ok_or("cast type")?;
        let arg = boxed(t.arg.as_deref())?;
        let name = cast_type(ty)?;
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
                let p = binary_prec(op).ok_or("operator")?;
                self.expr(l, p)?;
                self.push(" ");
                self.push(op);
                self.push(" ");
                self.expr(r, next(p))
            }
            K::AexprDistinct | K::AexprNotDistinct => {
                self.expr(boxed(a.lexpr.as_deref())?, Prec::Compare)?;
                self.push(if kind == K::AexprDistinct {
                    " IS DISTINCT FROM "
                } else {
                    " IS NOT DISTINCT FROM "
                });
                self.expr(boxed(a.rexpr.as_deref())?, Prec::Compare)
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
                self.expr(boxed(a.lexpr.as_deref())?, Prec::Compare)?;
                self.push(if op == "<>" { " NOT IN (" } else { " IN (" });
                self.list(&items.items, Self::expr_node)?;
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
                self.expr(boxed(a.lexpr.as_deref())?, Prec::Compare)?;
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
                self.expr(boxed(a.lexpr.as_deref())?, Prec::Compare)?;
                self.push(if kind == K::AexprBetween {
                    " BETWEEN "
                } else {
                    " NOT BETWEEN "
                });
                self.expr(node(lo)?, Prec::Compare)?;
                self.push(" AND ");
                self.expr(node(hi)?, Prec::Compare)
            }
            _ => Err("operator kind"),
        }
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
