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

pub(crate) const TYPE_REFUSAL_STAR: Unsupported =
    "star of a relation the catalog does not know in a derived column list";

/// A statement Postgres refuses too, for a reason the printer finds (an operator no
/// type has, a derived column list longer than its relation): `refuse` keeps
/// Postgres' error on the thread for the translation's caller to send
/// (`take_refusal`), instead of a translation.
pub(crate) const POSTGRES_REFUSAL: Unsupported = "refused as Postgres refuses it";

/// The error Postgres gives: its SQLSTATE, its message, a hint, and the byte offset in
/// the statement it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub state: overlite_core::SqlState,
    pub message: String,
    pub hint: Option<&'static str>,
    pub location: Option<usize>,
}

thread_local! {
    static REFUSAL: std::cell::RefCell<Option<Refusal>> = const { std::cell::RefCell::new(None) };
}

/// Keeps r as the statement's answer, and says so to the printer's caller.
pub(crate) fn refuse(r: Refusal) -> Unsupported {
    REFUSAL.with(|c| *c.borrow_mut() = Some(r));
    POSTGRES_REFUSAL
}

/// The refusal `refuse` kept on this thread, once.
pub(crate) fn take_refusal() -> Option<Refusal> {
    REFUSAL.with(|c| c.borrow_mut().take())
}

/// A byte offset of the tree (-1 for none) as `Refusal::location`.
fn location(at: i32) -> Option<usize> {
    usize::try_from(at).ok()
}

/// Postgres' 42883 for an operator no type of its operands has.
fn no_operator(op: &str, l: Ty, r: Ty, at: i32) -> Unsupported {
    let (Some(l), Some(r)) = (l.pg_name(), r.pg_name()) else {
        return "operator of operands of unknown type";
    };
    refuse(Refusal {
        state: overlite_core::SqlState::UNDEFINED_FUNCTION,
        message: format!("operator does not exist: {l} {op} {r}"),
        hint: Some("No operator matches the given name and argument types. You might need to add explicit type casts."),
        location: location(at),
    })
}

type Out = Result<(), Unsupported>;

/// Reports whether a Postgres collation name denotes a case-insensitive
/// collation (by convention or ICU comparison level).
pub(crate) fn collation_is_ci(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("nocase")
        || n.contains("insensitive")
        || n.contains("ks-level1")
        || n.contains("ks-level2")
        || n.ends_with("_ci")
        || n.ends_with("-ci")
}

/// The value of a literal under a cast, as a literal: what a cast of a literal in a
/// table's DDL is stored as. None when it is not one or fails to convert, which leaves
/// it to SQLite.
fn fold_literal(operand: &str, name: &str) -> Option<String> {
    let body = operand.strip_prefix('\'')?.strip_suffix('\'')?;
    if body.replace("''", "").contains('\'') {
        return None;
    }
    if name.eq_ignore_ascii_case("boolean") || name.eq_ignore_ascii_case("bool") {
        // As the storage format keeps one.
        return overlite_core::parse_bool(body).map(|b| if b { "1" } else { "0" }.to_string());
    }
    let value = overlite_core::Value::Text(body.replace("''", "'"));
    match overlite_engine::cast(&value, &overlite_engine::CastType::parse(name)?).ok()? {
        overlite_core::Value::Text(s) => Some(quote_literal(&s)),
        overlite_core::Value::Int(n) => Some(n.to_string()),
        overlite_core::Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

/// The text a boolean column that may hold text holds for true and false besides 1
/// and 0: every spelling Postgres' boolin reads, as resolve/booleans.rs lists them.
const BOOL_TRUE_FORMS: &[&str] = &[
    "t", "true", "y", "yes", "on", "T", "TRUE", "Y", "YES", "ON", "True", "Yes", "On",
];
const BOOL_FALSE_FORMS: &[&str] = &[
    "f", "false", "n", "no", "off", "F", "FALSE", "N", "NO", "OFF", "False", "No", "Off",
];

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
    /// `&`, `|`, `<<`, `>>`: SQLite binds them between a comparison and `+`, where
    /// Postgres binds every operator of its own.
    Bit,
    Add,
    Mul,
    /// `||`.
    Concat,
    Unary,
    Atom,
}

/// The functions the text passes rewrote by name (each found, until stage 5c, by
/// running every word of their sources as a call): a call to one is written as they
/// wrote it, or refused, since written as it came it would mean something else in
/// SQLite. Any other function is written as it came, SQLite's, the engine's, or one of
/// the session's that it inlines.
pub(crate) const REWRITTEN: &[&str] = &[
    "array_to_string",
    "clock_timestamp",
    "col_description",
    "generate_series",
    "generate_subscripts",
    "json_agg",
    "json_array",
    "json_array_elements",
    "json_array_elements_text",
    "json_build_array",
    "json_build_object",
    "json_each",
    "json_each_text",
    "json_object",
    "json_object_agg",
    "json_object_keys",
    "json_typeof",
    "jsonb_agg",
    "jsonb_array_elements",
    "jsonb_array_elements_text",
    "jsonb_array_length",
    "jsonb_build_array",
    "jsonb_build_object",
    "jsonb_each",
    "jsonb_each_text",
    "jsonb_insert",
    "jsonb_object_agg",
    "jsonb_object_keys",
    "jsonb_pretty",
    "jsonb_set",
    "jsonb_typeof",
    "localtime",
    "localtimestamp",
    "now",
    "obj_description",
    "pg_get_constraintdef",
    "pg_get_indexdef",
    "pg_options_to_table",
    "pg_partition_ancestors",
    "pg_partition_root",
    "pg_partition_tree",
    "regexp_split_to_table",
    "shobj_description",
    "statement_timestamp",
    "string_agg",
    "to_json",
    "to_jsonb",
    "transaction_timestamp",
    "trim",
    "unnest",
];

/// The catalog's functions the text passes rewrite by name, which the printer writes
/// as they do (`Emitter::func_call`): a lookup of a comment or a definition, or an
/// empty relation where overlite has no partitions and no foreign data.
const CATALOG_CALLS: &[&str] = &[
    "obj_description",
    "shobj_description",
    "col_description",
    "pg_get_indexdef",
    "pg_get_constraintdef",
    "pg_partition_ancestors",
    "pg_partition_tree",
    "pg_partition_root",
    "pg_options_to_table",
    "json_typeof",
    "jsonb_typeof",
];

/// pg_options_to_table's rows: none, as rewrite_pg_options_to_table writes them.
const EMPTY_OPTIONS: &str = "(SELECT NULL AS option_name, NULL AS option_value WHERE 0)";

/// The json and jsonb functions SQLite has under other names, and whether the value is
/// jsonb (rewrite/json.rs, JSON_FUNC_MAP and JSONB_FUNCS).
const JSON_RENAMES: &[(&str, &str, bool)] = &[
    ("json_build_object", "json_object", false),
    ("jsonb_build_object", "json_object", true),
    ("json_build_array", "json_array", false),
    ("jsonb_build_array", "json_array", true),
    ("json_object_agg", "json_group_object", false),
    ("jsonb_object_agg", "json_group_object", true),
    ("json_agg", "json_group_array", false),
    ("jsonb_agg", "json_group_array", true),
    ("jsonb_array_length", "json_array_length", false),
    ("to_json", "json_quote", false),
    ("to_jsonb", "json_quote", true),
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

/// An expression of a DDL statement (a DEFAULT, a CHECK, a generated column's, an
/// index's key or WHERE), as SQLite reads it: untyped, as the engine's type passes
/// finish a DDL statement's text. In a table's DDL (table_ddl), a cast is SQLite's own
/// CAST and a cast of a literal the value it makes (`Emitter::table_ddl_cast`).
pub(crate) fn expression(n: &NodeEnum, table_ddl: bool) -> Result<String, Unsupported> {
    thread_local! {
        // No table is known to a DDL statement's expressions: what their operands are
        // is read as the text passes read what the catalog does not know.
        static EMPTY: Rc<Context> = Rc::new(Context::new(
            std::sync::Arc::new(overlite_engine::TableTypes::of(&[])),
            None,
        ));
    }
    let typer = EMPTY.with(|c| Typer::new(Rc::clone(c)));
    let mut e = Emitter::new(None, Some(typer), false);
    e.ddl = true;
    e.table_ddl = table_ddl;
    e.expr(n, Prec::Or)?;
    Ok(e.out)
}

fn print(stmt: &NodeEnum, labels: Option<&[Option<String>]>, typer: Option<Typer>) -> Result<Printed, Unsupported> {
    let item_casts = typer.is_some() && matches!(stmt, NodeEnum::SelectStmt(s) if s.with_clause.is_none());
    let mut e = Emitter::new(labels, typer, item_casts);
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
    /// Inside a chain of `->`, whose outermost carries the JSONB collation.
    in_arrow: bool,
    /// The outermost SELECT's list is still to come: an item whose type only Postgres
    /// knows (MIN, MAX, COALESCE, SUM of numerics, DATE_TRUNC, a scalar subquery...) is
    /// cast to it there, so that the protocol sends it as one.
    item_casts: bool,
    /// The relations of a FROM written as a table function whose columns SQLite names
    /// otherwise (`unnest(a) AS u(x)` is `overlite_elements(...) AS u`, whose `x` is
    /// `u.value`): each alias and its columns, as the statement names them and as
    /// SQLite does.
    renames: Vec<(String, Vec<(String, String)>)>,
    /// The relations of each FROM being written, innermost last: each name and whether
    /// it is a table, whose ctid is its rowid.
    froms: Vec<Vec<(String, bool)>>,
    /// The CTEs in scope, which are no tables.
    cte_names: Vec<String>,
    /// The select whose ORDER BY is being written has an output named ctid, which a
    /// bare ctid there names.
    ctid_output: bool,
    /// How many set-returning functions without an alias have been written (`_srf0`...).
    srfs: usize,
    /// The set-returning functions of the select list being written, each call (by
    /// address) and what reads its column of the set FROM gains (`select_core`).
    list_srfs: Vec<(usize, String)>,
    /// The aliases of each FROM's set-returning functions, in the order they are
    /// written, innermost FROM last.
    srf_aliases: Vec<std::collections::VecDeque<String>>,
    /// How many derived column lists have been written (`_alias0`...).
    aliases: usize,
    /// The alias of a written table and its name, which RETURNING reads it by: SQLite's
    /// RETURNING names the table alone.
    returning_table: Option<(String, String)>,
    /// The OLD and NEW of the RETURNING being written (Postgres 18).
    old_new: Option<OldNew>,
    /// Writing an expression of a table's DDL, which SQLite keeps in the file's
    /// schema: a cast there is SQLite's CAST (`table_ddl_cast`).
    table_ddl: bool,
    /// Writing an expression of a DDL statement, which SQLite runs without the
    /// session (a default, a check, an index's key): the clock is SQLite's own, read
    /// to the millisecond by strftime, which the catalog reads back as now() or
    /// CURRENT_TIMESTAMP.
    ddl: bool,
    /// A column being written as a whole item of a select list or RETURNING: a boolean
    /// holding text is left as it is there, its declared type reads it on the way out.
    whole_item: bool,
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

/// The information_schema views the engine makes, each a temp view whose name holds
/// the dot (`"information_schema.tables"`), as rewrite_information_schema names them.
const INFORMATION_SCHEMA: &[&str] = &[
    "tables",
    "columns",
    "table_constraints",
    "key_column_usage",
    "referential_constraints",
    "constraint_column_usage",
    "check_constraints",
    "views",
    "schemata",
    "sequences",
    "routines",
    "column_privileges",
];

/// A relation of the catalog as SQLite names it: pg_catalog's views under their bare
/// names, information_schema's under the dotted name of their temp view; None for a
/// relation of another schema, or one information_schema does not have here (written
/// as it came, as the text passes leave it).
fn catalog_relation(schema: &str, rel: &str) -> Option<String> {
    match schema {
        "pg_catalog" => Some(ident(rel)),
        "information_schema" if INFORMATION_SCHEMA.contains(&rel) => {
            Some(quote_ident(&["information_schema.", rel].concat()))
        }
        _ => None,
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

impl<'a> Emitter<'a> {
    fn new(labels: Option<&'a [Option<String>]>, typer: Option<Typer>, item_casts: bool) -> Self {
        Emitter {
            out: String::with_capacity(128),
            labels,
            item_casts,
            typer,
            params: Vec::new(),
            in_form: false,
            in_arrow: false,
            renames: Vec::new(),
            froms: Vec::new(),
            cte_names: Vec::new(),
            ctid_output: false,
            srfs: 0,
            list_srfs: Vec::new(),
            srf_aliases: Vec::new(),
            aliases: 0,
            returning_table: None,
            old_new: None,
            table_ddl: false,
            ddl: false,
            whole_item: false,
        }
    }

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
        // A row lock is the session's (`take_row_locks`, on the statement as sent):
        // SQLite's write lock holds every row, and the clause has no SQLite spelling.
        let (ctes, outputs) = match &mut self.typer {
            Some(t) => (t.push_with(s.with_clause.as_ref()), t.outputs.take()),
            None => (0, None),
        };
        let cte_names = self.cte_names.len();
        if let Some(w) = &s.with_clause {
            self.with(w)?;
        }
        let op = SetOperation::try_from(s.op).map_err(|_| "set operation")?;
        match op {
            SetOperation::SetopNone | SetOperation::Undefined => {
                self.push_from(&s.from_clause);
                let renamed = self.renames.len();
                self.enter_from(&s.from_clause)?;
                if s.distinct_clause.iter().any(|d| d.node.is_some()) {
                    self.distinct_on(s, top)?;
                } else {
                    self.select_core(s, top)?;
                    if !s.sort_clause.is_empty()
                        && let Some(t) = &mut self.typer
                    {
                        t.outputs = Some(t.targets(&s.target_list));
                    }
                    let was = std::mem::replace(&mut self.ctid_output, names_ctid(&s.target_list));
                    self.order_limit(s)?;
                    self.ctid_output = was;
                }
                self.leave_from();
                self.renames.truncate(renamed);
                self.pop_scope();
            }
            SetOperation::SetopUnion | SetOperation::SetopIntersect | SetOperation::SetopExcept => {
                let (Some(l), Some(r)) = (&s.larg, &s.rarg) else {
                    return Err("set operation arms");
                };
                // SQLite reads a compound left to right with one precedence; Postgres binds
                // INTERSECT tighter and keeps a parenthesized right arm whole. A set
                // operation on the right is a member of its own, read from a subquery.
                self.set_member(l, top, false)?;
                self.push(match op {
                    SetOperation::SetopUnion => " UNION ",
                    SetOperation::SetopIntersect => " INTERSECT ",
                    _ => " EXCEPT ",
                });
                if s.all {
                    self.push("ALL ");
                }
                self.set_member(r, false, true)?;
                // The ORDER BY of a set operation reads its output columns alone.
                if !s.sort_clause.is_empty()
                    && let Some(t) = &mut self.typer
                {
                    let cols = t.select_cols(l);
                    t.push_from(&[]);
                    t.outputs = Some(cols);
                }
                let mut first = l.as_ref();
                while let Some(l) = first.larg.as_deref() {
                    first = l;
                }
                let was = std::mem::replace(&mut self.ctid_output, names_ctid(&first.target_list));
                self.enter_from(&[])?;
                self.order_limit(s)?;
                self.leave_from();
                self.ctid_output = was;
                if !s.sort_clause.is_empty() {
                    self.pop_scope();
                }
            }
        }
        if let Some(t) = &mut self.typer {
            t.pop_with(ctes);
            t.outputs = outputs;
        }
        self.cte_names.truncate(cte_names);
        Ok(())
    }

    /// A member of a UNION: SQLite takes no parentheses there, so one with its own
    /// ORDER BY, LIMIT or WITH, or a set operation of its own on the right, is read
    /// from a subquery, as the text passes write it (rewrite_set_members).
    fn set_member(&mut self, s: &pb::SelectStmt, top: bool, right: bool) -> Out {
        let own =
            !s.sort_clause.is_empty() || s.limit_count.is_some() || s.limit_offset.is_some() || s.with_clause.is_some();
        if own || (right && s.op != SetOperation::SetopNone as i32) {
            self.push("SELECT * FROM (");
            self.select(s, top)?;
            self.push(")");
            return Ok(());
        }
        self.select(s, top)
    }

    fn with(&mut self, w: &pb::WithClause) -> Out {
        self.push("WITH ");
        if w.recursive {
            self.push("RECURSIVE ");
        }
        for c in &w.ctes {
            if let NodeEnum::CommonTableExpr(c) = node(c)? {
                self.cte_names.push(c.ctename.clone());
            }
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
        // Set-returning functions in the select list: the set moves into FROM, joined
        // after its relations (lateral, as the engine's walkers are), and each call
        // reads its column of it, as rewrite_set_returning writes it. Calls side by
        // side step together, the shorter padded with NULLs (overlite_zip).
        let mut calls = Vec::new();
        for t in &s.target_list {
            if let NodeEnum::ResTarget(t) = node(t)? {
                list_srf_calls(boxed(t.val.as_deref())?, &mut calls);
            }
        }
        let mut set = None;
        if !calls.is_empty() {
            let alias = format!("_srf{}", self.srfs);
            self.srfs += 1;
            let zipped = calls.len() > 1;
            for (i, f) in calls.iter().enumerate() {
                let srf = srf_of_call(f)?;
                let read = if srf.pairs || srf.args.len() > 1 && srf.name == "unnest" {
                    let what = [srf.name, " returning a record in a select list"].concat();
                    ["overlite_unsupported(", &quote_literal(&what), ")"].concat()
                } else if zipped {
                    format!("json_extract({}.value, '$[{i}]')", ident(&alias))
                } else {
                    [ident(&alias).as_str(), ".value"].concat()
                };
                self.list_srfs.push((std::ptr::from_ref(*f) as usize, read));
            }
            set = Some((alias, calls));
        }
        self.push("SELECT ");
        if !s.distinct_clause.is_empty() {
            self.push("DISTINCT ");
        }
        let listed = self.targets(&s.target_list, top);
        if let Some((_, calls)) = &set {
            self.list_srfs.truncate(self.list_srfs.len() - calls.len());
        }
        listed?;
        if !s.from_clause.is_empty() {
            self.push(" FROM ");
            self.list(&s.from_clause, Self::table_ref)?;
        }
        if let Some((alias, calls)) = set {
            self.push(if s.from_clause.is_empty() { " FROM " } else { ", " });
            let srfs: Vec<Srf> = calls.iter().map(|f| srf_of_call(f)).collect::<Result<_, _>>()?;
            let pairs = srfs.len() == 1 && srfs[0].pairs;
            self.push(if pairs { "overlite_pairs(" } else { "overlite_elements(" });
            if srfs.len() > 1 {
                self.push("overlite_zip(");
            }
            for (i, srf) in srfs.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.srf_source(srf)?;
            }
            if srfs.len() > 1 {
                self.push(")");
            }
            self.push(") AS ");
            self.push(&ident(&alias));
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
        if !s.window_clause.is_empty() {
            self.push(" WINDOW ");
            for (i, w) in s.window_clause.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                let NodeEnum::WindowDef(w) = node(w)? else {
                    return Err("window definition");
                };
                self.push(&ident(&w.name));
                self.push(" AS ");
                self.window_spec(w)?;
            }
        }
        Ok(())
    }

    /// SELECT DISTINCT ON, which SQLite has not: the first row of each group of the ON
    /// expressions in the statement's order, by ROW_NUMBER() over them, as the text
    /// passes write it (rewrite/distinct_on.rs). Each item and each ORDER BY key is
    /// computed beside the row number in a subquery, and the outer SELECT keeps the
    /// first rows under the items' names; a star over one table keeps them by rowid.
    fn distinct_on(&mut self, s: &pb::SelectStmt, top: bool) -> Out {
        if !s.values_lists.is_empty() || s.group_distinct {
            return Err("DISTINCT ON");
        }
        let items: Vec<&pb::ResTarget> = s
            .target_list
            .iter()
            .map(|t| match node(t)? {
                NodeEnum::ResTarget(t) => Ok(t.as_ref()),
                _ => Err("target"),
            })
            .collect::<Result<_, _>>()?;
        let star = items.iter().any(|t| {
            matches!(t.val.as_deref().and_then(|v| v.node.as_ref()), Some(NodeEnum::ColumnRef(c))
                if matches!(c.fields.last().and_then(|f| f.node.as_ref()), Some(NodeEnum::AStar(_))))
        });
        // A position or an output name, in ON as in ORDER BY, is the item it stands for.
        let resolve = |n: &'_ NodeEnum| -> Result<NodeEnum, Unsupported> {
            match n {
                NodeEnum::AConst(k) => match &k.val {
                    Some(Val::Ival(i)) => {
                        let at = usize::try_from(i.ival - 1).map_err(|_| "DISTINCT ON position")?;
                        let t = items.get(at).ok_or("DISTINCT ON position")?;
                        Ok(boxed(t.val.as_deref())?.clone())
                    }
                    _ => Ok(n.clone()),
                },
                NodeEnum::ColumnRef(c) if c.fields.len() == 1 => {
                    let name = string(&c.fields[0])?;
                    match items.iter().find(|t| t.name == name) {
                        Some(t) => Ok(boxed(t.val.as_deref())?.clone()),
                        None => Ok(n.clone()),
                    }
                }
                _ => Ok(n.clone()),
            }
        };
        let on: Vec<NodeEnum> = s
            .distinct_clause
            .iter()
            .map(|d| resolve(node(d)?))
            .collect::<Result<_, _>>()?;
        let mut keys: Vec<(NodeEnum, &pb::SortBy)> = Vec::new();
        for k in &s.sort_clause {
            let NodeEnum::SortBy(k) = node(k)? else {
                return Err("sort key");
            };
            keys.push((resolve(boxed(k.node.as_deref())?)?, k.as_ref()));
        }
        let casts = std::mem::take(&mut self.item_casts);
        let _ = casts;
        if top {
            // The labels are the outer SELECT's, written below.
            self.labels.take();
        }
        if star {
            let ([only], [NodeEnum::RangeVar(r)]) = (
                items.as_slice(),
                s.from_clause
                    .iter()
                    .map(node)
                    .collect::<Result<Vec<_>, _>>()?
                    .as_slice(),
            ) else {
                return Err("DISTINCT ON over a star");
            };
            if !s.group_clause.is_empty() || s.having_clause.is_some() {
                return Err("DISTINCT ON over a star");
            }
            let qual = ident(r.alias.as_ref().map_or(&r.relname, |a| &a.aliasname));
            self.push("SELECT ");
            self.expr(boxed(only.val.as_deref())?, Prec::Or)?;
            self.push(" FROM ");
            self.range_var(r, true)?;
            self.push(" WHERE ");
            self.push(&qual);
            self.push(".rowid IN (SELECT rid FROM (SELECT ");
            self.push(&qual);
            self.push(".rowid AS rid, ");
            self.row_number(&on, &keys)?;
            self.push(" FROM ");
            self.range_var(r, true)?;
            if let Some(w) = &s.where_clause {
                self.push(" WHERE ");
                self.expr(node(w)?, Prec::Or)?;
            }
            self.push(") WHERE _ov_rn = 1)");
            if !keys.is_empty() {
                self.push(" ORDER BY ");
                for (i, (key, k)) in keys.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.expr(key, Prec::Or)?;
                    self.sort_direction(k, key)?;
                }
            }
            return self.limit(s);
        }
        self.push("SELECT ");
        for (i, t) in items.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            // A jsonb item keeps its type (and its order) past the subquery.
            if self.typed() && self.ty(boxed(t.val.as_deref())?).is(Base::Jsonb) {
                self.push(&format!("(\"_ov_c{i}\") COLLATE JSONB AS "));
            } else {
                self.push(&format!("\"_ov_c{i}\" AS "));
            }
            let label = if t.name.is_empty() {
                match figure_colname(boxed(t.val.as_deref())?) {
                    Label::Named(l) => l,
                    Label::Itself => match boxed(t.val.as_deref())? {
                        NodeEnum::ColumnRef(c) => string(c.fields.last().ok_or("column")?)?.to_string(),
                        _ => "?column?".to_string(),
                    },
                }
            } else {
                t.name.clone()
            };
            self.push(&quote_ident(&label));
        }
        self.push(" FROM (SELECT ");
        for (i, t) in items.iter().enumerate() {
            self.expr(boxed(t.val.as_deref())?, Prec::Or)?;
            self.push(&format!(" AS \"_ov_c{i}\", "));
        }
        for (i, (key, _)) in keys.iter().enumerate() {
            self.expr(key, Prec::Or)?;
            self.push(&format!(" AS \"_ov_o{i}\", "));
        }
        // Without ORDER BY, the rows in the order of the ON keys, as Postgres' sort
        // for them leaves them.
        if keys.is_empty() {
            for (i, d) in on.iter().enumerate() {
                self.expr(d, Prec::Or)?;
                self.push(&format!(" AS \"_ov_d{i}\", "));
            }
        }
        self.row_number(&on, &keys)?;
        if !s.from_clause.is_empty() {
            self.push(" FROM ");
            self.list(&s.from_clause, Self::table_ref)?;
        }
        if let Some(w) = &s.where_clause {
            self.push(" WHERE ");
            self.expr(node(w)?, Prec::Or)?;
        }
        if !s.group_clause.is_empty() {
            self.push(" GROUP BY ");
            self.list(&s.group_clause, Self::expr_node)?;
        }
        if let Some(h) = &s.having_clause {
            self.push(" HAVING ");
            self.expr(node(h)?, Prec::Or)?;
        }
        self.push(") AS _ov WHERE _ov_rn = 1");
        if keys.is_empty() {
            self.push(" ORDER BY ");
            for i in 0..on.len() {
                if i > 0 {
                    self.push(", ");
                }
                self.push(&format!("\"_ov_d{i}\" NULLS LAST"));
            }
        } else {
            self.push(" ORDER BY ");
            for (i, (key, k)) in keys.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.push(&format!("\"_ov_o{i}\""));
                self.sort_direction(k, key)?;
            }
        }
        self.limit(s)
    }

    /// `ROW_NUMBER() OVER (PARTITION BY on ORDER BY keys) AS _ov_rn`.
    fn row_number(&mut self, on: &[NodeEnum], keys: &[(NodeEnum, &pb::SortBy)]) -> Out {
        self.push("ROW_NUMBER() OVER (PARTITION BY ");
        for (i, e) in on.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.expr(e, Prec::Or)?;
        }
        if !keys.is_empty() {
            self.push(" ORDER BY ");
            for (i, (key, k)) in keys.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.expr(key, Prec::Or)?;
                self.sort_direction(k, key)?;
            }
        }
        self.push(") AS _ov_rn");
        Ok(())
    }

    /// A window: a name, or `(` [base] PARTITION BY, ORDER BY and the frame `)`.
    fn window_spec(&mut self, w: &pb::WindowDef) -> Out {
        self.push("(");
        let mut sep = "";
        if !w.refname.is_empty() {
            self.push(&ident(&w.refname));
            sep = " ";
        }
        if !w.partition_clause.is_empty() {
            self.push(sep);
            self.push("PARTITION BY ");
            self.list(&w.partition_clause, Self::expr_node)?;
            sep = " ";
        }
        if !w.order_clause.is_empty() {
            self.push(sep);
            self.push("ORDER BY ");
            self.sort_keys(&w.order_clause)?;
            sep = " ";
        }
        self.frame(w, sep)?;
        self.push(")");
        Ok(())
    }

    /// A window's frame, from Postgres' FRAMEOPTION_ bits (nodes/parsenodes.h), which
    /// SQLite reads in the same words.
    fn frame(&mut self, w: &pb::WindowDef, sep: &str) -> Out {
        let o = w.frame_options;
        if o & 0x1 == 0 {
            return Ok(());
        }
        self.push(sep);
        self.push(if o & 0x2 != 0 {
            "RANGE "
        } else if o & 0x4 != 0 {
            "ROWS "
        } else if o & 0x8 != 0 {
            "GROUPS "
        } else {
            return Err("window frame");
        });
        let between = o & 0x10 != 0;
        if between {
            self.push("BETWEEN ");
        }
        // The start: UNBOUNDED PRECEDING, CURRENT ROW, n PRECEDING, n FOLLOWING.
        if o & 0x20 != 0 {
            self.push("UNBOUNDED PRECEDING");
        } else if o & 0x200 != 0 {
            self.push("CURRENT ROW");
        } else if o & 0x800 != 0 {
            self.expr(boxed(w.start_offset.as_deref())?, Prec::Or)?;
            self.push(" PRECEDING");
        } else if o & 0x2000 != 0 {
            self.expr(boxed(w.start_offset.as_deref())?, Prec::Or)?;
            self.push(" FOLLOWING");
        } else {
            return Err("window frame");
        }
        if between {
            self.push(" AND ");
            if o & 0x100 != 0 {
                self.push("UNBOUNDED FOLLOWING");
            } else if o & 0x400 != 0 {
                self.push("CURRENT ROW");
            } else if o & 0x1000 != 0 {
                self.expr(boxed(w.end_offset.as_deref())?, Prec::Or)?;
                self.push(" PRECEDING");
            } else if o & 0x4000 != 0 {
                self.expr(boxed(w.end_offset.as_deref())?, Prec::Or)?;
                self.push(" FOLLOWING");
            } else {
                return Err("window frame");
            }
        }
        if o & 0x8000 != 0 {
            self.push(" EXCLUDE CURRENT ROW");
        } else if o & 0x10000 != 0 {
            self.push(" EXCLUDE GROUP");
        } else if o & 0x20000 != 0 {
            self.push(" EXCLUDE TIES");
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
            if let Some((ty, name)) = casts.then(|| self.item_cast(val)).flatten() {
                self.cast_item(val, ty, name)?;
            } else {
                self.whole_item = matches!(val, NodeEnum::ColumnRef(_));
                let out = self.expr(val, Prec::Or);
                self.whole_item = false;
                out?;
            }
            if let NodeEnum::ColumnRef(c) = val
                && matches!(c.fields.last().and_then(|f| f.node.as_ref()), Some(NodeEnum::AStar(_)))
            {
                self.star_renames(&c.fields)?;
            }
            if !t.name.is_empty() {
                self.push(" AS ");
                self.push(&quote_ident(&t.name));
            } else if let Some(Some(l)) = given.map(|g| &g[i]) {
                self.push(" AS ");
                self.push(&quote_ident(l));
            } else if let NodeEnum::ColumnRef(c) = val
                && let Some(Ok(name)) = c.fields.last().map(string)
                && (name == "ctid" || self.renamed(&c.fields)?.is_some())
            {
                // A column SQLite names otherwise: ctid, a table function's column.
                self.push(" AS ");
                self.push(&quote_ident(name));
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

    /// After a star over a FROM with a set-returning function written in place, the
    /// function's columns under the names the statement gives them, which the star
    /// does not hold (it holds SQLite's: value, ord), as rewrite_lateral adds them: a
    /// query reading the star's columns by those names finds them.
    fn star_renames(&mut self, fields: &[pb::Node]) -> Out {
        let Some(aliases) = self.srf_aliases.last() else {
            return Ok(());
        };
        let qualifier = match fields {
            [_] => None,
            [q, _] => Some(string(q)?),
            _ => return Ok(()),
        };
        let mut extra = Vec::new();
        for alias in aliases {
            if qualifier.is_some_and(|q| q != alias) {
                continue;
            }
            if let Some((_, cols)) = self.renames.iter().rev().find(|(a, _)| a == alias) {
                for (name, col) in cols {
                    if name != alias && *col != [ident(alias).as_str(), ".", &ident(name)].concat() {
                        extra.push((name.clone(), col.clone()));
                    }
                }
            }
        }
        for (name, col) in extra {
            self.push(", ");
            self.push(&col);
            self.push(" AS ");
            self.push(&quote_ident(&name));
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
            NodeEnum::FuncCall(f) => {
                matches!(
                    func_name(f),
                    Some(
                        "min"
                            | "max"
                            | "any_value"
                            | "lag"
                            | "lead"
                            | "first_value"
                            | "last_value"
                            | "nth_value"
                            | "sum"
                            | "avg"
                            | "abs"
                            | "round"
                            | "ceil"
                            | "ceiling"
                            | "floor"
                            | "date_trunc"
                    )
                ) || (matches!(func_name(f), Some("lower" | "upper"))
                    && f.args.len() == 1
                    && node(&f.args[0]).is_ok_and(|x| self.is_range(x)))
            }
            _ => false,
        };
        if !passes {
            return None;
        }
        let ty = self.ty(val);
        // A jsonb a COALESCE, MIN or a subquery gives, typed as one.
        if ty.is(Base::Jsonb) && !matches!(val, NodeEnum::FuncCall(_)) {
            return Some((ty, "jsonb"));
        }
        if let NodeEnum::FuncCall(f) = val
            && matches!(func_name(f), Some("sum" | "avg"))
            && ty.is(Base::Float8)
        {
            // A double's sum and average, typed as one over no rows too, where nothing
            // is left to sample.
            return Some((ty, "float8"));
        }
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

    /// Sort keys, each with its NULLS written out: Postgres sorts NULL above every
    /// value, SQLite below.
    fn sort_keys(&mut self, keys: &[pb::Node]) -> Out {
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            let NodeEnum::SortBy(k) = node(k)? else {
                return Err("sort key");
            };
            let key = boxed(k.node.as_deref())?;
            self.expr(key, Prec::Or)?;
            self.sort_direction(k, key)?;
        }
        Ok(())
    }

    /// The direction of a sort key and where its NULLs go.
    fn sort_direction(&mut self, k: &pb::SortBy, key: &NodeEnum) -> Out {
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
        let first = match SortByNulls::try_from(k.sortby_nulls) {
            Ok(SortByNulls::SortbyNullsFirst) => true,
            Ok(SortByNulls::SortbyNullsLast) => false,
            _ => desc,
        };
        self.push(if first { " NULLS FIRST" } else { " NULLS LAST" });
        Ok(())
    }

    fn order_limit(&mut self, s: &pb::SelectStmt) -> Out {
        if !s.sort_clause.is_empty() {
            self.push(" ORDER BY ");
            self.sort_keys(&s.sort_clause)?;
            if let Some(t) = &mut self.typer {
                t.outputs = None;
            }
        }
        self.limit(s)
    }

    fn limit(&mut self, s: &pb::SelectStmt) -> Out {
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
                if j.join_using_alias.is_some() {
                    return Err("join alias");
                }
                if let Some(a) = &j.alias {
                    // A join named as a whole, which SQLite reads in parentheses.
                    if !a.colnames.is_empty() {
                        return Err("join alias");
                    }
                    let mut bare = j.as_ref().clone();
                    bare.alias = None;
                    self.push("(");
                    self.table_ref(&NodeEnum::JoinExpr(Box::new(bare)))?;
                    self.push(") AS ");
                    self.push(&ident(&a.aliasname));
                    return Ok(());
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
                match &r.alias {
                    // `(VALUES ...) AS v(a, b)`: SQLite names a subquery's columns only by
                    // a CTE's column list, as the text passes write it (rewrite_derived_aliases).
                    Some(a) if !a.colnames.is_empty() => {
                        let mut names: Vec<String> = a
                            .colnames
                            .iter()
                            .map(|c| string(c).map(str::to_string))
                            .collect::<Result<_, _>>()?;
                        let width = match output_names(q) {
                            Ok(w) => w,
                            // A star's columns, where the catalog knows its relations.
                            Err(why) => match &mut self.typer {
                                Some(t) => t.star_names(q).ok_or(TYPE_REFUSAL_STAR)?,
                                None => return Err(why),
                            },
                        };
                        if names.len() > width.len() {
                            return Err(refuse(Refusal {
                                state: overlite_core::SqlState::INVALID_COLUMN_REFERENCE,
                                message: format!(
                                    "table \"{}\" has {} columns available but {} columns specified",
                                    a.aliasname,
                                    width.len(),
                                    names.len()
                                ),
                                hint: None,
                                location: None,
                            }));
                        }
                        names.extend(width[names.len()..].iter().cloned());
                        let cte = format!("_alias{}", self.aliases);
                        self.aliases += 1;
                        self.push("WITH ");
                        self.push(&cte);
                        self.push("(");
                        for (i, n) in names.iter().enumerate() {
                            if i > 0 {
                                self.push(", ");
                            }
                            self.push(&ident(n));
                        }
                        self.push(") AS (");
                        self.select(q, false)?;
                        self.push(") SELECT * FROM ");
                        self.push(&cte);
                    }
                    _ => self.select(q, false)?,
                }
                self.push(")");
                if let Some(a) = &r.alias {
                    self.push(" AS ");
                    self.push(&ident(&a.aliasname));
                }
                Ok(())
            }
            NodeEnum::RangeFunction(f) => self.range_function(f),
            _ => Err("FROM item"),
        }
    }

    /// Opens a FROM: its relations' names, and the columns of its table functions under
    /// the names SQLite gives them (`srf_from`), before the select list reads them.
    fn enter_from(&mut self, from: &[pb::Node]) -> Out {
        let mut rels = Vec::new();
        self.srf_aliases.push(std::collections::VecDeque::new());
        self.prepare_from(from, &mut rels)?;
        self.froms.push(rels);
        Ok(())
    }

    fn leave_from(&mut self) {
        self.froms.pop();
        self.srf_aliases.pop();
    }

    fn prepare_from(&mut self, from: &[pb::Node], rels: &mut Vec<(String, bool)>) -> Out {
        for f in from {
            match node(f)? {
                NodeEnum::JoinExpr(j) => {
                    self.prepare_from(std::slice::from_ref(j.larg.as_deref().ok_or("join")?), rels)?;
                    self.prepare_from(std::slice::from_ref(j.rarg.as_deref().ok_or("join")?), rels)?;
                }
                NodeEnum::RangeVar(r) => {
                    let table = !(r.schemaname.is_empty() && self.cte_names.contains(&r.relname));
                    rels.push((r.alias.as_ref().map_or(&r.relname, |a| &a.aliasname).clone(), table));
                }
                NodeEnum::RangeSubselect(r) => {
                    rels.push((r.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default(), false));
                }
                NodeEnum::RangeFunction(r) => {
                    let Some(mut srf) = srf_from(r)? else {
                        rels.push((r.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default(), false));
                        continue;
                    };
                    if !srf.aliased && srf.lateral {
                        srf.alias = format!("_srf{}", self.srfs);
                    }
                    if !srf.aliased {
                        self.srfs += 1;
                    }
                    rels.push((srf.alias.clone(), false));
                    let mut cols: Vec<(String, String)> = if srf.lateral {
                        (0..srf.names.len())
                            .map(|i| (srf.names[i].clone(), srf.column_of(i, &srf.alias)))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    // A function of one column is named by its alias too (`unnest(a) AS u`
                    // reads `u`).
                    if srf.aliased
                        && !srf.pairs
                        && srf.columns.len() - usize::from(r.ordinality) == 1
                        && srf.names[0] != srf.alias
                    {
                        let col = if srf.lateral {
                            srf.column_of(0, &srf.alias)
                        } else {
                            [ident(&srf.alias), ".".to_string(), ident(&srf.names[0])].concat()
                        };
                        cols.push((srf.alias.clone(), col));
                    }
                    if !cols.is_empty() {
                        self.renames.push((srf.alias.clone(), cols));
                    }
                    if let Some(q) = self.srf_aliases.last_mut() {
                        q.push_back(srf.alias);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A function in FROM. A set-returning function is the engine's walk of the JSON
    /// array a scalar builds (builtins/srf.rs), as the text passes write it
    /// (rewrite/srf.rs, table_funcs.rs): in a subquery naming its columns where it
    /// reads no column, else in place, lateral, its columns read under SQLite's names;
    /// generate_series of integers a recursive CTE. Any other function is SQLite's or
    /// the engine's table, as it is.
    fn range_function(&mut self, r: &pb::RangeFunction) -> Out {
        let Some(mut srf) = srf_from(r)? else {
            let [item] = r.functions.as_slice() else {
                return Err("ROWS FROM");
            };
            let NodeEnum::List(l) = node(item)? else {
                return Err("FROM function");
            };
            let call = l.items.first().ok_or("FROM function")?;
            if let NodeEnum::FuncCall(f) = node(call)?
                && let Some(name) = func_name(f)
                && matches!(
                    name,
                    "pg_partition_ancestors" | "pg_partition_tree" | "pg_partition_root" | "pg_options_to_table"
                )
                && r.coldeflist.is_empty()
            {
                // No partitions and no foreign data: an empty relation with the
                // function's columns, as rewrite_partition_srf and
                // rewrite_pg_options_to_table write it, WITH ORDINALITY too (psql's `\d`
                // of a table with a trigger reads pg_partition_ancestors so, which the
                // text pass left to SQLite).
                let names: Vec<String> = match &r.alias {
                    Some(a) if !a.colnames.is_empty() => a
                        .colnames
                        .iter()
                        .map(|c| string(c).map(ident))
                        .collect::<Result<_, _>>()?,
                    _ if name == "pg_options_to_table" && !r.ordinality => Vec::new(),
                    _ if name == "pg_options_to_table" => {
                        vec!["option_name".into(), "option_value".into(), "ordinality".into()]
                    }
                    _ if r.ordinality => vec![name.to_string(), "ordinality".into()],
                    _ => vec![name.to_string()],
                };
                if names.is_empty() {
                    self.push(EMPTY_OPTIONS);
                } else {
                    self.push("(SELECT ");
                    for (i, n) in names.iter().enumerate() {
                        if i > 0 {
                            self.push(", ");
                        }
                        self.push("NULL AS ");
                        self.push(n);
                    }
                    self.push(" WHERE 0)");
                }
                if let Some(a) = &r.alias {
                    self.push(" AS ");
                    self.push(&ident(&a.aliasname));
                }
                return Ok(());
            }
            // pg_available_extensions(): the relation of that name, its columns named
            // by the alias's list (psql's \dx).
            if let NodeEnum::FuncCall(f) = node(call)?
                && func_name(f) == Some("pg_available_extensions")
                && f.args.is_empty()
                && !r.ordinality
                && r.coldeflist.is_empty()
            {
                let cols = ["name", "default_version", "comment"];
                let names: Vec<String> = match &r.alias {
                    Some(a) if !a.colnames.is_empty() => a
                        .colnames
                        .iter()
                        .map(|c| string(c).map(ident))
                        .collect::<Result<_, _>>()?,
                    _ => cols.iter().map(|c| (*c).to_string()).collect(),
                };
                if names.len() > cols.len() {
                    return Err("FROM function with more columns than it has");
                }
                self.push("(SELECT ");
                for (i, c) in cols.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.push(c);
                    self.push(" AS ");
                    self.push(names.get(i).map_or(*c, String::as_str));
                }
                self.push(" FROM pg_available_extensions)");
                if let Some(a) = &r.alias {
                    self.push(" AS ");
                    self.push(&ident(&a.aliasname));
                }
                return Ok(());
            }
            if r.ordinality || r.alias.as_ref().is_some_and(|a| !a.colnames.is_empty()) || !r.coldeflist.is_empty() {
                return Err("FROM function with columns");
            }
            self.expr(node(call)?, Prec::Or)?;
            if let Some(a) = &r.alias {
                self.push(" AS ");
                self.push(&ident(&a.aliasname));
            }
            return Ok(());
        };
        if let Some(alias) = self
            .srf_aliases
            .last_mut()
            .and_then(std::collections::VecDeque::pop_front)
        {
            srf.alias = alias;
        }
        if srf.name == "generate_series" {
            return self.generate_series(&srf);
        }
        let walker = |me: &mut Self| -> Out {
            me.push(if srf.pairs {
                "overlite_pairs("
            } else {
                "overlite_elements("
            });
            me.srf_source(&srf)?;
            me.push(")");
            Ok(())
        };
        if srf.lateral {
            walker(self)?;
            self.push(" AS ");
            self.push(&ident(&srf.alias));
            return Ok(());
        }
        self.push("(SELECT ");
        for (i, (name, col)) in srf.names.iter().zip(&srf.columns).enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.push(col);
            self.push(" AS ");
            self.push(&ident(name));
        }
        self.push(" FROM ");
        walker(self)?;
        self.push(") AS ");
        self.push(&ident(&srf.alias));
        Ok(())
    }

    /// The scalar that builds a set-returning function's rows (rewrite/srf.rs, set_of).
    fn srf_source(&mut self, srf: &Srf) -> Out {
        let args = srf.args.as_slice();
        let array_arg = |me: &mut Self, a: &NodeEnum| -> Out {
            me.push("overlite_array_json(");
            me.expr(a, Prec::Or)?;
            me.push(")");
            Ok(())
        };
        match srf.name {
            "unnest" if args.len() > 1 => {
                self.push("overlite_zip(");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    array_arg(self, a)?;
                }
                self.push(")");
                Ok(())
            }
            "unnest" => array_arg(self, args.first().ok_or("unnest")?),
            "generate_subscripts" => {
                self.push("overlite_subscripts(");
                self.expr(args.first().ok_or("generate_subscripts")?, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            "regexp_split_to_table" => {
                self.push("overlite_regexp_split(");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.expr(a, Prec::Or)?;
                }
                self.push(")");
                Ok(())
            }
            // In a select list; in FROM it is a recursive CTE (`generate_series`).
            "generate_series" => {
                self.push("overlite_series(");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.expr(a, Prec::Or)?;
                }
                self.push(")");
                Ok(())
            }
            "json_object_keys" | "jsonb_object_keys" => {
                self.push("overlite_json_keys(");
                self.expr(args.first().ok_or("object keys")?, Prec::Or)?;
                self.push(", ");
                self.push(&quote_literal(srf.name));
                self.push(")");
                Ok(())
            }
            name => {
                let text = u8::from(name.ends_with("_text"));
                self.push(if srf.pairs {
                    "overlite_json_pairs("
                } else {
                    "overlite_json_elements("
                });
                self.expr(args.first().ok_or("json set")?, Prec::Or)?;
                if srf.pairs {
                    let sorted = u8::from(name.starts_with("jsonb"));
                    self.push(&format!(", {text}, {sorted})"));
                } else {
                    self.push(&format!(", {text})"));
                }
                Ok(())
            }
        }
    }

    /// generate_series of integers, as rewrite_generate_series writes it: a recursive
    /// CTE stepping from the start while the step's sign says the end is not passed.
    fn generate_series(&mut self, srf: &Srf) -> Out {
        let (a, b, step) = match srf.args.as_slice() {
            [a, b] => (a, b, None),
            [a, b, s] => (a, b, Some(s)),
            _ => return Err("generate_series"),
        };
        for x in [Some(a), Some(b), step].into_iter().flatten() {
            let t = self.ty(x);
            if !(t.is_integer() || t.base == Base::Any && matches!(x, NodeEnum::AExpr(_))) {
                return Err("generate_series of what is no integer");
            }
        }
        let one = NodeEnum::AConst(pb::AConst {
            isnull: false,
            val: Some(Val::Ival(pb::Integer { ival: 1 })),
            location: 0,
        });
        let step: &NodeEnum = step.copied().unwrap_or(&one);
        let ord = srf.names.len() > 1;
        let paren = |me: &mut Self, n: &NodeEnum| -> Out {
            me.push("(");
            me.expr(n, Prec::Or)?;
            me.push(")");
            Ok(())
        };
        // (((s) > 0 AND x <= (b)) OR ((s) < 0 AND x >= (b)))
        let within = |me: &mut Self, x: &dyn Fn(&mut Self) -> Out| -> Out {
            me.push("(((");
            me.expr(step, Prec::Or)?;
            me.push(") > 0 AND ");
            x(me)?;
            me.push(" <= ");
            paren(me, b)?;
            me.push(") OR ((");
            me.expr(step, Prec::Or)?;
            me.push(") < 0 AND ");
            x(me)?;
            me.push(" >= ");
            paren(me, b)?;
            me.push("))");
            Ok(())
        };
        self.push(if ord {
            "(WITH RECURSIVE _gs(value, ord) AS (SELECT "
        } else {
            "(WITH RECURSIVE _gs(value) AS (SELECT "
        });
        paren(self, a)?;
        if ord {
            self.push(", 1");
        }
        self.push(" WHERE ");
        within(self, &|me: &mut Self| paren(me, a))?;
        self.push(" UNION ALL SELECT value + ");
        paren(self, step)?;
        if ord {
            self.push(", ord + 1");
        }
        self.push(" FROM _gs WHERE ");
        within(self, &|me: &mut Self| {
            me.push("value + ");
            paren(me, step)
        })?;
        self.push(") SELECT value AS ");
        self.push(&ident(&srf.names[0]));
        if ord {
            self.push(", ord AS ");
            self.push(&ident(&srf.names[1]));
        }
        self.push(" FROM _gs)");
        if srf.aliased {
            self.push(" AS ");
            self.push(&ident(&srf.alias));
        }
        Ok(())
    }

    /// A table: its schema as written, except the two the text passes resolve.
    fn range_var(&mut self, r: &pb::RangeVar, alias: bool) -> Out {
        if !r.inh {
            return Err("ONLY");
        }
        if !r.catalogname.is_empty() {
            return Err("catalog-qualified name");
        }
        if let Some(name) = catalog_relation(&r.schemaname, &r.relname) {
            self.push(&name);
        } else {
            if !r.schemaname.is_empty() {
                self.push(&ident(&r.schemaname));
                self.push(".");
            }
            // A quoted name holding a dot is written whole, as the text passes leave
            // it: the stored name of a table of another schema reads the same.
            self.push(&ident(&r.relname));
        }
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

    /// The WITH of a write: SQLite takes one before INSERT, UPDATE and DELETE.
    fn write_with(&mut self, w: Option<&pb::WithClause>) -> Result<usize, Unsupported> {
        let n = match &mut self.typer {
            Some(t) => t.push_with(w),
            None => 0,
        };
        if let Some(w) = w {
            self.with(w)?;
        }
        Ok(n)
    }

    fn pop_with(&mut self, n: usize) {
        if let Some(t) = &mut self.typer {
            t.pop_with(n);
        }
    }

    fn insert(&mut self, s: &pb::InsertStmt) -> Out {
        // OVERRIDING SYSTEM VALUE lets a value into a GENERATED ALWAYS column, which
        // the session checks and takes out before translating (identity_insert): the
        // value given is what SQLite stores. USER VALUE, which the session refuses,
        // would ask for the default instead.
        if s.r#override == pb::OverridingKind::OverridingUserValue as i32 {
            return Err("OVERRIDING USER VALUE");
        }
        let ctes = self.write_with(s.with_clause.as_ref())?;
        self.push("INSERT INTO ");
        let r = s.relation.as_ref().ok_or("INSERT target")?;
        self.range_var(r, true)?;
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
                // SQLite reads `ON` after a FROM as a join's: a SELECT before ON CONFLICT
                // takes a WHERE, as SQLite's documentation asks.
                if s.on_conflict_clause.is_some()
                    && q.values_lists.is_empty()
                    && q.op == SetOperation::SetopNone as i32
                    && !q.from_clause.is_empty()
                    && q.where_clause.is_none()
                    && q.group_clause.is_empty()
                    && q.sort_clause.is_empty()
                    && q.limit_count.is_none()
                    && q.limit_offset.is_none()
                    && q.window_clause.is_empty()
                {
                    self.push(" WHERE true");
                }
            }
        }
        self.push_target(r);
        self.enter_from(&[pb::Node {
            node: Some(NodeEnum::RangeVar(r.clone())),
        }])?;
        if let Some(c) = &s.on_conflict_clause {
            self.on_conflict(c)?;
        }
        let old_new = self.old_new_of(s.returning_clause.as_ref(), r, WriteKind::Insert)?;
        if old_new.is_some()
            && s.on_conflict_clause
                .as_ref()
                .is_some_and(|c| c.action == pb::OnConflictAction::OnconflictUpdate as i32)
        {
            // The row a conflict finds is not kept apart from the one written.
            return Err("RETURNING OLD/NEW of ON CONFLICT DO UPDATE");
        }
        self.old_new = old_new;
        let out = self.returning_with_alias(r, s.returning_clause.as_ref());
        self.old_new = None;
        out?;
        self.leave_from();
        self.pop_scope();
        self.pop_with(ctes);
        Ok(())
    }

    /// ON CONFLICT, which SQLite spells as Postgres does, but for a constraint's name.
    fn on_conflict(&mut self, c: &pb::OnConflictClause) -> Out {
        use pb::OnConflictAction as A;
        self.push(" ON CONFLICT");
        if let Some(infer) = &c.infer {
            if !infer.conname.is_empty() {
                return Err("ON CONFLICT ON CONSTRAINT");
            }
            if !infer.index_elems.is_empty() {
                self.push(" (");
                for (i, e) in infer.index_elems.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    let NodeEnum::IndexElem(e) = node(e)? else {
                        return Err("conflict target");
                    };
                    if !e.opclass.is_empty() || !e.collation.is_empty() || e.ordering != 1 || e.nulls_ordering != 1 {
                        return Err("conflict target");
                    }
                    match &e.expr {
                        Some(x) => {
                            self.push("(");
                            self.expr(node(x)?, Prec::Or)?;
                            self.push(")");
                        }
                        None => self.push(&ident(&e.name)),
                    }
                }
                self.push(")");
            }
            if let Some(w) = &infer.where_clause {
                self.push(" WHERE ");
                self.expr(node(w)?, Prec::Or)?;
            }
        }
        match A::try_from(c.action) {
            Ok(A::OnconflictNothing) => self.push(" DO NOTHING"),
            Ok(A::OnconflictUpdate) => {
                self.push(" DO UPDATE SET ");
                self.set_list(&c.target_list)?;
                if let Some(w) = &c.where_clause {
                    self.push(" WHERE ");
                    self.expr(node(w)?, Prec::Or)?;
                }
            }
            _ => return Err("ON CONFLICT action"),
        }
        Ok(())
    }

    fn returning(&mut self, r: Option<&pb::ReturningClause>) -> Out {
        let Some(r) = r else { return Ok(()) };
        if r.exprs.is_empty() {
            return Ok(());
        }
        self.push(" RETURNING ");
        let Some(on) = self.old_new.clone() else {
            return self.targets(&r.exprs, true);
        };
        // `old.*` and `new.*`: each column of the table, through the alias.
        let mut items = Vec::with_capacity(r.exprs.len());
        for e in &r.exprs {
            let star = match node(e)? {
                NodeEnum::ResTarget(t) => match boxed(t.val.as_deref())? {
                    NodeEnum::ColumnRef(c)
                        if c.fields.len() == 2 && matches!(node(&c.fields[1])?, NodeEnum::AStar(_)) =>
                    {
                        string(&c.fields[0]).ok().filter(|q| *q == on.old || *q == on.new)
                    }
                    _ => None,
                },
                _ => None,
            };
            let Some(qual) = star else {
                // An unnamed `old.v` is labelled v, as Postgres labels it, where SQLite
                // would label what it is written as.
                let mut e = e.clone();
                if let Some(NodeEnum::ResTarget(t)) = e.node.as_mut()
                    && t.name.is_empty()
                    && let Some(NodeEnum::ColumnRef(c)) = t.val.as_deref().and_then(|v| v.node.as_ref())
                    && let [q, col] = c.fields.as_slice()
                    && string(q).is_ok_and(|q| q == on.old || q == on.new)
                    && let Ok(col) = string(col)
                {
                    t.name = col.to_string();
                }
                items.push(e);
                continue;
            };
            for col in &on.columns {
                let field = |s: &str| pb::Node {
                    node: Some(NodeEnum::String(pb::String { sval: s.to_string() })),
                };
                items.push(pb::Node {
                    node: Some(NodeEnum::ResTarget(Box::new(pb::ResTarget {
                        name: col.clone(),
                        val: Some(Box::new(pb::Node {
                            node: Some(NodeEnum::ColumnRef(pb::ColumnRef {
                                fields: vec![field(qual), field(col)],
                                location: -1,
                            })),
                        })),
                        location: -1,
                        ..Default::default()
                    }))),
                });
            }
        }
        self.targets(&items, true)
    }

    /// The OLD and NEW a RETURNING names (by its WITH, or as `old` and `new`) where it
    /// reads either, for a write of kind to the table r; None where it reads neither.
    fn old_new_of(
        &self,
        ret: Option<&pb::ReturningClause>,
        r: &pb::RangeVar,
        kind: WriteKind,
    ) -> Result<Option<OldNew>, Unsupported> {
        let Some(ret) = ret else { return Ok(None) };
        let (mut old, mut new) = ("old".to_string(), "new".to_string());
        for o in &ret.options {
            let NodeEnum::ReturningOption(o) = node(o)? else {
                return Err("RETURNING option");
            };
            match pb::ReturningOptionKind::try_from(o.option) {
                Ok(pb::ReturningOptionKind::ReturningOptionOld) => old.clone_from(&o.value),
                Ok(pb::ReturningOptionKind::ReturningOptionNew) => new.clone_from(&o.value),
                _ => return Err("RETURNING option"),
            }
        }
        // A table named or aliased so is read by that name, as Postgres reads it.
        let named = r.alias.as_ref().map_or(r.relname.as_str(), |a| a.aliasname.as_str());
        if ret.options.is_empty() && (named == old || named == new) {
            return Ok(None);
        }
        let mut reads = false;
        for e in &ret.exprs {
            let Some(n) = e.node.as_ref() else { continue };
            for (n, ..) in n.nodes() {
                if let pg_query::NodeRef::ColumnRef(c) = n
                    && let Some(q) = c.fields.first().and_then(|f| string(f).ok())
                    && (q == old || q == new)
                {
                    if c.fields.len() != 2 {
                        return Err("RETURNING OLD or NEW as a whole row");
                    }
                    reads = true;
                }
            }
        }
        if !reads {
            return Ok(None);
        }
        if !r.schemaname.is_empty() && r.schemaname != "public" {
            return Err("RETURNING OLD/NEW of a table of another schema");
        }
        let columns = self.typer.as_ref().and_then(|t| t.table_columns(r)).unwrap_or_default();
        Ok(Some(OldNew {
            old,
            new,
            kind,
            table: r.relname.clone(),
            columns,
        }))
    }

    /// A column of OLD or NEW, as SQLite reads it in a RETURNING; None for any other
    /// column.
    fn old_new_column(&mut self, c: &pb::ColumnRef) -> Result<Option<()>, Unsupported> {
        let Some(on) = &self.old_new else { return Ok(None) };
        let [q, col] = c.fields.as_slice() else { return Ok(None) };
        let Ok(q) = string(q) else { return Ok(None) };
        let is_old = q == on.old;
        if !is_old && q != on.new {
            return Ok(None);
        }
        let NodeEnum::String(col) = node(col)? else {
            return Err("OLD or NEW star in an expression");
        };
        let col = ident(&col.sval);
        let table = ident(&on.table);
        let text = match (is_old, on.kind) {
            (true, WriteKind::Insert) | (false, WriteKind::Delete) => "NULL".to_string(),
            (true, WriteKind::Update) => format!(
                "(SELECT \"_overlite_old\".{col} FROM \"_overlite_old\" WHERE \"_overlite_old\".\"_r\" = {table}.rowid)"
            ),
            _ => format!("{table}.{col}"),
        };
        self.push(&text);
        Ok(Some(()))
    }

    /// The `a = x, b = y` of an UPDATE or an ON CONFLICT DO UPDATE, each value's
    /// parameter typed by its column.
    fn set_list(&mut self, items: &[pb::Node]) -> Out {
        let mut i = 0;
        while i < items.len() {
            if i > 0 {
                self.push(", ");
            }
            let NodeEnum::ResTarget(t) = node(&items[i])? else {
                return Err("SET item");
            };
            if !t.indirection.is_empty() {
                return Err("SET indirection");
            }
            let val = boxed(t.val.as_deref())?;
            if let NodeEnum::MultiAssignRef(m) = val {
                // `SET (a, b) = (x, y)` or `= (SELECT ...)`: SQLite's row value, each
                // item's parameter typed by its column.
                let n = usize::try_from(m.ncolumns).map_err(|_| "SET (a, b) =")?;
                let group = items.get(i..i + n).ok_or("SET (a, b) =")?;
                let mut names = Vec::with_capacity(n);
                for g in group {
                    let NodeEnum::ResTarget(g) = node(g)? else {
                        return Err("SET item");
                    };
                    if !g.indirection.is_empty() {
                        return Err("SET indirection");
                    }
                    names.push(g.name.clone());
                }
                let source = boxed(m.source.as_deref())?;
                self.push("(");
                for (k, name) in names.iter().enumerate() {
                    if k > 0 {
                        self.push(", ");
                    }
                    self.push(&ident(name));
                }
                self.push(") = ");
                match source {
                    NodeEnum::RowExpr(row) if row.args.len() == n => {
                        self.push("(");
                        for (k, (x, name)) in row.args.iter().zip(&names).enumerate() {
                            if k > 0 {
                                self.push(", ");
                            }
                            let x = node(x)?;
                            let ty = self.set_column_type(name);
                            self.note_param(x, ty);
                            self.expr(x, Prec::Or)?;
                        }
                        self.push(")");
                    }
                    NodeEnum::SubLink(l) if l.sub_link_type == SubLinkType::MultiexprSublink as i32 => {
                        self.subquery(l)?;
                    }
                    _ => return Err("SET (a, b) ="),
                }
                i += n;
                continue;
            }
            let ty = self.set_column_type(&t.name);
            self.note_param(val, ty);
            self.push(&ident(&t.name));
            self.push(" = ");
            self.expr(val, Prec::Or)?;
            i += 1;
        }
        Ok(())
    }

    /// The type of a column an UPDATE sets, as the environment knows it.
    fn set_column_type(&mut self, name: &str) -> Ty {
        let Some(typer) = &mut self.typer else {
            return Ty::ANY;
        };
        let col = pb::Node {
            node: Some(NodeEnum::String(pb::String { sval: name.to_string() })),
        };
        typer.column(std::slice::from_ref(&col)).map_or(Ty::ANY, |c| c.ty)
    }

    /// Whether a RETURNING reads only the written table: SQLite's sees no other, where
    /// Postgres' sees those of FROM and USING too.
    fn update(&mut self, s: &pb::UpdateStmt) -> Out {
        let r = s.relation.as_ref().ok_or("UPDATE target")?;
        let ret = joined_returning(
            s.returning_clause.as_ref(),
            r,
            &s.from_clause,
            s.where_clause.as_deref(),
        );
        let ctes = self.write_with(s.with_clause.as_ref())?;
        let old_new = self.old_new_of(ret.as_ref(), r, WriteKind::Update)?;
        if old_new.is_some() {
            if s.with_clause.is_some() {
                return Err("RETURNING OLD beside a WITH");
            }
            // The rows as they were, made before the first is written: the WHERE below
            // reads them, and RETURNING finds each by its rowid.
            self.push("WITH \"_overlite_old\" AS MATERIALIZED (SELECT rowid AS \"_r\", * FROM ");
            self.range_var(r, true)?;
            if s.from_clause.is_empty()
                && let Some(w) = &s.where_clause
            {
                self.push_target(r);
                self.enter_from(&[pb::Node {
                    node: Some(NodeEnum::RangeVar(r.clone())),
                }])?;
                self.push(" WHERE ");
                let out = self.expr(node(w)?, Prec::Or);
                self.leave_from();
                self.pop_scope();
                out?;
            }
            self.push(") ");
        }
        self.push("UPDATE ");
        self.range_var(r, true)?;
        if s.from_clause.is_empty() {
            self.push_target(r);
        } else {
            // The FROM's tables are in scope beside the written one.
            let mut from = vec![pb::Node {
                node: Some(NodeEnum::RangeVar(r.clone())),
            }];
            from.extend(s.from_clause.iter().cloned());
            self.push_from(&from);
        }
        let renamed = self.renames.len();
        let mut rels = vec![pb::Node {
            node: Some(NodeEnum::RangeVar(r.clone())),
        }];
        rels.extend(s.from_clause.iter().cloned());
        self.enter_from(&rels)?;
        self.push(" SET ");
        self.set_list(&s.target_list)?;
        if !s.from_clause.is_empty() {
            self.push(" FROM ");
            self.list(&s.from_clause, Self::table_ref)?;
        }
        if let Some(w) = &s.where_clause {
            self.push(" WHERE ");
            if old_new.is_some() {
                self.push("(");
            }
            self.expr(node(w)?, Prec::Or)?;
            if old_new.is_some() {
                self.push(") AND ");
            }
        } else if old_new.is_some() {
            self.push(" WHERE ");
        }
        if old_new.is_some() {
            let qual = r.alias.as_ref().map_or(&r.relname, |a| &a.aliasname);
            self.push(&ident(qual));
            self.push(".rowid IN (SELECT \"_r\" FROM \"_overlite_old\")");
        }
        self.old_new = old_new;
        let out = self.returning_with_alias(r, ret.as_ref());
        self.old_new = None;
        out?;
        self.renames.truncate(renamed);
        self.leave_from();
        self.pop_scope();
        self.pop_with(ctes);
        Ok(())
    }

    /// A RETURNING of a written table its alias names, which SQLite's reads by its
    /// name.
    fn returning_with_alias(&mut self, r: &pb::RangeVar, ret: Option<&pb::ReturningClause>) -> Out {
        self.returning_table = r.alias.as_ref().map(|a| (a.aliasname.clone(), r.relname.clone()));
        let out = self.returning(ret);
        self.returning_table = None;
        out
    }

    fn delete(&mut self, s: &pb::DeleteStmt) -> Out {
        let r = s.relation.as_ref().ok_or("DELETE target")?;
        let ctes = self.write_with(s.with_clause.as_ref())?;
        self.push("DELETE FROM ");
        let renamed = self.renames.len();
        if s.using_clause.is_empty() {
            self.range_var(r, true)?;
            self.push_target(r);
            self.enter_from(&[pb::Node {
                node: Some(NodeEnum::RangeVar(r.clone())),
            }])?;
            if let Some(w) = &s.where_clause {
                self.push(" WHERE ");
                self.expr(node(w)?, Prec::Or)?;
            }
        } else {
            // DELETE ... USING, which SQLite has not: the rows of the join, by rowid,
            // as rewrite_delete_using writes it.
            let mut bare = r.clone();
            bare.alias = None;
            self.range_var(&bare, false)?;
            let mut from = vec![pb::Node {
                node: Some(NodeEnum::RangeVar(r.clone())),
            }];
            from.extend(s.using_clause.iter().cloned());
            self.push_from(&from);
            self.enter_from(&from)?;
            let qual = r.alias.as_ref().map_or(&r.relname, |a| &a.aliasname);
            self.push(" WHERE rowid IN (SELECT ");
            self.push(&ident(qual));
            self.push(".rowid FROM ");
            self.list(&from, Self::table_ref)?;
            if let Some(w) = &s.where_clause {
                self.push(" WHERE ");
                self.expr(node(w)?, Prec::Or)?;
            }
            self.push(")");
        }
        let ret = joined_returning(
            s.returning_clause.as_ref(),
            r,
            &s.using_clause,
            s.where_clause.as_deref(),
        );
        self.old_new = self.old_new_of(ret.as_ref(), r, WriteKind::Delete)?;
        let out = self.returning_with_alias(r, ret.as_ref());
        self.old_new = None;
        out?;
        self.renames.truncate(renamed);
        self.leave_from();
        self.pop_scope();
        self.pop_with(ctes);
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
            NodeEnum::ColumnRef(c) if self.old_new_column(c)?.is_some() => Ok(()),
            NodeEnum::ColumnRef(c) => {
                // A column of a catalog relation named with its schema: the relation as
                // `catalog_relation` writes it, then the column.
                if c.fields.len() > 2
                    && let Ok(schema @ ("pg_catalog" | "information_schema")) = string(&c.fields[0])
                    && let Ok(rel) = string(&c.fields[1])
                    && let Some(name) = catalog_relation(schema, rel)
                {
                    self.push(&name);
                    for f in &c.fields[2..] {
                        self.push(".");
                        match node(f)? {
                            NodeEnum::String(s) => self.push(&ident(&s.sval)),
                            NodeEnum::AStar(_) => self.push("*"),
                            _ => return Err("column reference"),
                        }
                    }
                    return Ok(());
                }
                if c.fields.len() == 2 && matches!(string(&c.fields[0]), Ok("pg_catalog")) {
                    // `pg_catalog.x` names a column x of a relation pg_catalog, which no
                    // table is: the text passes read it as a bare x.
                    let NodeEnum::String(s) = node(&c.fields[1])? else {
                        return Err("column reference");
                    };
                    self.push(&ident(&s.sval));
                    return Ok(());
                }
                if let Some(t) = &mut self.typer
                    && t.any_mixed()
                    && !matches!(c.fields.last().and_then(|f| f.node.as_ref()), Some(NodeEnum::AStar(_)))
                    && t.column(&c.fields).is_some_and(|c| c.mixed)
                    && !std::mem::take(&mut self.whole_item)
                {
                    // A boolean that may hold text, read as Postgres' boolin reads each
                    // spelling (resolve/booleans.rs): 1, 0 or NULL.
                    self.push("overlite_bool(");
                    self.whole_item = true;
                    let out = self.expr_bare(n);
                    self.whole_item = false;
                    out?;
                    self.push(")");
                    return Ok(());
                }
                if let Some(written) = self.renamed(&c.fields)? {
                    self.push(&written);
                    return Ok(());
                }
                if let Some(ctid) = self.ctid(&c.fields)? {
                    self.push(&ctid);
                    return Ok(());
                }
                let last = c.fields.len() - 1;
                for (i, f) in c.fields.iter().enumerate() {
                    if i > 0 {
                        self.push(".");
                    }
                    match node(f)? {
                        NodeEnum::String(s) => {
                            // A system column (oid, tableoid, xmin) is a column of the
                            // catalog's views, which have them, or of a table with one
                            // by that name: written as it came, as the text passes do.
                            // RETURNING names a written table by its name, not its alias.
                            match &self.returning_table {
                                Some((alias, table)) if i == 0 && last > 0 && s.sval == *alias => {
                                    self.push(&ident(table));
                                }
                                _ => self.push(&ident(&s.sval)),
                            }
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
            // ARRAY[...] is the JSON array an array is stored as.
            NodeEnum::AArrayExpr(arr) => {
                self.push("json_array(");
                self.list(&arr.elements, Self::expr_node)?;
                self.push(")");
                Ok(())
            }
            NodeEnum::AIndirection(i) => self.subscript(i),
            // The SQL/JSON constructors, as rewrite_sql_json_constructors writes them:
            // json_array and json_object as jsonb, their NULLs left out unless NULL ON
            // NULL (JSON_ARRAY's default) or ABSENT ON NULL (JSON_OBJECT's) says.
            NodeEnum::JsonArrayConstructor(c) => {
                let values: Vec<&NodeEnum> = c
                    .exprs
                    .iter()
                    .map(|e| match node(e)? {
                        NodeEnum::JsonValueExpr(v) => boxed(v.raw_expr.as_deref()),
                        _ => Err("JSON_ARRAY item"),
                    })
                    .collect::<Result<_, _>>()?;
                self.json_constructor("json_array(", &values, c.absent_on_null)
            }
            NodeEnum::JsonObjectConstructor(c) => {
                if c.unique {
                    return Err("JSON_OBJECT WITH UNIQUE KEYS");
                }
                let mut values: Vec<&NodeEnum> = Vec::new();
                for e in &c.exprs {
                    let NodeEnum::JsonKeyValue(kv) = node(e)? else {
                        return Err("JSON_OBJECT item");
                    };
                    values.push(boxed(kv.key.as_deref())?);
                    values.push(boxed(kv.value.as_deref().and_then(|v| v.raw_expr.as_deref()))?);
                }
                self.json_constructor("json_object(", &values, c.absent_on_null)
            }
            NodeEnum::SqlvalueFunction(f) => {
                use pb::SqlValueFunctionOp as O;
                // CURRENT_DATE is the date in the session's zone. The session writes the
                // user and the schema into the statement before it is translated; what is
                // left of them is the engine's functions (rewrite_niladic_funcs).
                match O::try_from(f.op) {
                    Ok(O::SvfopCurrentDate) if self.ddl => self.push("CURRENT_DATE"),
                    Ok(O::SvfopCurrentTimestamp) if self.ddl => self.push("strftime('%Y-%m-%d %H:%M:%f')"),
                    Ok(O::SvfopCurrentTime) if self.ddl => self.push("CURRENT_TIME"),
                    Ok(O::SvfopCurrentDate) if self.typed() => self.push("overlite_current_date()"),
                    Ok(O::SvfopCurrentTimestamp) if self.typed() => self.push("overlite_now(julianday('now'))"),
                    Ok(O::SvfopCurrentUser | O::SvfopCurrentRole | O::SvfopUser) => self.push("current_user()"),
                    Ok(O::SvfopSessionUser) => self.push("session_user()"),
                    Ok(O::SvfopCurrentSchema) => self.push("current_schema()"),
                    Ok(O::SvfopCurrentCatalog) => self.push("current_database()"),
                    _ => return Err("SQL value function"),
                }
                Ok(())
            }
            // DEFAULT in a VALUES row or a SET, which the session fills in before.
            NodeEnum::SetToDefault(_) => {
                self.push("DEFAULT");
                Ok(())
            }
            NodeEnum::CollateClause(c) => self.collate(c),
            _ => Err("expression"),
        }
    }

    fn json_constructor(&mut self, call: &str, values: &[&NodeEnum], absent: bool) -> Out {
        let absent = absent && !values.is_empty();
        if absent {
            self.push("overlite_json_absent(");
        }
        self.push("(overlite_cast(");
        self.push(call);
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.expr(v, Prec::Or)?;
        }
        self.push("), 'jsonb') COLLATE JSONB)");
        if absent {
            self.push(")");
        }
        Ok(())
    }

    /// `x COLLATE name`, as rewrite_collate writes it: SQLite's and the engine's own
    /// collations by their names, a language collation or one CREATE COLLATION made by
    /// its name, quoted, a case-insensitive one as NOCASE; any other is dropped.
    fn collate(&mut self, c: &pb::CollateClause) -> Out {
        let name = match c.collname.as_slice() {
            [n] | [_, n] => string(n)?,
            _ => return Err("collation"),
        };
        let arg = boxed(c.arg.as_deref())?;
        let low = name.to_lowercase();
        let clause = if matches!(low.as_str(), "decimal" | "binary" | "nocase" | "rtrim") {
            [" COLLATE ", name].concat()
        } else if overlite_engine::collation_known(name) {
            [" COLLATE \"", &name.replace('"', "\"\""), "\""].concat()
        } else if collation_is_ci(name) {
            " COLLATE NOCASE".to_string()
        } else {
            String::new()
        };
        self.expr(arg, if clause.is_empty() { Prec::Or } else { Prec::Unary })?;
        self.push(&clause);
        Ok(())
    }

    /// A subscript of an array, `a[i]`, as rewrite_array_subscript writes it: the JSON
    /// element at i - 1, NULL past the end or where the value is no JSON array; a
    /// slice `a[i:j]` the JSON array of the elements from i to j, of a column only.
    fn subscript(&mut self, i: &pb::AIndirection) -> Out {
        let arg = boxed(i.arg.as_deref())?;
        let mut out = String::new();
        std::mem::swap(&mut self.out, &mut out);
        let r = self.expr(arg, Prec::Or);
        std::mem::swap(&mut self.out, &mut out);
        r?;
        // What is not a column may be an array in Postgres' text (string_to_array's):
        // the JSON array of it.
        if !matches!(arg, NodeEnum::ColumnRef(_)) {
            out = ["overlite_array_json(", &out, ")"].concat();
        }
        // A json or jsonb array's element may be an array itself.
        let json_elements = self.typed() && matches!(self.ty(arg).base, Base::Json | Base::Jsonb);
        // The subscripts of a run of them (`a[1][2]`), read as one path.
        let mut plain: Vec<String> = Vec::new();
        for x in &i.indirection {
            let NodeEnum::AIndices(ix) = node(x)? else {
                return Err("field selection");
            };
            if ix.is_slice && !plain.is_empty() {
                out = element(&out, &std::mem::take(&mut plain), json_elements);
            }
            let bound = |me: &mut Self, b: Option<&pb::Node>| -> Result<Option<String>, Unsupported> {
                let Some(b) = b else { return Ok(None) };
                let b = node(b)?;
                me.note_param(b, Ty::of(Base::Int4));
                let mut text = String::new();
                std::mem::swap(&mut me.out, &mut text);
                let r = me.expr(b, Prec::Or);
                std::mem::swap(&mut me.out, &mut text);
                r?;
                Ok(Some(text))
            };
            let lo = bound(self, ix.lidx.as_deref())?;
            let hi = bound(self, ix.uidx.as_deref())?;
            if ix.is_slice && (!matches!(arg, NodeEnum::ColumnRef(_)) || i.indirection.len() > 1) {
                // Of any other expression the engine's, which reads it once.
                out = [
                    "overlite_array(overlite_array_slice(",
                    &out,
                    ", ",
                    lo.as_deref().unwrap_or("NULL"),
                    ", ",
                    hi.as_deref().unwrap_or("NULL"),
                    "))",
                ]
                .concat();
            } else if ix.is_slice {
                let mut conds = Vec::new();
                if let Some(lo) = lo {
                    conds.push(["key + 1 >= (", &lo, ")"].concat());
                }
                if let Some(hi) = hi {
                    conds.push(["key + 1 <= (", &hi, ")"].concat());
                }
                let filter = if conds.is_empty() {
                    String::new()
                } else {
                    [" WHERE ", &conds.join(" AND ")].concat()
                };
                out = [
                    "CASE WHEN json_valid(",
                    &out,
                    ") THEN overlite_array((SELECT json_group_array(value) FROM json_each(",
                    &out,
                    ")",
                    &filter,
                    ")) END",
                ]
                .concat();
            } else {
                let idx = hi.ok_or("subscript")?;
                plain.push(idx);
            }
        }
        if !plain.is_empty() {
            out = element(&out, &plain, json_elements);
        }
        self.push(&out);
        Ok(())
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
        for (i, n) in nodes.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.note_param(n, common);
            self.value(n, common, Prec::Or)?;
        }
        Ok(())
    }

    /// ctid, the row's identifier, which a table calls rowid in SQLite: None for a
    /// reference to another column.
    fn ctid(&mut self, fields: &[pb::Node]) -> Result<Option<String>, Unsupported> {
        if !matches!(fields.last().map(string), Some(Ok("ctid"))) {
            return Ok(None);
        }
        let parts: Vec<&str> = match fields.iter().map(string).collect::<Result<Vec<_>, _>>() {
            Ok(p) if p.last() == Some(&"ctid") => p,
            _ => return Ok(None),
        };
        // A table may have a column of that name, which SQLite lets it have.
        if let Some(t) = &mut self.typer
            && t.column(fields).is_some_and(|c| c.ty.known())
        {
            return Ok(None);
        }
        let scope = self.froms.last().map_or(&[][..], Vec::as_slice);
        match parts.as_slice() {
            // An output column named ctid, in the ORDER BY that reads it.
            ["ctid"] if self.ctid_output => Ok(Some("ctid".to_string())),
            ["ctid"] if !scope.is_empty() && scope.iter().all(|(_, table)| *table) => Ok(Some("rowid".to_string())),
            // A CTE's or a subquery's column it names ctid (its table's rowid).
            ["ctid"] if scope.len() == 1 => Ok(Some("ctid".to_string())),
            [q, "ctid"] => {
                for scope in self.froms.iter().rev() {
                    if let Some((_, table)) = scope.iter().find(|(n, _)| n == q) {
                        return Ok(Some(
                            [ident(q).as_str(), if *table { ".rowid" } else { ".ctid" }].concat(),
                        ));
                    }
                }
                Err("system column")
            }
            _ => Err("system column"),
        }
    }

    /// A column of a table function written in place, under the name SQLite gives it
    /// (`u.x` is `u.value`), when fields name one.
    fn renamed(&self, fields: &[pb::Node]) -> Result<Option<String>, Unsupported> {
        if self.renames.is_empty() {
            return Ok(None);
        }
        let parts: Vec<&str> = match fields.iter().map(string).collect::<Result<Vec<_>, _>>() {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let found = match parts.as_slice() {
            [alias, col] => self
                .renames
                .iter()
                .rev()
                .find(|(a, _)| a == alias)
                .and_then(|(a, cols)| cols.iter().find(|(n, _)| n == col).map(|(_, w)| (a, w))),
            [col] => self
                .renames
                .iter()
                .rev()
                .find_map(|(a, cols)| cols.iter().find(|(n, _)| n == col).map(|(_, w)| (a, w))),
            _ => None,
        };
        let _ = &found;
        Ok(found.map(|(_, written)| written.clone()))
    }

    fn constant(&mut self, k: &pb::AConst) -> Out {
        if k.isnull {
            self.push("NULL");
            return Ok(());
        }
        match k.val.as_ref().ok_or("constant")? {
            Val::Ival(i) => self.push(&i.ival.to_string()),
            // An integer past int4 the grammar leaves as text is a bigint where it
            // fits one, -9223372036854775808 included, which SQLite reads as one too.
            Val::Fval(f) if f.fval.parse::<i64>().is_ok() => self.push(&f.fval),
            // A number with more digits than a double keeps is a numeric's text.
            // Its sign inside, so that the item stays a numeric.
            Val::Fval(f) if self.typed() && is_long_number(&f.fval) => {
                self.push("(CAST(overlite_numeric(");
                self.push(&quote_literal(&f.fval));
                self.push(") AS DECIMALTEXT) COLLATE DECIMAL)");
            }
            Val::Fval(f) => self.push(&f.fval),
            Val::Boolval(b) => self.push(if b.boolval { "true" } else { "false" }),
            Val::Sval(s) => self.push(&quote_literal(&s.sval)),
            // X'1F' is a bit string in Postgres; the text passes leave it to SQLite, which
            // reads a blob, and clients write bytes so.
            Val::Bsval(b) => match b.bsval.strip_prefix('x') {
                Some(hex) if hex.bytes().all(|c| c.is_ascii_hexdigit()) => {
                    self.push("X'");
                    self.push(hex);
                    self.push("'");
                }
                _ => return Err("bit string"),
            },
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
        if !self.typed() {
            return self.expr(n, min);
        }
        if !(is_lit || is_param) {
            // What Postgres converts on its own where a value meets a wider type: a date
            // or a timestamp meeting a timestamptz, a date meeting a timestamp.
            let from = self.ty(n);
            return match target {
                t if t.is(Base::Timestamptz) && (from.is(Base::Timestamp) || from.is(Base::Date)) => {
                    self.as_timestamptz(n)
                }
                t if t.is(Base::Timestamp) && from.is(Base::Date) => {
                    self.push("overlite_cast(");
                    self.expr(n, Prec::Or)?;
                    self.push(", 'timestamp')");
                    Ok(())
                }
                _ => self.expr(n, min),
            };
        }
        self.note_param(n, target);
        match target {
            t if t.is(Base::Timestamptz) => self.as_timestamptz(n)?,
            t if t.is(Base::Timestamp) || t.is(Base::Date) => {
                self.push("overlite_cast(");
                self.expr(n, Prec::Or)?;
                self.push(if t.is(Base::Date) {
                    ", 'date')"
                } else {
                    ", 'timestamp')"
                });
            }
            // A range in its canonical form, `[1,5]` as `[1,6)`.
            t if !t.array && range_name(t.base).is_some() => {
                self.push("overlite_cast(");
                self.expr(n, Prec::Or)?;
                self.push(", '");
                self.push(range_name(t.base).unwrap_or("int4range"));
                self.push("')");
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

    /// n as a timestamptz, where it is not one: the one conversion of a timestamp, a
    /// date or text to a timestamptz, a wall clock in the session's zone read when the
    /// statement runs (overlite_tz_in, which reads each element of an array so too), as
    /// Postgres converts it in the session's TimeZone. A timestamptz goes as it is.
    fn as_timestamptz(&mut self, n: &NodeEnum) -> Out {
        if self.typed() && self.ty(n).base == Base::Timestamptz {
            return self.expr(n, Prec::Or);
        }
        self.push("overlite_tz_in(");
        self.expr(n, Prec::Or)?;
        self.push(")");
        Ok(())
    }

    /// A cast: to text SQLite's own CAST, to boolean the engine's boolin, to any other
    /// type the engine knows the engine's input function. With the environment, a
    /// timestamptz cast to text, a date or a time is read in the session's zone, and a
    /// value cast to timestamptz that is not one is a wall clock there.
    fn cast(&mut self, t: &pb::TypeCast) -> Out {
        let ty = t.type_name.as_ref().ok_or("cast type")?;
        let arg = boxed(t.arg.as_deref())?;
        if !ty.array_bounds.is_empty() {
            // An array: the engine reads it and makes each element its type, whatever
            // form it comes in (a stored JSON array, Postgres' text, a parameter), as
            // the text passes write a cast of a literal or a parameter
            // (rewrite_array_cast).
            if ty.setof || ty.pct_type {
                return Err("array or SETOF type");
            }
            let mut elem = ty.clone();
            elem.array_bounds.clear();
            let name = cast_type(&elem)?.replace(", ", ",");
            let target = types::of_type_name(ty);
            if self.typed() {
                self.note_param(arg, target);
            }
            self.push("overlite_assign(");
            if self.typed() && target.base == Base::Timestamptz {
                self.as_timestamptz(arg)?;
            } else {
                self.expr(arg, Prec::Or)?;
            }
            self.push(", ");
            self.push(&quote_literal(&[name.as_str(), "[]"].concat()));
            self.push(")");
            return Ok(());
        }
        let name = cast_type(ty)?;
        if let Some(lookup) = reg_lookup(&name) {
            return self.reg_cast(arg, &name, lookup);
        }
        if self.table_ddl && !matches!(name.as_str(), "regconfig" | "tsquery" | "tsvector" | "hstore") {
            return self.table_ddl_cast(arg, &name);
        }
        if self.typed() {
            let target = types::of_type_name(ty);
            self.note_param(arg, target);
            let from = self.ty(arg);
            if from.is(Base::TstzRange) && matches!(target.base, Base::Text | Base::Varchar) && !target.array {
                // Its bounds in the session's zone, as range_out writes them.
                self.push("overlite_tstzrange_text(");
                self.expr(arg, Prec::Or)?;
                self.push(")");
                return Ok(());
            }
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
                self.push("overlite_cast(");
                self.as_timestamptz(arg)?;
                self.push(", ");
                self.push(&quote_literal(&name));
                self.push(")");
                return Ok(());
            }
        }
        match name.as_str() {
            // A text search configuration is its name; a query and a vector are read by
            // the engine's input functions (rewrite_fts).
            "regconfig" => self.expr(arg, Prec::Atom)?,
            "tsquery" | "tsvector" if is_string(arg) => {
                self.push(if name == "tsquery" {
                    "tsquery_in("
                } else {
                    "tsvector_in("
                });
                self.expr(arg, Prec::Or)?;
                self.push(")");
            }
            "tsquery" | "tsvector" => self.expr(arg, Prec::Atom)?,
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
            // An array goes to text in Postgres' form (`{a,b}`), not as the JSON array
            // it is stored as.
            "text" if self.yields_array(arg) => {
                self.push("overlite_array_text(");
                self.expr(arg, Prec::Or)?;
                self.push(")");
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

    /// A cast in a table's DDL, as the text passes leave one there (typed_cast.rs):
    /// SQLite's own CAST, so that the file's schema reads without overlite's
    /// functions, a string literal folded to the value it makes (`'5'::integer` is 5, a
    /// uuid its canonical text, `'t'::boolean` 1), a boolean read by the engine's
    /// boolin.
    fn table_ddl_cast(&mut self, arg: &NodeEnum, name: &str) -> Out {
        if let NodeEnum::AConst(k) = arg
            && let Some(Val::Sval(s)) = &k.val
            && let Some(v) = fold_literal(&quote_literal(&s.sval), name)
        {
            self.push(&v);
            return Ok(());
        }
        if name == "boolean" {
            self.push("pg_bool(");
            self.expr(arg, Prec::Or)?;
            self.push(")");
            return Ok(());
        }
        self.push("CAST(");
        self.expr(arg, Prec::Or)?;
        self.push(" AS ");
        self.push(name);
        self.push(")");
        Ok(())
    }

    /// A cast to an object identifier type that names its object (regclass, regtype,
    /// ...), as rewrite_casts writes it: a number, quoted or not, is the oid; a quoted
    /// name the oid of the object of that name, of any schema (and a system catalog's
    /// own, which pg_class does not list); anything else an oid, written as the
    /// object's name, which is how psql reads one.
    fn reg_cast(&mut self, arg: &NodeEnum, name: &str, (col, table): (&str, &str)) -> Out {
        if let NodeEnum::AConst(k) = arg {
            match &k.val {
                Some(Val::Ival(i)) => {
                    self.push(&i.ival.to_string());
                    return Ok(());
                }
                Some(Val::Sval(v)) => {
                    let v = v.sval.trim();
                    if !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()) {
                        self.push(v);
                        return Ok(());
                    }
                    if name == "regclass"
                        && let Some(oid) = system_catalog_oid(v)
                    {
                        self.push(oid);
                        return Ok(());
                    }
                    let (name, quoted) = object_name(v);
                    self.push("(SELECT oid FROM ");
                    self.push(table);
                    self.push(" WHERE ");
                    self.push(col);
                    self.push(" = ");
                    self.push(&quote_literal(&name));
                    // An unquoted name is folded, and the catalog keeps a name as it
                    // was created: SQLite's names are one whatever their case.
                    if !quoted {
                        self.push(" COLLATE NOCASE");
                    }
                    self.push(")");
                    return Ok(());
                }
                _ => {}
            }
        }
        if let NodeEnum::ParamRef(_) = arg {
            // A parameter is the text of a name (or of an oid), as a literal is, read
            // when the statement runs: its schema and quotes taken off.
            self.push("(SELECT oid FROM ");
            self.push(table);
            self.push(" WHERE ");
            self.push(col);
            self.push(" = trim(substr(");
            self.expr(arg, Prec::Or)?;
            self.push(", instr(");
            self.expr(arg, Prec::Or)?;
            self.push(", '.') + 1), '\"') COLLATE NOCASE OR CAST(oid AS TEXT) = CAST(");
            self.expr(arg, Prec::Or)?;
            self.push(" AS TEXT) LIMIT 1)");
            return Ok(());
        }
        self.push("(SELECT ");
        self.push(col);
        self.push(" FROM ");
        self.push(table);
        self.push(" WHERE oid = ");
        self.expr(arg, Prec::Compare)?;
        self.push(")");
        Ok(())
    }

    fn a_expr(&mut self, a: &pb::AExpr) -> Out {
        use pb::AExprKind as K;
        let kind = K::try_from(a.kind).map_err(|_| "operator kind")?;
        let op = match a.name.as_slice() {
            [n] => string(n)?,
            // OPERATOR(pg_catalog.~) is the operator a bare ~ names.
            [schema, n] if string(schema)? == "pg_catalog" => string(n)?,
            _ => return Err("qualified operator"),
        };
        match kind {
            K::AexprOp => {
                let Some(r) = a.rexpr.as_deref() else {
                    return Err("postfix operator");
                };
                let r = node(r)?;
                let Some(l) = a.lexpr.as_deref() else {
                    // Postgres' math prefixes, as rewrite_math_operators writes them.
                    if let Some(f) = match op {
                        "|/" => Some("sqrt("),
                        "||/" => Some("cbrt("),
                        "@" => Some("abs("),
                        _ => None,
                    } {
                        self.push(f);
                        self.expr(r, Prec::Or)?;
                        self.push(")");
                        return Ok(());
                    }
                    if op == "~" {
                        self.push("~");
                        return self.expr(r, Prec::Unary);
                    }
                    // A text search query negated (`!!` is tsquery's alone).
                    if op == "!!" {
                        self.push("overlite_tsquery_op('!', ");
                        self.expr(r, Prec::Or)?;
                        self.push(", NULL)");
                        return Ok(());
                    }
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
                if let (NodeEnum::RowExpr(lr), NodeEnum::RowExpr(rr)) = (l, r) {
                    return self.row_comparison(op, &lr.args, &rr.args, a.location);
                }
                // `(a, b) < (SELECT x, y ...)`: SQLite's row value against the
                // subquery's row, compared in Postgres' order.
                if let (NodeEnum::RowExpr(_), NodeEnum::SubLink(sub)) = (l, r)
                    && sub.sub_link_type == SubLinkType::ExprSublink as i32
                    && matches!(op, "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=")
                {
                    self.row_or_expr(l)?;
                    self.push(" ");
                    self.push(if op == "!=" { "<>" } else { op });
                    self.push(" ");
                    return self.subquery(sub);
                }
                if matches!(op, "~" | "~*" | "!~" | "!~*") {
                    return self.regex(op, l, r);
                }
                if matches!(op, "=" | "<>" | "!=")
                    && let Some(truth) = self.catalog_bool(l, r)
                {
                    self.expr(l, Prec::Compare)?;
                    self.push(" ");
                    self.push(op);
                    self.push(if truth { " 1" } else { " 0" });
                    return Ok(());
                }
                if matches!(op, "&&" | "||")
                    && self.typed()
                    && (self.ty(l).is(Base::Tsquery) || self.ty(r).is(Base::Tsquery))
                {
                    // Two text search queries joined, the engine's (the text pass took
                    // `&&` for an overlap).
                    self.push("overlite_tsquery_op(");
                    self.push(if op == "&&" { "'&', " } else { "'|', " });
                    self.expr(l, Prec::Or)?;
                    self.push(", ");
                    self.expr(r, Prec::Or)?;
                    self.push(")");
                    return Ok(());
                }
                if matches!(op, "#" | "^") && !(self.typed() && (self.is_range(l) || self.is_range(r))) {
                    return self.math_call(op, l, r);
                }
                // pg_trgm's word similarity and distances, which only text has.
                if let Some(f) = match op {
                    "<%" | "%>" | "<<%" | "%>>" => Some("overlite_trgm_match("),
                    "<->" | "<<->" | "<->>" | "<<<->" | "<->>>" => Some("overlite_trgm_dist("),
                    _ => None,
                } {
                    self.note_param(l, Ty::TEXT);
                    self.note_param(r, Ty::TEXT);
                    self.push(f);
                    self.expr(l, Prec::Or)?;
                    self.push(", ");
                    self.expr(r, Prec::Or)?;
                    self.push(", ");
                    self.push(&quote_literal(op));
                    self.push(")");
                    return Ok(());
                }
                if self.typed() {
                    return self.typed_op(a, op, l, r);
                }
                // `%` is pg_trgm's on text: only the environment tells it from a remainder.
                if op == "%" {
                    return Err("operator");
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
                if let NodeEnum::RowExpr(row) = l {
                    let rows = items
                        .items
                        .iter()
                        .map(|x| match node(x)? {
                            NodeEnum::RowExpr(r) => Ok(r.args.as_slice()),
                            _ => Err("row IN a list of what is not a row"),
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    return self.row_in_list(&row.args, &rows, op == "<>", a.location);
                }
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
            // `x = ANY (array)` and `x <> ALL (array)`: a list in ARRAY[...] is an IN list,
            // as rewrite_any_array writes it, any other array the elements json_each
            // reads of it, whatever form it is in (rewrite_any_array_expr).
            K::AexprOpAny | K::AexprOpAll => {
                let not = match (kind, op) {
                    (K::AexprOpAny, "=") => false,
                    (K::AexprOpAll, "<>" | "!=") => true,
                    _ => return Err("ANY or ALL of another operator"),
                };
                let (l, r) = (boxed(a.lexpr.as_deref())?, boxed(a.rexpr.as_deref())?);
                let tl = self.ty(l);
                self.expr(l, Prec::Compare)?;
                self.push(if not { " NOT IN (" } else { " IN (" });
                if let NodeEnum::AArrayExpr(arr) = r {
                    for (i, x) in arr.elements.iter().enumerate() {
                        if i > 0 {
                            self.push(", ");
                        }
                        self.value(node(x)?, tl, Prec::Or)?;
                    }
                } else {
                    if tl.known() && !tl.array {
                        self.note_param(
                            r,
                            Ty {
                                base: tl.base,
                                array: true,
                            },
                        );
                    }
                    self.push("SELECT value FROM json_each(overlite_array_json(");
                    self.expr(r, Prec::Or)?;
                    self.push("))");
                }
                self.push(")");
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

    /// A comparison of two rows, as Postgres makes it of their members: `=` all of
    /// them equal, `<>` any of them not, and the order ones the first pair that is not
    /// equal deciding, each pair compared as two values of its types are. NULL where a
    /// pair that decides is NULL, as in Postgres.
    fn row_comparison(&mut self, op: &str, l: &[pb::Node], r: &[pb::Node], at: i32) -> Out {
        if l.len() != r.len() {
            return Err(refuse(Refusal {
                state: overlite_core::SqlState::SYNTAX_ERROR,
                message: "unequal number of entries in row expressions".to_string(),
                hint: None,
                location: location(at),
            }));
        }
        let l: Vec<&NodeEnum> = l.iter().map(node).collect::<Result<_, _>>()?;
        let r: Vec<&NodeEnum> = r.iter().map(node).collect::<Result<_, _>>()?;
        self.push("(");
        match op {
            "=" | "<>" | "!=" => {
                let join = if op == "=" { " AND " } else { " OR " };
                for (i, (x, y)) in l.iter().zip(&r).enumerate() {
                    if i > 0 {
                        self.push(join);
                    }
                    self.row_pair(op, x, y)?;
                }
            }
            "<" | "<=" | ">" | ">=" => {
                // a1 < b1 OR (a1 = b1 AND (a2 < b2 OR (a2 = b2 AND ... an <= bn)))
                let strict = &op[..1];
                let last = l.len() - 1;
                for i in 0..last {
                    self.row_pair(strict, l[i], r[i])?;
                    self.push(" OR (");
                    self.row_pair("=", l[i], r[i])?;
                    self.push(" AND (");
                }
                self.row_pair(op, l[last], r[last])?;
                for _ in 0..last {
                    self.push("))");
                }
            }
            _ => {
                return Err(refuse(Refusal {
                    state: overlite_core::SqlState::UNDEFINED_FUNCTION,
                    message: format!("could not determine interpretation of row comparison operator {op}"),
                    hint: None,
                    location: location(at),
                }));
            }
        }
        self.push(")");
        Ok(())
    }

    /// One pair of members of two rows compared, in parentheses.
    fn row_pair(&mut self, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        self.push("(");
        if self.typed() {
            self.comparison(op, l, r)?;
        } else {
            let p = binary_prec(op).ok_or("operator")?;
            self.expr(l, next(p))?;
            self.push(" ");
            self.push(op);
            self.push(" ");
            self.expr(r, next(p))?;
        }
        self.push(")");
        Ok(())
    }

    /// `(a, b) IN ((1, 2), (3, 4))`: the row equal to any of them, NOT IN unequal to
    /// all, as Postgres reads it. The terms are joined two by two, so that a long list
    /// nests no deeper than its logarithm (SQLite bounds an expression's depth).
    fn row_in_list(&mut self, l: &[pb::Node], rows: &[&[pb::Node]], not: bool, at: i32) -> Out {
        match rows {
            [] => Err("empty IN list"),
            [one] => self.row_comparison(if not { "<>" } else { "=" }, l, one, at),
            _ => {
                let (a, b) = rows.split_at(rows.len() / 2);
                self.push("(");
                self.row_in_list(l, a, not, at)?;
                self.push(if not { " AND " } else { " OR " });
                self.row_in_list(l, b, not, at)?;
                self.push(")");
                Ok(())
            }
        }
    }

    /// Whether n is an array: by its type, or by what makes one whatever its elements
    /// (array_agg, ARRAY[...], ARRAY(subquery), a cast to an array type).
    fn yields_array(&mut self, n: &NodeEnum) -> bool {
        match n {
            NodeEnum::FuncCall(f) if func_name(f) == Some("array_agg") => true,
            NodeEnum::AArrayExpr(_) => true,
            NodeEnum::SubLink(l) => l.sub_link_type == SubLinkType::ArraySublink as i32,
            NodeEnum::TypeCast(t) => t.type_name.as_ref().is_some_and(|t| !t.array_bounds.is_empty()),
            _ => self.typed() && self.ty(n).array,
        }
    }

    /// `a # b` (xor of integers) and `a ^ b` (power), which SQLite has as functions, as
    /// rewrite_math_operators writes them.
    fn math_call(&mut self, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        self.push(if op == "#" { "overlite_xor(" } else { "power(" });
        self.expr(l, Prec::Or)?;
        self.push(", ");
        self.expr(r, Prec::Or)?;
        self.push(")");
        Ok(())
    }

    /// The truth a string literal compared with a boolean column of the catalog says:
    /// the catalog's views keep their booleans as 1 and 0, which SQLite does not
    /// compare equal to 't' or 'f' (rewrite_catalog_bool_literals). A column of a table
    /// the environment knows is read by its own type instead.
    fn catalog_bool(&mut self, l: &NodeEnum, r: &NodeEnum) -> Option<bool> {
        let NodeEnum::ColumnRef(c) = l else { return None };
        let name = c.fields.last().and_then(|f| string(f).ok())?;
        if !crate::types::BOOL_CATALOG_COLUMNS.contains(name) || self.ty(l).known() {
            return None;
        }
        let NodeEnum::AConst(k) = r else { return None };
        let Some(Val::Sval(v)) = &k.val else { return None };
        match v.sval.to_ascii_lowercase().as_str() {
            "t" | "true" | "y" | "yes" | "on" | "1" => Some(true),
            "f" | "false" | "n" | "no" | "off" | "0" => Some(false),
            _ => None,
        }
    }

    /// The POSIX regular expression operators, as SQLite's REGEXP over the engine's
    /// regexp(): `~*` and `!~*` with the pattern's `(?i)`, as rewrite_match_operators
    /// writes them for the text passes.
    fn regex(&mut self, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        self.note_param(l, Ty::TEXT);
        self.note_param(r, Ty::TEXT);
        let not = op.starts_with('!');
        if not {
            self.push("NOT (");
        }
        self.expr(l, Prec::Compare)?;
        self.push(" REGEXP ");
        if op.ends_with('*') {
            match r {
                NodeEnum::AConst(k) if matches!(k.val, Some(Val::Sval(_))) => {
                    let Some(Val::Sval(p)) = &k.val else {
                        return Err("pattern");
                    };
                    self.push(&quote_literal(&["(?i)", &p.sval].concat()));
                }
                _ => {
                    self.push("('(?i)' || ");
                    self.expr(r, Prec::Concat)?;
                    self.push(")");
                }
            }
        } else {
            self.expr(r, Prec::Compare)?;
        }
        if not {
            self.push(")");
        }
        Ok(())
    }

    /// A binary operator with the environment: by its operands' types.
    fn typed_op(&mut self, a: &pb::AExpr, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        match op {
            "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" => self.comparison(op, l, r),
            "+" | "-" | "*" if self.is_range(l) || self.is_range(r) => self.range_set_op(op, l, r, a.location),
            "+" | "-" | "*" | "/" => self.arithmetic(a, op),
            "||" => {
                let (tl, tr) = (self.ty(l), self.ty(r));
                if tl.array || tr.array {
                    // Two arrays end to end, or an element put at one end.
                    let kinds = match (tl.array, tr.array) {
                        (true, true) => "aa",
                        (true, false) if tr.base == Base::Unknown && is_string(r) => "aa",
                        (true, false) => "ae",
                        (false, true) if tl.base == Base::Unknown && is_string(l) => "aa",
                        _ => "ea",
                    };
                    self.push("overlite_array_cat(");
                    self.array_value(l, tr, kinds.starts_with('a'))?;
                    self.push(", ");
                    self.array_value(r, tl, kinds.ends_with('a'))?;
                    self.push(", '");
                    self.push(kinds);
                    self.push("')");
                    return Ok(());
                }
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
            "@>" | "<@" | "&&" | "-|-" | "<<" | ">>" | "&<" | "&>" if self.is_range(l) || self.is_range(r) => {
                self.range_op(op, l, r, a.location)
            }
            // Only ranges have these here (and geometry, which overlite has not), and a
            // shift beside a range's literal is a range's.
            "-|-" | "&<" | "&>" => self.range_op(op, l, r, a.location),
            "<<" | ">>" if is_range_literal(l) || is_range_literal(r) => self.range_op(op, l, r, a.location),
            "@>" | "<@" | "&&" if self.ty(l).array || self.ty(r).array => self.array_op(op, l, r),
            // A range's literal containing what the catalog does not know, a range or an
            // element: json_contains reads either.
            "@>" | "<@" if is_range_literal(if op == "@>" { l } else { r }) => {
                let (c, e) = if op == "@>" { (l, r) } else { (r, l) };
                self.push("json_contains(");
                self.expr(c, Prec::Or)?;
                self.push(", ");
                self.expr(e, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            "->" | "->>" | "#>" | "#>>" | "@>" | "<@" | "?" | "?|" | "?&" => self.json_op(a, op, l, r),
            // Full-text search's match, the engine's (rewrite_fts), the vector first.
            "@@" => {
                let (v, q) = if self.ty(l).is(Base::Tsquery) { (r, l) } else { (l, r) };
                self.push("ts_match(");
                self.expr(v, Prec::Or)?;
                self.push(", ");
                self.expr(q, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            // Overlap of what the catalog does not know, an array or a range, as the
            // text passes write it.
            "&&" if self.ty(l).base == Base::Any || self.ty(r).base == Base::Any => {
                self.push("overlite_overlaps(");
                self.expr(l, Prec::Or)?;
                self.push(", ");
                self.expr(r, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            // The bitwise operators of integers, SQLite's own.
            "&" | "|" | "<<" | ">>" => {
                let p = binary_prec(op).ok_or("operator")?;
                self.expr(l, p)?;
                self.push(" ");
                self.push(op);
                self.push(" ");
                self.expr(r, next(p))
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
                // Of numbers a remainder, and of what the catalog does not know, as the
                // text passes take it.
                let num = |t: Ty| t.is_integer() || t.is_numeric() || t.base == Base::Any;
                if num(tl) && num(tr) {
                    self.expr(l, Prec::Mul)?;
                    self.push(" % ");
                    return self.expr(r, Prec::Concat);
                }
                Err(no_operator("%", tl, tr, a.location))
            }
            _ => Err("operator"),
        }
    }

    /// The array operators: containment as json_contains reads two JSON arrays, overlap
    /// as overlite_overlaps, the text passes' functions, each side an array or a
    /// literal or a parameter read as the other's type.
    fn array_op(&mut self, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        let (tl, tr) = (self.ty(l), self.ty(r));
        let side = |t: Ty| t.array || matches!(t.base, Base::Unknown | Base::Any);
        if !(side(tl) && side(tr)) {
            return Err("array operator with a side that is no array");
        }
        let (a, b, ta, tb) = if op == "<@" { (r, l, tr, tl) } else { (l, r, tl, tr) };
        self.push(if op == "&&" {
            "overlite_overlaps("
        } else {
            "json_contains("
        });
        self.array_value(a, tb, true)?;
        self.push(", ");
        self.array_value(b, ta, true)?;
        self.push(")");
        Ok(())
    }

    /// n beside an array of type other: an untyped literal or a parameter read as that
    /// array when it stands for one, anything else as it is.
    fn array_value(&mut self, n: &NodeEnum, other: Ty, array: bool) -> Out {
        let lit = is_string(n) || matches!(n, NodeEnum::ParamRef(_));
        if !(array && lit && self.ty(n).base == Base::Unknown && other.array && other.known()) {
            if !array && other.array {
                self.note_param(n, Ty::of(other.base));
            }
            return self.expr(n, Prec::Or);
        }
        self.note_param(n, other);
        self.push("overlite_assign(");
        self.expr(n, Prec::Or)?;
        self.push(", ");
        let elem = Ty::of(other.base).cast_name().or(match other.base {
            Base::Int2 => Some("smallint"),
            Base::Int4 => Some("integer"),
            Base::Int8 => Some("bigint"),
            Base::Float4 => Some("real"),
            Base::Float8 => Some("double precision"),
            Base::Bool => Some("boolean"),
            Base::Text => Some("text"),
            Base::Varchar => Some("varchar"),
            Base::Uuid => Some("uuid"),
            Base::Jsonb => Some("jsonb"),
            Base::Json => Some("json"),
            _ => None,
        });
        self.push(&quote_literal(&[elem.unwrap_or("text"), "[]"].concat()));
        self.push(")");
        Ok(())
    }

    /// Whether the environment knows n is a range.
    fn is_range(&mut self, n: &NodeEnum) -> bool {
        let t = self.ty(n);
        !t.array
            && matches!(
                t.base,
                Base::Int4Range | Base::Int8Range | Base::NumRange | Base::DateRange | Base::TsRange | Base::TstzRange
            )
    }

    /// A range's union (`+`), difference (`-`) or intersection (`*`), as the engine
    /// computes them (overlite_range_setop) over the range type's bounds.
    fn range_set_op(&mut self, op: &str, l: &NodeEnum, r: &NodeEnum, at: i32) -> Out {
        let (tl, tr) = (self.ty(l), self.ty(r));
        let ty = if tl.is_range() { tl } else { tr };
        let ok = |t: Ty| t == ty || matches!(t.base, Base::Unknown | Base::Any);
        if !(ok(tl) && ok(tr)) {
            return Err(no_operator(op, tl, tr, at));
        }
        self.note_param(l, ty);
        self.note_param(r, ty);
        self.push("overlite_range_setop(");
        self.push(&quote_literal(op));
        self.push(", ");
        self.value(l, ty, Prec::Or)?;
        self.push(", ");
        self.value(r, ty, Prec::Or)?;
        self.push(", ");
        self.push(&quote_literal(range_name(ty.base).unwrap_or("")));
        self.push(")");
        Ok(())
    }

    /// A range operator or comparison over ranges, as the engine reads them
    /// (overlite_range_op): their bounds, where SQLite compared their texts. The other
    /// side is a range too, or a literal or a parameter read as one.
    fn range_op(&mut self, op: &str, l: &NodeEnum, r: &NodeEnum, at: i32) -> Out {
        let (tl, tr) = (self.ty(l), self.ty(r));
        let ok = |me: &mut Self, n: &NodeEnum, t: Ty| me.is_range(n) || matches!(t.base, Base::Unknown | Base::Any);
        if matches!(op, "@>" | "<@") {
            // A range containing an element: Postgres' anyrange @> anyelement, which
            // json_contains reads of a range (the text passes' function).
            let (c, e, tc, te) = if op == "@>" { (l, r, tl, tr) } else { (r, l, tr, tl) };
            if (self.is_range(c) || tc.base == Base::Any) && !self.is_range(e) && !te.array && te.known() {
                self.note_param(c, tc);
                self.push("json_contains(");
                self.expr(c, Prec::Or)?;
                self.push(", ");
                self.expr(e, Prec::Or)?;
                self.push(")");
                return Ok(());
            }
        }
        if !(ok(self, l, tl) && ok(self, r, tr)) {
            return Err(no_operator(op, tl, tr, at));
        }
        self.note_param(l, tr);
        self.note_param(r, tl);
        self.push("pg_bool(overlite_range_op(");
        self.push(&quote_literal(op));
        self.push(", ");
        self.value(l, tr, Prec::Or)?;
        self.push(", ");
        self.value(r, tl, Prec::Or)?;
        self.push("))");
        Ok(())
    }

    /// The json and jsonb operators, where the environment knows the left side is one
    /// (hstore has `->` and `?` too, arrays and ranges `@>`), as the text passes write
    /// them: `->` SQLite's, a chain of them in the JSONB collation, which compares and
    /// sorts jsonb as jsonb; `->>` SQLite's, made to give text by the engine; `#>` and
    /// `#>>` overlite_jsonb_path; `@>` and `<@` json_contains; `?`, `?|` and `?&`
    /// jsonb_exists, jsonb_exists_any and jsonb_exists_all.
    fn json_op(&mut self, a: &pb::AExpr, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        let (tl, tr) = (self.ty(l), self.ty(r));
        // What the catalog does not know is taken for json, as the text passes take it:
        // hstore, stored as JSON text, answers `->`, `?` and `@>` the same. Containment
        // is the container's to say, the right side of `<@`; over what the catalog does
        // not know it is json_contains whatever the other side, which reads a range
        // containing an element too.
        let (lead, other) = if op == "<@" { (tr, tl) } else { (tl, tr) };
        if lead.is(Base::Hstore) {
            return self.hstore_op(a, op, l, r);
        }
        let any_json = lead.base == Base::Any && !lead.array && !(matches!(op, "@>" | "<@") && other.array);
        if !(lead.is(Base::Jsonb) || lead.is(Base::Json) || any_json) {
            // What Postgres has no such operator for (an array's `->`, a text's `?`):
            // its error, where both sides' types are known.
            return Err(no_operator(op, tl, tr, a.location));
        }
        match op {
            "->" | "->>" => {
                self.note_param(r, Ty::TEXT);
                // The outermost `->` of a chain carries the collation; a `->>` ends one.
                let chained =
                    |n: &NodeEnum| matches!(n, NodeEnum::AExpr(x) if op_name(x) == Some("->") && x.lexpr.is_some());
                let top = op == "->" && !self.in_arrow;
                if top {
                    self.push("(");
                }
                let inner = std::mem::replace(&mut self.in_arrow, op == "->");
                let r1 = if chained(l) && op == "->" {
                    self.expr(l, Prec::Concat)
                } else {
                    let was = std::mem::replace(&mut self.in_arrow, false);
                    let r = self.expr(l, Prec::Concat);
                    self.in_arrow = was;
                    r
                };
                self.in_arrow = inner;
                r1?;
                self.push(if op == "->" { " -> " } else { " ->> " });
                // A key SQLite's `->` would not take for a label (one opening as a path
                // does, or holding what JSON escapes) as the path to it, quoted as the
                // key is written in the stored text. (`->>` is the engine's, by key.)
                if op == "->"
                    && let NodeEnum::AConst(k) = r
                    && let Some(Val::Sval(key)) = &k.val
                    && (key.sval.starts_with('$') || key.sval.chars().any(|c| c == '\\' || c == '"' || c < ' '))
                {
                    let mut path = String::from("$.\"");
                    for c in key.sval.chars() {
                        match c {
                            '\\' => path.push_str("\\\\"),
                            '"' => path.push_str("\\\""),
                            c if c < ' ' => path.push_str(&format!("\\u{:04x}", u32::from(c))),
                            c => path.push(c),
                        }
                    }
                    path.push('"');
                    self.push(&quote_literal(&path));
                    if top {
                        self.push(") COLLATE JSONB");
                    }
                    return Ok(());
                }
                let was = std::mem::replace(&mut self.in_arrow, false);
                let r2 = self.expr(r, Prec::Unary);
                self.in_arrow = was;
                r2?;
                if top {
                    self.push(") COLLATE JSONB");
                }
                let _ = a;
                Ok(())
            }
            "#>" | "#>>" => {
                let jsonb = op == "#>";
                if jsonb {
                    self.push("(");
                }
                self.push("overlite_jsonb_path(");
                self.expr(l, Prec::Or)?;
                self.push(", ");
                self.expr(r, Prec::Or)?;
                self.push(if jsonb { ", 0) COLLATE JSONB)" } else { ", 1)" });
                Ok(())
            }
            "@>" | "<@" => {
                let (container, contained) = if op == "@>" { (l, r) } else { (r, l) };
                self.push("json_contains(");
                self.expr(container, Prec::Or)?;
                self.push(", ");
                self.expr(contained, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            _ => {
                self.note_param(r, Ty::TEXT);
                self.push(match op {
                    "?|" => "jsonb_exists_any(",
                    "?&" => "jsonb_exists_all(",
                    _ => "jsonb_exists(",
                });
                self.expr(l, Prec::Or)?;
                self.push(", ");
                self.expr(r, Prec::Or)?;
                self.push(")");
                Ok(())
            }
        }
    }

    /// A boolean column that may hold text (col) compared with a constant truth value
    /// (other: true, false, or a string Postgres' boolin reads), and that value.
    fn mixed_test<'n>(&mut self, col: &'n NodeEnum, other: &NodeEnum) -> Option<(&'n NodeEnum, bool)> {
        let NodeEnum::ColumnRef(c) = col else { return None };
        if !self.typer.as_ref()?.any_mixed() || !self.typer.as_mut()?.column(&c.fields)?.mixed {
            return None;
        }
        let k = match other {
            NodeEnum::AConst(k) => k,
            NodeEnum::TypeCast(t) if types::of_type_name(t.type_name.as_ref()?).is(Base::Bool) => {
                match t.arg.as_deref()?.node.as_ref()? {
                    NodeEnum::AConst(k) => k,
                    _ => return None,
                }
            }
            _ => return None,
        };
        match k.val.as_ref()? {
            Val::Boolval(b) => Some((col, b.boolval)),
            Val::Sval(s) => overlite_core::parse_bool(&s.sval).map(|b| (col, b)),
            _ => None,
        }
    }

    /// hstore's operators (the column holds a JSON object of strings): `->` a value
    /// by its key (overlite_hstore_get, or of an array of keys an array of values),
    /// `?`, `?|` and `?&` the json functions over the keys, `@>` and `<@`
    /// json_contains; any other is Postgres' 42883.
    fn hstore_op(&mut self, a: &pb::AExpr, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        let (tl, tr) = (self.ty(l), self.ty(r));
        match op {
            "->" => {
                let keys = tr.array || matches!(r, NodeEnum::AArrayExpr(_));
                if !keys {
                    self.note_param(r, Ty::TEXT);
                }
                self.push("overlite_hstore_get(");
                self.expr(l, Prec::Or)?;
                self.push(", ");
                self.expr(r, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            "?" | "?|" | "?&" => {
                if op == "?" {
                    self.note_param(r, Ty::TEXT);
                }
                self.push(match op {
                    "?|" => "jsonb_exists_any(",
                    "?&" => "jsonb_exists_all(",
                    _ => "jsonb_exists(",
                });
                self.expr(l, Prec::Or)?;
                self.push(", ");
                self.expr(r, Prec::Or)?;
                self.push(")");
                Ok(())
            }
            "@>" | "<@" => {
                let (container, contained) = if op == "@>" { (l, r) } else { (r, l) };
                self.push("json_contains(");
                self.hstore_side(container)?;
                self.push(", ");
                self.hstore_side(contained)?;
                self.push(")");
                Ok(())
            }
            _ => Err(no_operator(op, tl, tr, a.location)),
        }
    }

    /// A side of an hstore containment: an untyped literal read as an hstore.
    fn hstore_side(&mut self, n: &NodeEnum) -> Out {
        if is_string(n) && self.ty(n).base == Base::Unknown {
            self.push("hstore_in(");
            self.expr(n, Prec::Or)?;
            self.push(")");
            return Ok(());
        }
        self.expr(n, Prec::Or)
    }

    /// A comparison, by its sides' types: two intervals by their spans, a date with a
    /// timestamp at its midnight, a timestamp with a timestamptz in the session's zone,
    /// an untyped literal or a parameter as the type of the other side.
    fn comparison(&mut self, op: &str, l: &NodeEnum, r: &NodeEnum) -> Out {
        let p = binary_prec(op).ok_or("operator")?;
        if matches!(op, "=" | "<>" | "!=")
            && let Some((col, truth)) = self.mixed_test(l, r).or_else(|| self.mixed_test(r, l))
        {
            // `b = <a truth value>` over a boolean that may hold text: b among every
            // spelling of it, which an index on b serves (resolve/booleans.rs).
            self.whole_item = true;
            let out = self.expr(col, Prec::Or);
            self.whole_item = false;
            out?;
            let forms = if truth { BOOL_TRUE_FORMS } else { BOOL_FALSE_FORMS };
            self.push(if op == "=" { " IN (" } else { " NOT IN (" });
            self.push(if truth { "1" } else { "0" });
            for f in forms {
                self.push(", ");
                self.push(&quote_literal(f));
            }
            self.push(")");
            return Ok(());
        }
        let (tl, tr) = (self.ty(l), self.ty(r));
        if self.is_range(l) || self.is_range(r) {
            return self.range_op(op, l, r, -1);
        }
        // Two arrays in Postgres' order, element by element, whichever form each is in.
        let array = |t: Ty| t.array || t.base == Base::Unknown;
        if (tl.array || tr.array) && array(tl) && array(tr) {
            self.note_param(l, tr);
            self.note_param(r, tl);
            self.push("overlite_array_cmp(");
            self.expr(l, Prec::Or)?;
            self.push(", ");
            self.expr(r, Prec::Or)?;
            self.push(") ");
            self.push(op);
            self.push(" 0");
            return Ok(());
        }
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
        // A date or a timestamp meeting a wider type is made one (`value`).
        self.value(l, tr, p)?;
        self.push(" ");
        self.push(op);
        self.push(" ");
        self.value(r, tl, next(p))
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
        // A float beside a numeric makes the numeric a float, as Postgres casts it.
        let decimal =
            tys.iter().any(|t| t.is_numeric()) && !tys.iter().any(|t| t.is(Base::Float4) || t.is(Base::Float8));
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
        if !self.list_srfs.is_empty() {
            let at = std::ptr::from_ref(f) as usize;
            if let Some((_, read)) = self.list_srfs.iter().find(|(p, _)| *p == at) {
                let read = read.clone();
                self.push(&read);
                return Ok(());
            }
        }
        if self.typed()
            && f.funcformat == pb::CoercionForm::CoerceSqlSyntax as i32
            && let [schema, name] = f.funcname.as_slice()
            && string(schema)? == "pg_catalog"
        {
            return self.sql_syntax(string(name)?, f);
        }
        let name = match f.funcname.as_slice() {
            [n] => string(n)?,
            // pg_catalog's functions are the ones a bare name calls.
            [schema, n] if string(schema)? == "pg_catalog" => string(n)?,
            _ => return Err("qualified function"),
        };
        // tstzrange(a, b): a bound that is not a timestamptz (a literal, a timestamp) is
        // one in the session's zone, as Postgres converts it.
        if self.typed() && name == "tstzrange" && (2..=3).contains(&f.args.len()) && f.over.is_none() {
            self.push("tstzrange(");
            for (i, a) in f.args.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                let a = node(a)?;
                if i < 2 {
                    self.value(a, Ty::of(Base::Timestamptz), Prec::Or)?;
                } else {
                    self.expr(a, Prec::Or)?;
                }
            }
            self.push(")");
            return Ok(());
        }
        // lower and upper of what the environment knows is a range: its bound, NULL for
        // the empty range, where the overloaded lower would read `empty` as text.
        if self.typed()
            && matches!(name, "lower" | "upper")
            && let [x] = f.args.as_slice()
            && f.over.is_none()
            && self.is_range(node(x)?)
        {
            self.push("overlite_range_bound(");
            self.expr(node(x)?, Prec::Or)?;
            self.push(if name == "lower" { ", 0)" } else { ", 1)" });
            return Ok(());
        }
        if self.ddl
            && f.args.is_empty()
            && matches!(
                name,
                "now" | "transaction_timestamp" | "statement_timestamp" | "clock_timestamp"
            )
        {
            // SQLite's clock, which runs without the session, to the millisecond:
            // overlite_default_expr reads it back as now().
            self.push("strftime('%Y-%m-%d %H:%M:%f', 'now')");
            return Ok(());
        }
        if self.typed() && f.args.is_empty() && f.over.is_none() && f.agg_filter.is_none() {
            // The clock, as rewrite_now writes it: the transaction's start, or the
            // statement's, or the clock as it is read.
            let clock = match name {
                "now" | "transaction_timestamp" => Some("overlite_now(julianday('now'))"),
                "statement_timestamp" => Some("overlite_now(julianday('now'), 'statement')"),
                "clock_timestamp" => Some("overlite_clock_timestamp()"),
                _ => None,
            };
            if let Some(c) = clock {
                self.push(c);
                return Ok(());
            }
        }
        if self.typed()
            && name == "date_part"
            && let [field, x] = f.args.as_slice()
            && f.over.is_none()
        {
            // date_part of a timestamptz reads its wall clock in the session's zone.
            let (field, x) = (node(field)?, node(x)?);
            self.push("date_part(");
            self.expr(field, Prec::Or)?;
            self.push(", ");
            if self.ty(x).is(Base::Timestamptz) {
                self.push("overlite_tz_wall(");
                self.expr(x, Prec::Or)?;
                self.push(")");
            } else {
                self.expr(x, Prec::Or)?;
            }
            self.push(")");
            return Ok(());
        }
        if f.agg_within_group || f.func_variadic {
            return Err("aggregate or window clause");
        }
        // TRIM([BOTH | LEADING | TRAILING] [chars] FROM s) is btrim, ltrim or rtrim in the
        // tree, SQLite's trim, ltrim and rtrim of the same arguments (rewrite_trim_from).
        if f.funcformat == pb::CoercionForm::CoerceSqlSyntax as i32
            && matches!(name, "btrim" | "ltrim" | "rtrim")
            && f.over.is_none()
            && f.agg_filter.is_none()
        {
            self.push(if name == "btrim" { "trim(" } else { name });
            if name != "btrim" {
                self.push("(");
            }
            self.list(&f.args, Self::expr_node)?;
            self.push(")");
            return Ok(());
        }
        if f.funcformat != pb::CoercionForm::CoerceExplicitCall as i32 {
            return Err("special function syntax");
        }
        if f.over.is_none() && f.agg_filter.is_none() && f.agg_order.is_empty() && !f.agg_star && !f.agg_distinct {
            match (name, f.args.as_slice()) {
                ("obj_description" | "shobj_description", [o, c]) => {
                    return self.description(node(o)?, None, Some(node(c)?));
                }
                ("obj_description", [o]) => return self.description(node(o)?, None, None),
                ("pg_typeof", [x]) => return self.pg_typeof(node(x)?),
                ("pg_get_acl", [c, o, n]) => return self.object_acl(node(c)?, node(o)?, node(n)?),
                ("col_description", [o, n]) => return self.description(node(o)?, Some(node(n)?), None),
                ("pg_get_indexdef", [o, ..]) => return self.indexdef(node(o)?),
                ("pg_get_constraintdef", [o, ..]) => return self.constraintdef(node(o)?),
                ("pg_get_viewdef", [o, ..]) => return self.viewdef(node(o)?),
                ("pg_partition_ancestors" | "pg_partition_tree" | "pg_partition_root", _) => {
                    self.push("(SELECT NULL AS ");
                    self.push(name);
                    self.push(" WHERE 0)");
                    return Ok(());
                }
                // Postgres' names of a JSON value's type, from SQLite's json_type
                // (the text pass wrote json_type alone: `integer` for a number).
                ("json_typeof" | "jsonb_typeof", [x]) => {
                    self.push("CASE json_type(");
                    self.expr(node(x)?, Prec::Or)?;
                    self.push(
                        ") WHEN 'integer' THEN 'number' WHEN 'real' THEN 'number' WHEN 'text' THEN 'string' \
                         WHEN 'true' THEN 'boolean' WHEN 'false' THEN 'boolean' ELSE json_type(",
                    );
                    self.expr(node(x)?, Prec::Or)?;
                    self.push(") END");
                    return Ok(());
                }
                ("pg_options_to_table", _) => {
                    self.push(EMPTY_OPTIONS);
                    return Ok(());
                }
                ("array_to_string", [a, sep, ..]) => {
                    if let NodeEnum::SubLink(l) = node(a)?
                        && l.sub_link_type == SubLinkType::ArraySublink as i32
                    {
                        return self.joined_subquery(l, node(sep)?);
                    }
                }
                _ => {}
            }
        }
        if REWRITTEN.contains(&name)
            && !JSON_RENAMES.iter().any(|(n, _, _)| *n == name)
            && !matches!(name, "string_agg" | "array_to_string")
            && !CATALOG_CALLS.contains(&name)
        {
            return Err("function");
        }
        let clauses = f.over.is_some() || f.agg_filter.is_some() || !f.agg_order.is_empty();
        let args: Vec<&NodeEnum> = f.args.iter().map(node).collect::<Result<_, _>>()?;
        if self.typed() && !clauses {
            match (name, args.as_slice()) {
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
        // bool_and and bool_or are MIN and MAX over the truth values, read as Postgres'
        // boolin reads them.
        if self.typed()
            && let ("bool_and" | "bool_or", [x]) = (name, args.as_slice())
            && !f.agg_distinct
        {
            self.push(if name == "bool_or" {
                "pg_bool(max(overlite_bool("
            } else {
                "pg_bool(min(overlite_bool("
            });
            self.expr(x, Prec::Or)?;
            self.push("))");
            self.call_clauses(f)?;
            self.push(")");
            return Ok(());
        }
        // avg of integers is a numeric, Postgres' sum divided by the count at its
        // scale, where SQLite's is a double; NULL over no rows or only NULLs, where
        // the count is 0.
        if name == "avg"
            && self.typed()
            && let [x] = args.as_slice()
            && self.ty(x).is_integer()
            && f.agg_order.is_empty()
        {
            let distinct = if f.agg_distinct { "DISTINCT " } else { "" };
            self.push("overlite_cast(overlite_numeric_op(sum(");
            self.push(distinct);
            self.expr(x, Prec::Or)?;
            self.push(")");
            self.call_clauses(f)?;
            self.push(", nullif(count(");
            self.push(distinct);
            self.expr(x, Prec::Or)?;
            self.push(")");
            self.call_clauses(f)?;
            self.push(", 0), '/'), 'numeric')");
            return Ok(());
        }
        // array_agg is the JSON array an array is stored as (NULL over no rows, as
        // Postgres' is), where the engine's aggregate wrote Postgres' text.
        if name == "array_agg" && self.typed() {
            let [x] = args.as_slice() else {
                return Err("array_agg");
            };
            let elem = self.ty(x);
            if elem.array {
                return Err("array_agg of arrays");
            }
            self.push("overlite_array(nullif(json_group_array(");
            if f.agg_distinct {
                self.push("DISTINCT ");
            }
            self.json_element(x, elem)?;
            if !f.agg_order.is_empty() {
                self.push(" ORDER BY ");
                self.sort_keys(&f.agg_order)?;
            }
            self.push(")");
            self.call_clauses(f)?;
            self.push(", '[]'))");
            return Ok(());
        }
        // The json builders and aggregates by SQLite's names, those of jsonb in its
        // collation, as rewrite_json_funcs writes them.
        let (call, jsonb) = match JSON_RENAMES.iter().find(|(n, _, _)| *n == name) {
            Some((_, to, jsonb)) => (*to, *jsonb),
            None if name == "string_agg" && f.agg_distinct => ("overlite_string_agg_distinct", false),
            None if name == "string_agg" => ("group_concat", false),
            None => (name, false),
        };
        // A json or jsonb value built into one is JSON, not a string of its text; json
        // and jsonb aggregates over no rows are NULL, where SQLite's are `[]` and `{}`.
        let renamed = self.typed() && name != call;
        let builds = renamed
            && matches!(
                call,
                "json_object" | "json_array" | "json_group_array" | "json_group_object"
            );
        let empty = match call {
            "json_group_array" if renamed => Some("'[]'"),
            "json_group_object" if renamed => Some("'{}'"),
            _ => None,
        };
        let json_typed = renamed
            && matches!(
                name,
                "json_build_object" | "json_build_array" | "json_object_agg" | "json_agg"
            );
        if jsonb {
            self.push("(overlite_cast(");
        } else if json_typed {
            self.push("overlite_cast(");
        }
        if empty.is_some() {
            self.push("nullif(");
        }
        if call.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric()) {
            self.push(call);
        } else {
            self.push(&quote_ident(call));
        }
        self.push("(");
        if f.agg_star {
            self.push("*");
        } else {
            if f.agg_distinct && call != "overlite_string_agg_distinct" {
                self.push("DISTINCT ");
            }
            if builds {
                for (i, a) in f.args.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.json_value(node(a)?)?;
                }
            } else {
                self.list(&f.args, Self::expr_node)?;
            }
            if !f.agg_order.is_empty() {
                self.push(" ORDER BY ");
                self.sort_keys(&f.agg_order)?;
            }
        }
        self.push(")");
        self.call_clauses(f)?;
        if let Some(e) = empty {
            self.push(", ");
            self.push(e);
            self.push(")");
        }
        if jsonb {
            self.push(", 'jsonb') COLLATE JSONB)");
        } else if json_typed {
            self.push(", 'json')");
        }
        Ok(())
    }

    /// x as SQLite's json functions take a value into what they build: a json or jsonb
    /// as JSON (json()), a boolean as true or false, anything else as it is.
    fn json_value(&mut self, x: &NodeEnum) -> Out {
        let ty = self.ty(x);
        if !ty.array && matches!(ty.base, Base::Json | Base::Jsonb) {
            self.push("json(");
            self.expr(x, Prec::Or)?;
            self.push(")");
            return Ok(());
        }
        self.json_element(x, ty)
    }

    /// obj_description, shobj_description and col_description: the comment
    /// pg_description holds for the object (and column), as rewrite_obj_description
    /// writes them.
    /// A catalog named, the comment of the object of that catalog (the deprecated
    /// form without one takes any).
    fn description(&mut self, oid: &NodeEnum, column: Option<&NodeEnum>, catalog: Option<&NodeEnum>) -> Out {
        self.push("(SELECT description FROM pg_description WHERE objoid = ");
        self.expr(oid, Prec::Compare)?;
        self.push(" AND objsubid = ");
        match column {
            Some(c) => self.expr(c, Prec::Compare)?,
            None => self.push("0"),
        }
        const CATALOGS: [(&str, u32); 8] = [
            ("pg_class", 1259),
            ("pg_proc", 1255),
            ("pg_namespace", 2615),
            ("pg_type", 1247),
            ("pg_constraint", 2606),
            ("pg_trigger", 2620),
            ("pg_database", 1262),
            ("pg_authid", 1260),
        ];
        let named = match catalog {
            Some(NodeEnum::AConst(k)) => match &k.val {
                Some(Val::Sval(s)) => Some(CATALOGS.iter().find(|(n, _)| *n == s.sval).map_or(0, |(_, o)| *o)),
                _ => None,
            },
            _ => None,
        };
        if let Some(oid) = named {
            self.push(" AND classoid = ");
            self.push(&oid.to_string());
        } else if let Some(c) = catalog {
            self.push(" AND classoid = CASE ");
            self.expr(c, Prec::Compare)?;
            for (name, oid) in CATALOGS {
                self.push(&format!(" WHEN '{name}' THEN {oid}"));
            }
            self.push(" ELSE 0 END");
        } else if column.is_some() {
            self.push(" AND classoid = 1259");
        }
        self.push(" LIMIT 1)");
        Ok(())
    }

    /// pg_typeof(x): the type the statement's typing gives x, as regtype prints it;
    /// where the typing does not know it, the type of the value SQLite holds.
    fn pg_typeof(&mut self, x: &NodeEnum) -> Out {
        if let Some(name) = self.ty(x).pg_name() {
            // x stays in the statement, which an aggregate in it makes one row of.
            let name = quote_literal(&name);
            self.push("CASE WHEN (");
            self.expr(x, Prec::Or)?;
            self.push(&[") IS NULL THEN ", &name, " ELSE ", &name, " END"].concat());
            return Ok(());
        }
        self.push("CASE typeof(");
        self.expr(x, Prec::Or)?;
        self.push(
            ") WHEN 'integer' THEN 'bigint' WHEN 'real' THEN 'double precision' WHEN 'blob' THEN 'bytea' \
             WHEN 'null' THEN 'unknown' ELSE 'text' END",
        );
        Ok(())
    }

    /// pg_get_acl(classid, objid, objsubid): the privileges of a table (or a column of
    /// it) or a schema, as their catalogs keep them; NULL for another object.
    fn object_acl(&mut self, class: &NodeEnum, oid: &NodeEnum, sub: &NodeEnum) -> Out {
        self.push(
            "(SELECT CASE WHEN _c = 1259 AND _s = 0 THEN (SELECT relacl FROM pg_class WHERE oid = _o) \
             WHEN _c = 1259 THEN (SELECT attacl FROM pg_attribute WHERE attrelid = _o AND attnum = _s) \
             WHEN _c = 2615 THEN (SELECT nspacl FROM pg_namespace WHERE oid = _o) END FROM (SELECT ",
        );
        self.expr(class, Prec::Or)?;
        self.push(" AS _c, ");
        self.expr(oid, Prec::Or)?;
        self.push(" AS _o, ");
        self.expr(sub, Prec::Or)?;
        self.push(" AS _s))");
        Ok(())
    }

    /// pg_get_indexdef: the CREATE INDEX the catalog's columns say, with the USING
    /// psql looks for, as rewrite_object_defs writes it.
    fn indexdef(&mut self, oid: &NodeEnum) -> Out {
        self.push(
            "(SELECT 'CREATE ' || CASE WHEN ix._u THEN 'UNIQUE ' ELSE '' END || 'INDEX ' || (SELECT relname FROM \
             pg_class WHERE oid = ix._i) || ' ON ' || (SELECT n.nspname || '.' || c.relname FROM pg_class c JOIN \
             pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = ix._r) || ' USING btree (' || ix._c || ')' \
             || COALESCE(' WHERE ' || ix._w, '') \
             FROM (SELECT indexrelid AS _i, indrelid AS _r, indisunique AS _u, ov_cols AS _c, ov_pred AS _w FROM pg_index) ix \
             WHERE ix._i = ",
        );
        self.expr(oid, Prec::Compare)?;
        self.push(")");
        Ok(())
    }

    /// pg_get_viewdef: the view's query, as pg_views (or pg_matviews) has it, found by
    /// the view's oid.
    fn viewdef(&mut self, oid: &NodeEnum) -> Out {
        self.push(
            "(SELECT v.definition FROM (SELECT schemaname, viewname, definition, 'v' AS kind FROM pg_views \
             UNION ALL SELECT schemaname, matviewname, definition, 'm' FROM pg_matviews) v \
             JOIN pg_class c ON c.relname = v.viewname AND c.relkind = v.kind \
             JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = v.schemaname WHERE c.oid = ",
        );
        self.expr(oid, Prec::Compare)?;
        self.push(")");
        Ok(())
    }

    /// pg_get_constraintdef: the constraint as its kind and the catalog's columns say,
    /// as rewrite_object_defs writes it.
    fn constraintdef(&mut self, oid: &NodeEnum) -> Out {
        self.push(
            "(SELECT CASE _t WHEN 'f' THEN 'FOREIGN KEY (' || _c || ') REFERENCES ' || _r WHEN 'p' THEN \
             'PRIMARY KEY (' || _c || ')' WHEN 'u' THEN 'UNIQUE (' || _c || ')' WHEN 'c' THEN 'CHECK (' || _c || \
             ')' WHEN 'x' THEN _c ELSE '' END FROM (SELECT oid AS _o, contype AS _t, ov_cols AS _c, ov_ref AS _r \
             FROM pg_constraint) WHERE _o = ",
        );
        self.expr(oid, Prec::Compare)?;
        self.push(")");
        Ok(())
    }

    /// array_to_string(ARRAY(subquery), sep): the rows joined, in their order, as
    /// rewrite_array_to_string_subquery writes it (psql's list of an enum's labels).
    fn joined_subquery(&mut self, l: &pb::SubLink, sep: &NodeEnum) -> Out {
        let NodeEnum::SelectStmt(q) = boxed(l.subselect.as_deref())? else {
            return Err("subquery");
        };
        if q.op != SetOperation::SetopNone as i32 || q.target_list.len() != 1 || !q.values_lists.is_empty() {
            return Err("ARRAY of a subquery of more than one column");
        }
        let mut q = q.as_ref().clone();
        if let Some(NodeEnum::ResTarget(t)) = q.target_list[0].node.as_mut() {
            t.name = "_v".to_string();
        }
        self.push("(SELECT group_concat(_v, ");
        self.expr(sep, Prec::Or)?;
        self.push(") FROM (");
        self.select(&q, false)?;
        self.push("))");
        Ok(())
    }

    /// An aggregate's FILTER and a window function's OVER, after its call.
    fn call_clauses(&mut self, f: &pb::FuncCall) -> Out {
        if let Some(w) = &f.agg_filter {
            self.push(" FILTER (WHERE ");
            self.expr(node(w)?, Prec::Or)?;
            self.push(")");
        }
        if let Some(w) = &f.over {
            self.push(" OVER ");
            let w = w.as_ref();
            let bare = w.partition_clause.is_empty() && w.order_clause.is_empty() && w.frame_options & 0x1 == 0;
            if !w.name.is_empty() && bare && w.refname.is_empty() {
                self.push(&ident(&w.name));
            } else {
                self.window_spec(w)?;
            }
        }
        Ok(())
    }

    /// x as an element of a JSON array: a boolean as JSON's true or false, where SQLite
    /// keeps one as 1 or 0.
    fn json_element(&mut self, x: &NodeEnum, ty: Ty) -> Out {
        if ty.is(Base::Bool) {
            self.push("CASE WHEN ");
            self.expr(x, Prec::Or)?;
            self.push(" IS NULL THEN NULL WHEN ");
            self.expr(x, Prec::Or)?;
            self.push(" THEN json('true') ELSE json('false') END");
            return Ok(());
        }
        self.expr(x, Prec::Or)
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
        if from.base != Base::Timestamptz && to_ty.base == Base::Timestamptz {
            self.push(name);
            self.push("(");
            self.as_timestamptz(x)?;
            self.push(", ");
            self.expr(target, Prec::Or)?;
            self.push(")");
            return Ok(());
        }
        let (pre, post) = if !from.is(Base::Timestamptz) {
            ("", "")
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
                    // A date, as Postgres takes it: the timestamptz of its midnight in
                    // the session's zone (timezone(text, timestamptz) over the cast).
                    Base::Date => "overlite_instant_at_time_zone(overlite_tz_in(",
                    _ => return Err("AT TIME ZONE of a time"),
                };
                self.note_param(zone, Ty::TEXT);
                self.push(func);
                if t.base == Base::Date {
                    self.expr(x, Prec::Or)?;
                    self.push(")");
                } else if t.base == Base::Unknown {
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
            ("btrim" | "ltrim" | "rtrim", _) => {
                self.push(if name == "btrim" { "trim" } else { name });
                self.push("(");
                self.list(&f.args, Self::expr_node)?;
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
            // `x = ANY (subquery)` is IN, `x <> ALL (subquery)` NOT IN.
            SubLinkType::AnySublink if sublink_op(s) == Some("=") => self.in_subquery(s, false),
            SubLinkType::AllSublink if matches!(sublink_op(s), Some("<>" | "!=")) => self.in_subquery(s, true),
            // ARRAY(subquery): the JSON array of its rows, in their order, as
            // rewrite_array_subquery writes it.
            SubLinkType::ArraySublink => {
                let NodeEnum::SelectStmt(q) = boxed(s.subselect.as_deref())? else {
                    return Err("subquery");
                };
                if q.op != SetOperation::SetopNone as i32 || q.target_list.len() != 1 || !q.values_lists.is_empty() {
                    return Err("ARRAY of a subquery of more than one column");
                }
                let mut q = q.as_ref().clone();
                if let Some(NodeEnum::ResTarget(t)) = q.target_list[0].node.as_mut() {
                    t.name = "_a".to_string();
                }
                self.push("overlite_array((SELECT json_group_array(_a) FROM (");
                self.select(&q, false)?;
                self.push(")))");
                Ok(())
            }
            _ => Err("sublink kind"),
        }
    }

    fn in_subquery(&mut self, s: &pb::SubLink, not: bool) -> Out {
        let test = boxed(s.testexpr.as_deref())?;
        // A row is SQLite's row value, which IN a subquery of as many columns takes.
        self.row_or_expr(test)?;
        self.push(if not { " NOT IN " } else { " IN " });
        self.subquery(s)
    }

    /// A row as SQLite's row value, `(a, b)`, or an expression as one compares.
    fn row_or_expr(&mut self, n: &NodeEnum) -> Out {
        let NodeEnum::RowExpr(row) = n else {
            return self.expr(n, Prec::Compare);
        };
        self.push("(");
        self.list(&row.args, Self::expr_node)?;
        self.push(")");
        Ok(())
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

/// The element of array a at the 1-based subscripts idx, as array_ref reads it: NULL
/// for a subscript out of its bounds, and NULL where fewer subscripts than the array
/// has dimensions leave an array (unless its elements are json, which may be arrays).
fn element(a: &str, idx: &[String], json_elements: bool) -> String {
    let mut path = String::from("'$'");
    let mut bounds = Vec::new();
    for i in idx {
        path.push_str(" || '[' || ((");
        path.push_str(i);
        path.push_str(")-1) || ']'");
        bounds.push(["(", i, ") >= 1"].concat());
    }
    let extract = ["json_extract(", a, ", ", &path, ")"].concat();
    let value = if json_elements {
        extract
    } else {
        [
            "CASE WHEN json_type(",
            a,
            ", ",
            &path,
            ") <> 'array' THEN ",
            &extract,
            " END",
        ]
        .concat()
    };
    [
        "CASE WHEN ",
        &bounds.join(" AND "),
        " THEN CASE WHEN json_valid(",
        a,
        ") THEN ",
        &value,
        " END END",
    ]
    .concat()
}

/// The operator of `x op ANY (subquery)`.
fn sublink_op(s: &pb::SubLink) -> Option<&str> {
    match s.oper_name.as_slice() {
        [n] => string(n).ok(),
        _ => None,
    }
}

/// What a RETURNING's OLD and NEW are written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteKind {
    Insert,
    Update,
    Delete,
}

/// The OLD and NEW of a RETURNING: their names, the write, its table and the table's
/// columns (for `old.*` and `new.*`).
#[derive(Debug, Clone)]
struct OldNew {
    old: String,
    new: String,
    kind: WriteKind,
    table: String,
    columns: Vec<String>,
}

/// A set-returning function of a FROM, as the engine walks it (`range_function`).
pub(crate) struct Srf<'a> {
    pub name: &'static str,
    pub args: Vec<&'a NodeEnum>,
    /// It has keys: overlite_pairs walks it.
    pairs: bool,
    /// It reads a column, of a table to its left: written in place.
    lateral: bool,
    /// Its alias, the statement's or its name.
    pub alias: String,
    /// Whether the statement gave the alias.
    aliased: bool,
    /// Its columns as the statement names them, the ordinality last.
    pub names: Vec<String>,
    /// The same columns as the walker's row holds them: `value`, `key`, `ord`, or a
    /// zipped array's element (`json_extract(value, '$[1]')`).
    columns: Vec<String>,
}

impl Srf<'_> {
    /// A column as read from the walker under alias, for one written in place.
    fn column_of(&self, i: usize, alias: &str) -> String {
        let c = &self.columns[i];
        match c.strip_prefix("json_extract(value") {
            Some(rest) => ["json_extract(", &ident(alias), ".value", rest].concat(),
            None => [ident(alias).as_str(), ".", c].concat(),
        }
    }
}

/// The set-returning functions the engine walks, as rewrite/srf.rs knows them.
const SRFS: &[&str] = &[
    "unnest",
    "generate_subscripts",
    "generate_series",
    "json_each_text",
    "jsonb_each_text",
    "json_each",
    "jsonb_each",
    "json_array_elements_text",
    "jsonb_array_elements_text",
    "json_array_elements",
    "jsonb_array_elements",
    "json_object_keys",
    "jsonb_object_keys",
    "regexp_split_to_table",
];

/// The set-returning function a FROM item calls, None for another function.
pub(crate) fn srf_from(r: &pb::RangeFunction) -> Result<Option<Srf<'_>>, Unsupported> {
    let [item] = r.functions.as_slice() else {
        return Ok(None);
    };
    let NodeEnum::List(l) = node(item)? else {
        return Err("FROM function");
    };
    let NodeEnum::FuncCall(f) = node(l.items.first().ok_or("FROM function")?)? else {
        return Ok(None);
    };
    let fname = match f.funcname.as_slice() {
        [n] => string(n)?,
        [schema, n] if string(schema)? == "pg_catalog" => string(n)?,
        _ => return Ok(None),
    };
    let Some(name) = SRFS.iter().find(|n| **n == fname).copied() else {
        return Ok(None);
    };
    if !r.coldeflist.is_empty() || r.is_rowsfrom || f.agg_star || f.agg_distinct || f.over.is_some() {
        return Err("set-returning function's clauses");
    }
    let args: Vec<&NodeEnum> = f.args.iter().map(node).collect::<Result<_, _>>()?;
    if args.is_empty() {
        return Err("set-returning function of nothing");
    }
    let width = if name == "unnest" { args.len() } else { 1 };
    let pairs = name.contains("_each");
    // generate_series is a recursive CTE in a subquery, which reads a column of an
    // outer query but none to its left, as rewrite_generate_series writes it.
    let lateral = name != "generate_series" && args.iter().any(|a| reads_column(a));
    let (alias, aliased) = match &r.alias {
        Some(a) => (a.aliasname.clone(), true),
        None => (name.to_string(), false),
    };
    let colnames: Vec<String> = r
        .alias
        .iter()
        .flat_map(|a| a.colnames.iter())
        .map(|c| string(c).map(str::to_string))
        .collect::<Result<_, _>>()?;
    let (mut names, mut columns): (Vec<String>, Vec<String>) = if pairs {
        (vec!["key".into(), "value".into()], vec!["key".into(), "value".into()])
    } else if name.contains("array_elements") {
        (vec!["value".into()], vec!["value".into()])
    } else if width > 1 {
        (
            vec![name.to_string(); width],
            (0..width).map(|i| format!("json_extract(value, '$[{i}]')")).collect(),
        )
    } else {
        (vec![alias.clone()], vec!["value".into()])
    };
    if r.ordinality {
        names.push("ordinality".into());
        columns.push("ord".into());
    }
    if colnames.len() > names.len() {
        return Err("set-returning function's column list");
    }
    for (n, c) in names.iter_mut().zip(colnames) {
        *n = c;
    }
    Ok(Some(Srf {
        name,
        args,
        pairs,
        lateral,
        alias,
        aliased,
        names,
        columns,
    }))
}

/// The set-returning functions an expression of a select list calls, outside its
/// subqueries, in the order written.
fn list_srf_calls<'a>(n: &'a NodeEnum, out: &mut Vec<&'a pb::FuncCall>) {
    let mut each = |c: Option<&'a pb::Node>| {
        if let Some(c) = c.and_then(|c| c.node.as_ref()) {
            list_srf_calls(c, out);
        }
    };
    match n {
        NodeEnum::FuncCall(f) => {
            if func_name(f).is_some_and(|name| SRFS.contains(&name)) && f.over.is_none() {
                out.push(f);
                return;
            }
            for a in &f.args {
                each(Some(a));
            }
        }
        NodeEnum::AExpr(a) => {
            each(a.lexpr.as_deref());
            each(a.rexpr.as_deref());
        }
        NodeEnum::TypeCast(t) => each(t.arg.as_deref()),
        NodeEnum::CoalesceExpr(c) => c.args.iter().for_each(|a| each(Some(a))),
        NodeEnum::CaseExpr(c) => {
            each(c.arg.as_deref());
            for w in &c.args {
                if let Some(NodeEnum::CaseWhen(w)) = w.node.as_ref() {
                    each(w.expr.as_deref());
                    each(w.result.as_deref());
                }
            }
            each(c.defresult.as_deref());
        }
        _ => {}
    }
}

/// A set-returning function called in a select list, as `srf_source` builds its rows.
fn srf_of_call(f: &pb::FuncCall) -> Result<Srf<'_>, Unsupported> {
    let name = func_name(f)
        .and_then(|n| SRFS.iter().find(|s| **s == n).copied())
        .ok_or("function")?;
    if f.agg_star || f.agg_distinct || f.agg_filter.is_some() || !f.agg_order.is_empty() {
        return Err("set-returning function's clauses");
    }
    let args: Vec<&NodeEnum> = f.args.iter().map(node).collect::<Result<_, _>>()?;
    if args.is_empty() {
        return Err("set-returning function of nothing");
    }
    let mut inner = Vec::new();
    for a in &args {
        list_srf_calls(a, &mut inner);
    }
    if !inner.is_empty() {
        return Err("set-returning function of a set");
    }
    Ok(Srf {
        name,
        args,
        pairs: name.contains("_each"),
        lateral: true,
        alias: String::new(),
        aliased: false,
        names: Vec::new(),
        columns: Vec::new(),
    })
}

/// A write's RETURNING with its FROM or USING: an item reading a joined relation is
/// a subquery over the join for the row written, as rewrite_update_from and
/// rewrite_delete_using write it (SQLite's RETURNING sees the written table alone),
/// under the name Postgres gives the item. The rest are as they came.
fn joined_returning(
    ret: Option<&pb::ReturningClause>,
    target: &pb::RangeVar,
    joined: &[pb::Node],
    filter: Option<&pb::Node>,
) -> Option<pb::ReturningClause> {
    let ret = ret?;
    if joined.is_empty() {
        return Some(ret.clone());
    }
    let mut names: Vec<String> = Vec::new();
    for j in joined {
        for (n, ..) in j.node.as_ref().map(NodeEnum::nodes).unwrap_or_default() {
            match n {
                pg_query::NodeRef::RangeVar(r) => {
                    names.push(
                        r.alias
                            .as_ref()
                            .map_or_else(|| r.relname.clone(), |a| a.aliasname.clone()),
                    );
                }
                pg_query::NodeRef::RangeSubselect(pb::RangeSubselect { alias: Some(a), .. })
                | pg_query::NodeRef::RangeFunction(pb::RangeFunction { alias: Some(a), .. }) => {
                    names.push(a.aliasname.clone());
                }
                _ => {}
            }
        }
    }
    let target_name = target.alias.as_ref().map_or(&target.relname, |a| &a.aliasname);
    names.retain(|n| n != target_name);
    let reads_joined = |v: &NodeEnum| {
        v.nodes().into_iter().any(|(n, ..)| {
                matches!(n, pg_query::NodeRef::ColumnRef(c)
                    if c.fields.len() > 1 && c.fields.first().and_then(|f| string(f).ok()).is_some_and(|q| names.iter().any(|n| n == q)))
            })
    };
    let mut out = ret.clone();
    for item in &mut out.exprs {
        let Some(NodeEnum::ResTarget(t)) = item.node.as_mut() else {
            continue;
        };
        let Some(v) = t.val.as_deref().and_then(|v| v.node.as_ref()) else {
            continue;
        };
        if !reads_joined(v) {
            continue;
        }
        if t.name.is_empty() {
            t.name = match figure_colname(v) {
                Label::Named(l) => l,
                Label::Itself => match v {
                    NodeEnum::ColumnRef(c) => c
                        .fields
                        .last()
                        .and_then(|f| string(f).ok())
                        .unwrap_or("?column?")
                        .to_string(),
                    _ => "?column?".to_string(),
                },
            };
        }
        let one = pb::Node {
            node: Some(NodeEnum::AConst(pb::AConst {
                isnull: false,
                val: Some(Val::Ival(pb::Integer { ival: 1 })),
                location: 0,
            })),
        };
        let sub = pb::SelectStmt {
            target_list: vec![pb::Node {
                node: Some(NodeEnum::ResTarget(Box::new(pb::ResTarget {
                    val: t.val.clone(),
                    ..Default::default()
                }))),
            }],
            from_clause: joined.to_vec(),
            where_clause: filter.map(|f| Box::new(f.clone())),
            limit_count: Some(Box::new(one)),
            limit_option: LimitOption::Count as i32,
            op: SetOperation::SetopNone as i32,
            ..Default::default()
        };
        t.val = Some(Box::new(pb::Node {
            node: Some(NodeEnum::SubLink(Box::new(pb::SubLink {
                sub_link_type: SubLinkType::ExprSublink as i32,
                subselect: Some(Box::new(pb::Node {
                    node: Some(NodeEnum::SelectStmt(Box::new(sub))),
                })),
                ..Default::default()
            }))),
        }));
    }
    Some(out)
}

/// Whether a select list has an output named ctid.
fn names_ctid(items: &[pb::Node]) -> bool {
    items.iter().any(|t| match t.node.as_ref() {
        Some(NodeEnum::ResTarget(t)) if t.name == "ctid" => true,
        Some(NodeEnum::ResTarget(t)) if t.name.is_empty() => matches!(
            t.val.as_deref().and_then(|v| v.node.as_ref()),
            Some(NodeEnum::ColumnRef(c)) if c.fields.last().and_then(|f| string(f).ok()) == Some("ctid")
        ),
        _ => false,
    })
}

/// Whether an expression reads a column.
fn reads_column(n: &NodeEnum) -> bool {
    format!("{n:?}").contains("ColumnRef(")
}

/// The names of a query's output columns, as Postgres names them; a star is not known.
fn output_names(q: &pb::SelectStmt) -> Result<Vec<String>, Unsupported> {
    if q.op != SetOperation::SetopNone as i32 {
        return output_names(q.larg.as_deref().ok_or("set operation arms")?);
    }
    if let Some(row) = q.values_lists.first() {
        let NodeEnum::List(row) = node(row)? else {
            return Err("VALUES row");
        };
        return Ok((1..=row.items.len()).map(|k| format!("column{k}")).collect());
    }
    q.target_list
        .iter()
        .map(|t| {
            let NodeEnum::ResTarget(t) = node(t)? else {
                return Err("target");
            };
            if !t.name.is_empty() {
                return Ok(t.name.clone());
            }
            match boxed(t.val.as_deref())? {
                NodeEnum::ColumnRef(c) => match node(c.fields.last().ok_or("column")?)? {
                    NodeEnum::String(s) => Ok(s.sval.clone()),
                    _ => Err("star in a derived column list"),
                },
                v => match figure_colname(v) {
                    Label::Named(l) => Ok(l),
                    Label::Itself => Err("target"),
                },
            }
        })
        .collect()
}

/// The name of a range type.
fn range_name(b: Base) -> Option<&'static str> {
    Some(match b {
        Base::Int4Range => "int4range",
        Base::Int8Range => "int8range",
        Base::NumRange => "numrange",
        Base::DateRange => "daterange",
        Base::TsRange => "tsrange",
        Base::TstzRange => "tstzrange",
        _ => return None,
    })
}

/// A function's name, unqualified or in pg_catalog.
fn func_name(f: &pb::FuncCall) -> Option<&str> {
    match f.funcname.as_slice() {
        [n] => string(n).ok(),
        [schema, n] if string(schema).ok() == Some("pg_catalog") => string(n).ok(),
        _ => None,
    }
}

/// An operator's name.
fn op_name(a: &pb::AExpr) -> Option<&str> {
    match a.name.as_slice() {
        [n] => string(n).ok(),
        [schema, n] if string(schema).ok() == Some("pg_catalog") => string(n).ok(),
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
        "&" | "|" | "<<" | ">>" => Prec::Bit,
        "+" | "-" => Prec::Add,
        // `%` is a remainder only where the environment says so: on text it is pg_trgm's
        // similarity, a call (`typed_op`), and without the environment it is refused.
        "*" | "/" | "%" => Prec::Mul,
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
        Prec::Compare => Prec::Bit,
        Prec::Bit => Prec::Add,
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
                Ok(K::AexprOp) => match (&a.lexpr, op_name(a)) {
                    (None, _) => Prec::Unary,
                    (Some(_), Some("~" | "~*")) => Prec::Equality,
                    (Some(_), Some("!~" | "!~*")) => Prec::Not,
                    // SQLite binds -> and ->> as tightly as ||.
                    (Some(_), Some("->" | "->>")) => Prec::Concat,
                    (Some(_), Some(op)) => binary_prec(op).unwrap_or(Prec::Atom),
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
        "float" => "double precision",
        "regconfig" | "tsquery" | "tsvector" => b,
        // The reg* types that name an object are looked up (`Emitter::reg_cast`); the
        // others, oid and another name are the engine's to read or refuse (an enum, a
        // type it does not know), as the text passes leave them.
        b if b.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric()) => b,
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

/// Whether n is a string literal written as a range is (`'[1,5)'`, `'empty'`).
fn is_range_literal(n: &NodeEnum) -> bool {
    matches!(n, NodeEnum::AConst(k) if matches!(&k.val, Some(Val::Sval(v))
        if v.sval.starts_with(['[', '(']) && v.sval.ends_with([']', ')']) && v.sval.contains(',')
            || v.sval.eq_ignore_ascii_case("empty")))
}

/// The column and the catalog view a reg* type names its objects by.
fn reg_lookup(ty: &str) -> Option<(&'static str, &'static str)> {
    Some(match ty {
        "regclass" => ("relname", "pg_class"),
        "regnamespace" => ("nspname", "pg_namespace"),
        "regtype" => ("typname", "pg_type"),
        "regrole" => ("rolname", "pg_roles"),
        "regproc" | "regprocedure" => ("proname", "pg_proc"),
        _ => return None,
    })
}

/// The oid of a system catalog by its name, which pg_depend and pg_description go by as
/// classid (`'pg_class'::regclass`): pg_class lists no catalog of its own.
fn system_catalog_oid(name: &str) -> Option<&'static str> {
    let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
    Some(match name {
        "pg_class" => "1259",
        "pg_type" => "1247",
        "pg_attribute" => "1249",
        "pg_proc" => "1255",
        "pg_namespace" => "2615",
        "pg_constraint" => "2606",
        "pg_index" => "2610",
        "pg_attrdef" => "2604",
        "pg_trigger" => "2620",
        "pg_rewrite" => "2618",
        "pg_description" => "2609",
        "pg_policy" => "3256",
        "pg_database" => "1262",
        "pg_authid" => "1260",
        "pg_extension" => "3079",
        "pg_opclass" => "2616",
        "pg_opfamily" => "2753",
        "pg_am" => "2601",
        "pg_operator" => "2617",
        "pg_language" => "2612",
        "pg_cast" => "2605",
        "pg_collation" => "3456",
        "pg_conversion" => "2607",
        "pg_aggregate" => "2600",
        "pg_tablespace" => "1213",
        "pg_event_trigger" => "3466",
        "pg_foreign_data_wrapper" => "2328",
        "pg_foreign_server" => "1417",
        "pg_default_acl" => "826",
        "pg_ts_config" => "3602",
        "pg_ts_dict" => "3600",
        "pg_ts_parser" => "3601",
        "pg_ts_template" => "3764",
        "pg_statistic_ext" => "3381",
        "pg_publication" => "6104",
        "pg_subscription" => "6100",
        "pg_transform" => "3576",
        "pg_largeobject" => "2613",
        "pg_largeobject_metadata" => "2995",
        "pg_sequence" => "2224",
        "pg_depend" => "2608",
        "pg_shdepend" => "1214",
        "pg_shdescription" => "2396",
        "pg_enum" => "3501",
        "pg_range" => "3541",
        "pg_inherits" => "2611",
        "pg_auth_members" => "1261",
        "pg_db_role_setting" => "2964",
        "pg_parameter_acl" => "6243",
        "pg_init_privs" => "3394",
        _ => return None,
    })
}

/// The object a reg* literal names, without its schema, and whether it was quoted:
/// `public."My Table"` is `My Table`, an unquoted name folded to lower case as
/// Postgres reads one.
fn object_name(lit: &str) -> (String, bool) {
    let (mut cur, mut was_quoted) = (String::new(), false);
    let (mut quoted, mut chars) = (false, lit.chars().peekable());
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                cur.push('"');
            }
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            '.' if !quoted => {
                cur.clear();
                was_quoted = false;
            }
            c if quoted => cur.push(c),
            c => cur.extend(c.to_lowercase()),
        }
    }
    (cur, was_quoted)
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
