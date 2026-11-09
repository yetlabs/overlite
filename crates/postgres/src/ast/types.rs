//! The type environment: the type of every expression of a statement, read from its
//! tree and the catalog (`env::Context`), as Postgres' parser would type it for the
//! types overlite stores.
//!
//! A column's type comes from the table or view of the scope it stands in, innermost
//! first, through aliases, joins, subqueries in FROM and CTEs; an expression's from its
//! parts, by the operators' and functions' result types (Postgres' operator resolution
//! for these types: an integer of the wider width, a numeric beside any integer, a
//! double beside anything numeric; `timestamp + interval` a timestamp, `date - date` an
//! integer...), a cast's from its type, a literal's from its form (`1` integer, `1.5`
//! numeric, `'x'` unknown until it meets a typed side). What is not known is `Any`: a
//! table the catalog does not have, a function the environment does not type.
//!
//! The printer (`emit.rs`) asks it at each node where the SQLite SQL depends on a type.

use crate::ast::env::Context;
use crate::ast::labels::{Label, figure_colname};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, SetOperation, SubLinkType, a_const::Val};
use std::rc::Rc;

/// A type, without its modifier.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) enum Base {
    /// An untyped literal (`'x'`, NULL): it takes the type of what it meets.
    Unknown,
    /// A type the environment does not know.
    Any,
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Text,
    Varchar,
    Bpchar,
    Name,
    Bytea,
    Uuid,
    Json,
    Jsonb,
    Date,
    Time,
    Timetz,
    Timestamp,
    Timestamptz,
    Interval,
    Tsvector,
    Tsquery,
    Hstore,
    Inet,
    Cidr,
    Int4Range,
    Int8Range,
    NumRange,
    DateRange,
    TsRange,
    TstzRange,
}

/// A type: a base, or an array of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) struct Ty {
    pub base: Base,
    pub array: bool,
}

impl Ty {
    pub(crate) const ANY: Ty = Ty::of(Base::Any);
    pub(crate) const UNKNOWN: Ty = Ty::of(Base::Unknown);
    pub(crate) const BOOL: Ty = Ty::of(Base::Bool);
    pub(crate) const TEXT: Ty = Ty::of(Base::Text);

    pub(crate) const fn of(base: Base) -> Ty {
        Ty { base, array: false }
    }

    /// The scalar of this base: is it one, and not an array.
    pub(crate) fn is(self, base: Base) -> bool {
        !self.array && self.base == base
    }

    pub(crate) fn known(self) -> bool {
        !matches!(self.base, Base::Unknown | Base::Any)
    }

    /// An integer or floating type, not a numeric.
    pub(crate) fn is_number(self) -> bool {
        !self.array
            && matches!(
                self.base,
                Base::Int2 | Base::Int4 | Base::Int8 | Base::Float4 | Base::Float8
            )
    }

    pub(crate) fn is_integer(self) -> bool {
        !self.array && matches!(self.base, Base::Int2 | Base::Int4 | Base::Int8)
    }

    pub(crate) fn is_numeric(self) -> bool {
        self.is(Base::Numeric)
    }

    /// A date, time, timestamp, timestamptz or interval: what overlite_tadd reads.
    pub(crate) fn is_temporal(self) -> bool {
        !self.array
            && matches!(
                self.base,
                Base::Date | Base::Time | Base::Timestamp | Base::Timestamptz | Base::Interval
            )
    }

    pub(crate) fn is_texty(self) -> bool {
        !self.array && matches!(self.base, Base::Text | Base::Varchar | Base::Bpchar | Base::Name)
    }

    /// The kind overlite_tadd, overlite_tsub and overlite_tmul are told a side is
    /// (builtins/temporal.rs, `Kind::name`), empty for one they read by its value.
    pub(crate) fn kind(self) -> &'static str {
        if self.array {
            return "";
        }
        match self.base {
            Base::Date => "DATE",
            Base::Time => "TIME",
            Base::Timestamp => "TIMESTAMP",
            Base::Timestamptz => "TIMESTAMPTZ",
            Base::Interval => "INTERVAL",
            Base::Numeric => "DECIMAL",
            Base::Int2 | Base::Int4 | Base::Int8 | Base::Float4 | Base::Float8 => "NUMBER",
            _ => "",
        }
    }

    /// The type a select-list item of this type is cast to, so that the protocol sends
    /// it as one: SQLite declares nothing for an expression.
    pub(crate) fn cast_name(self) -> Option<&'static str> {
        if self.array {
            return None;
        }
        Some(match self.base {
            Base::Date => "date",
            Base::Time => "time",
            Base::Timestamp => "timestamp",
            Base::Timestamptz => "timestamptz",
            Base::Interval => "interval",
            Base::Numeric => "numeric",
            _ => return None,
        })
    }

    /// What the result-column mark says of this type (`overlite_engine::TYPED_MARK`):
    /// an integer's width, which a declared INTEGER does not tell the protocol, a
    /// boolean, which SQLite holds as 1 or 0, and an array's element type.
    pub(crate) fn mark(self) -> &'static str {
        if self.array {
            // An array's element type, which a computed column does not tell either.
            return match self.base {
                Base::Int2 => "int2[]",
                Base::Int4 => "int4[]",
                Base::Int8 => "int8[]",
                Base::Bool => "boolean[]",
                Base::Float8 => "double precision[]",
                Base::Numeric => "numeric[]",
                Base::Text | Base::Varchar | Base::Bpchar | Base::Name => "text[]",
                Base::Uuid => "uuid[]",
                Base::Date => "date[]",
                Base::Timestamp => "timestamp[]",
                Base::Jsonb => "jsonb[]",
                _ => "",
            };
        }
        match self {
            t if t.is(Base::Int2) => "int2",
            t if t.is(Base::Int4) => "int4",
            t if t.is(Base::Int8) => "int8",
            // A comparison or a test is a boolean whatever SQLite's 1 or 0 says.
            t if t.is(Base::Bool) => "bool",
            _ => "",
        }
    }

    /// A declared type of this type, as a parameter's is read (types::param_types),
    /// empty for one with none.
    pub(crate) fn decl(self) -> &'static str {
        if self.array {
            return "";
        }
        match self.base {
            Base::Bool => "BOOLEAN",
            Base::Int2 => "SMALLINT",
            Base::Int4 => "INTEGER",
            Base::Int8 => "BIGINT",
            Base::Float4 => "REAL",
            Base::Float8 => "DOUBLE PRECISION",
            Base::Numeric => "DECIMALTEXT",
            Base::Text => "TEXT",
            Base::Varchar => "VARCHAR",
            Base::Bpchar => "CHAR",
            Base::Bytea => "BYTEA",
            Base::Uuid => "UUID",
            Base::Json => "JSON",
            Base::Jsonb => "JSONB",
            Base::Date => "DATE",
            Base::Time => "TIME",
            Base::Timestamp => "TIMESTAMP",
            Base::Timestamptz => "TIMESTAMPTZ",
            Base::Interval => "INTERVAL",
            _ => "",
        }
    }

    /// The type of a column SQLite declares so, as the storage format declares each
    /// (ARCHITECTURE.md, "Storage format").
    pub(crate) fn of_decl(decl: &str) -> Ty {
        let d = decl.trim().trim_matches('"').to_ascii_lowercase();
        if let Some(elem) = d.strip_suffix("[]") {
            let t = Ty::of_decl(elem.trim_end_matches(|c: char| c == ']' || c == '[' || c.is_ascii_digit()));
            return if t.known() {
                Ty {
                    base: t.base,
                    array: true,
                }
            } else {
                Ty::ANY
            };
        }
        if d.ends_with(']') {
            return Ty::ANY;
        }
        let base = d.split('(').next().unwrap_or("").trim();
        let zoned = d.contains("with time zone") && !d.contains("without");
        Ty::of(match base {
            "boolean" | "bool" => Base::Bool,
            "smallint" | "int2" | "smallserial" | "serial2" => Base::Int2,
            "integer" | "int" | "int4" | "serial" | "serial4" | "mediumint" => Base::Int4,
            "bigint" | "int8" | "bigserial" | "serial8" => Base::Int8,
            "real" | "float4" => Base::Float4,
            "double precision" | "double" | "float8" | "float" => Base::Float8,
            "decimaltext" | "numeric" | "decimal" => Base::Numeric,
            "text" | "citext" | "string" | "clob" => Base::Text,
            "varchar" | "character varying" | "nvarchar" => Base::Varchar,
            "char" | "character" | "bpchar" | "nchar" => Base::Bpchar,
            "name" => Base::Name,
            "bytea" | "blob" => Base::Bytea,
            "uuid" => Base::Uuid,
            "json" => Base::Json,
            "jsonb" => Base::Jsonb,
            "date" => Base::Date,
            "time" | "time without time zone" if !zoned => Base::Time,
            "time" | "time with time zone" | "timetz" => Base::Timetz,
            "timestamp" | "timestamp without time zone" | "datetime" if !zoned => Base::Timestamp,
            "timestamp" | "timestamp with time zone" | "timestamptz" => Base::Timestamptz,
            "interval" => Base::Interval,
            "tsvector" => Base::Tsvector,
            "tsquery" => Base::Tsquery,
            "hstore" => Base::Hstore,
            "inet" => Base::Inet,
            "cidr" => Base::Cidr,
            "int4range" => Base::Int4Range,
            "int8range" => Base::Int8Range,
            "numrange" => Base::NumRange,
            "daterange" => Base::DateRange,
            "tsrange" => Base::TsRange,
            "tstzrange" => Base::TstzRange,
            _ => Base::Any,
        })
    }

    /// The rank of a numeric type in Postgres' implicit casts, the wider last.
    fn rank(self) -> u8 {
        match self.base {
            Base::Int2 => 1,
            Base::Int4 => 2,
            Base::Int8 => 3,
            Base::Numeric => 4,
            Base::Float4 => 5,
            Base::Float8 => 6,
            _ => 0,
        }
    }
}

/// Postgres' result type of `l op r` for an arithmetic operator, when it knows one.
pub(crate) fn arithmetic(op: &str, l: Ty, r: Ty) -> Ty {
    use Base::{Date, Interval, Time, Timestamp, Timestamptz};
    let (l, r) = match (l.base, r.base) {
        (Base::Unknown, _) if r.known() => (r, r),
        (_, Base::Unknown) if l.known() => (l, l),
        _ => (l, r),
    };
    if l.is_temporal() || r.is_temporal() {
        let sub = op == "-";
        return Ty::of(match (op, l.base, r.base) {
            // overlite_tmul gives an interval or refuses the product, as Postgres has
            // no other.
            ("*" | "/", Interval, _) | ("*", _, Interval) => Interval,
            ("+" | "-", Timestamp, Interval) | ("+", Interval, Timestamp) => Timestamp,
            ("+" | "-", Timestamptz, Interval) | ("+", Interval, Timestamptz) => Timestamptz,
            ("+" | "-", Date, Interval) | ("+", Interval | Time, Date) | ("+", Date, Time) => Timestamp,
            ("+" | "-", Date, _) if l.is_integer() || r.is_integer() => Date,
            ("+", _, Date) if l.is_integer() => Date,
            ("+" | "-", Time, Interval) | ("+", Interval, Time) => Time,
            ("+" | "-", Interval, Interval) => Interval,
            ("-", Time, Time) => Interval,
            ("-", Timestamp | Timestamptz, Timestamp | Timestamptz) => Interval,
            ("-", Date, Date) => Base::Int4,
            _ if sub => Base::Any,
            _ => Base::Any,
        });
    }
    if op == "||" {
        return concat(l, r);
    }
    let numeric = |t: Ty| t.rank() > 0 && !t.array;
    if numeric(l) && numeric(r) {
        return if l.rank() >= r.rank() { l } else { r };
    }
    // A numeric beside what is not known is exact arithmetic all the same: the printer
    // writes it so (`emit::arithmetic`), and the result is a numeric.
    if (l.is_numeric() && !r.known()) || (r.is_numeric() && !l.known()) {
        return Ty::of(Base::Numeric);
    }
    Ty::ANY
}

/// What `l || r` gives.
fn concat(l: Ty, r: Ty) -> Ty {
    if l.is(Base::Tsvector) || r.is(Base::Tsvector) {
        return Ty::of(Base::Tsvector);
    }
    if l.is(Base::Jsonb) && matches!(r.base, Base::Jsonb | Base::Unknown) {
        return Ty::of(Base::Jsonb);
    }
    if l.array || r.array {
        return if l.array { l } else { r };
    }
    Ty::TEXT
}

/// The type Postgres resolves for the values of a CASE, COALESCE, GREATEST or LEAST:
/// the widest number among numbers, a timestamptz among dates and timestamps beside
/// one, else the first known; text for untyped literals alone.
pub(crate) fn common(tys: &[Ty]) -> Ty {
    let known: Vec<Ty> = tys.iter().copied().filter(|t| t.known()).collect();
    let Some(&first) = known.first() else {
        return if tys.iter().any(|t| t.base == Base::Any) {
            Ty::ANY
        } else {
            Ty::TEXT
        };
    };
    if known.iter().all(|t| t.rank() > 0 && !t.array) {
        return *known.iter().max_by_key(|t| t.rank()).unwrap_or(&first);
    }
    let stamp = |t: &Ty| matches!(t.base, Base::Date | Base::Timestamp | Base::Timestamptz) && !t.array;
    if known.iter().all(stamp) {
        for b in [Base::Timestamptz, Base::Timestamp] {
            if known.iter().any(|t| t.base == b) {
                return Ty::of(b);
            }
        }
    }
    first
}

/// A column of a relation in scope.
#[derive(Debug, Clone)]
pub(crate) struct Col {
    pub name: String,
    pub ty: Ty,
    /// A boolean column that may hold text (`ColumnType::mixed_bool`).
    pub mixed: bool,
    /// The table's rowid (`ColumnType::rowid`), whose width is not known.
    pub rowid: bool,
}

impl Col {
    fn of(c: &overlite_engine::ColumnType) -> Col {
        Col {
            name: c.name.to_lowercase(),
            ty: Ty::of_decl(&c.decl),
            mixed: c.mixed_bool,
            rowid: c.rowid,
        }
    }
}

/// A relation of a FROM: its names (an alias, or the table's name with and without
/// its schema) and its columns, None for one the catalog does not have.
#[derive(Debug, Clone)]
struct Rel {
    name: String,
    schema: String,
    cols: Option<Rc<[Col]>>,
}

/// The types of a statement's expressions, scope by scope as the printer walks it.
pub(crate) struct Typer {
    ctx: Rc<Context>,
    /// The relations of each query level, innermost last.
    scopes: Vec<Vec<Rel>>,
    /// The CTEs visible, innermost last.
    ctes: Vec<(String, Rc<[Col]>)>,
    /// The output columns of the select whose ORDER BY is being read: a bare name
    /// there is an output column first.
    pub(crate) outputs: Option<Vec<Col>>,
    /// Whether a column read so far is a boolean that may hold text, whose readings
    /// the text passes keep (resolve/booleans.rs).
    pub(crate) mixed: bool,
}

fn node(n: &pb::Node) -> Option<&NodeEnum> {
    n.node.as_ref()
}

fn boxed(n: Option<&pb::Node>) -> Option<&NodeEnum> {
    n.and_then(|n| n.node.as_ref())
}

fn string(n: &pb::Node) -> Option<&str> {
    match node(n)? {
        NodeEnum::String(s) => Some(&s.sval),
        _ => None,
    }
}

/// The type a cast's type name says, for the types the printer casts to
/// (`emit::cast_type`'s names), Any for another.
pub(crate) fn of_type_name(t: &pb::TypeName) -> Ty {
    let names: Vec<&str> = t.names.iter().filter_map(string).collect();
    let b = match names.as_slice() {
        ["pg_catalog", b] | [b] => *b,
        _ => return Ty::ANY,
    };
    let base = Ty::of_decl(match b {
        "int2" => "smallint",
        "float8" => "double precision",
        "bpchar" => "char",
        "timestamptz" => "timestamptz",
        "timetz" => "timetz",
        "varchar" => "varchar",
        b => b,
    });
    if t.array_bounds.is_empty() {
        base
    } else if base.known() {
        Ty {
            base: base.base,
            array: true,
        }
    } else {
        Ty::ANY
    }
}

impl Typer {
    pub(crate) fn new(ctx: Rc<Context>) -> Typer {
        Typer {
            ctx,
            scopes: Vec::new(),
            ctes: Vec::new(),
            outputs: None,
            mixed: false,
        }
    }

    // ---- scopes ----

    /// Pushes the CTEs of a WITH, and says how many to pop after its statement.
    pub(crate) fn push_with(&mut self, w: Option<&pb::WithClause>) -> usize {
        let Some(w) = w else { return 0 };
        let mut n = 0;
        for c in &w.ctes {
            let Some(NodeEnum::CommonTableExpr(c)) = node(c) else {
                continue;
            };
            let mut cols = match boxed(c.ctequery.as_deref()) {
                Some(NodeEnum::SelectStmt(q)) => {
                    // A recursive CTE is typed by its first arm, which does not read it.
                    let mut q = q.as_ref();
                    if w.recursive {
                        while q.op != SetOperation::SetopNone as i32 {
                            match q.larg.as_deref() {
                                Some(l) => q = l,
                                None => break,
                            }
                        }
                    }
                    self.select_cols(q)
                }
                _ => Vec::new(),
            };
            for (k, a) in c.aliascolnames.iter().enumerate() {
                if let (Some(col), Some(name)) = (cols.get_mut(k), string(a)) {
                    col.name = name.to_string();
                }
            }
            self.ctes.push((c.ctename.clone(), Rc::from(cols)));
            n += 1;
        }
        n
    }

    pub(crate) fn pop_with(&mut self, n: usize) {
        for _ in 0..n {
            self.ctes.pop();
        }
    }

    /// Opens a query level whose relations are a FROM's.
    pub(crate) fn push_from(&mut self, from: &[pb::Node]) {
        let mut rels = Vec::new();
        for f in from {
            if let Some(f) = node(f) {
                self.table_ref(f, &mut rels);
            }
        }
        self.scopes.push(rels);
    }

    /// Opens a query level whose relation is a write's target.
    pub(crate) fn push_target(&mut self, r: &pb::RangeVar) {
        let mut rels = Vec::new();
        self.range_var(r, &mut rels);
        self.scopes.push(rels);
    }

    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn table_ref(&mut self, f: &NodeEnum, rels: &mut Vec<Rel>) {
        match f {
            NodeEnum::RangeVar(r) => self.range_var(r, rels),
            NodeEnum::JoinExpr(j) => {
                if let Some(l) = boxed(j.larg.as_deref()) {
                    self.table_ref(l, rels);
                }
                if let Some(r) = boxed(j.rarg.as_deref()) {
                    self.table_ref(r, rels);
                }
            }
            NodeEnum::RangeSubselect(s) => {
                let mut cols = match boxed(s.subquery.as_deref()) {
                    Some(NodeEnum::SelectStmt(q)) => self.select_cols(q),
                    _ => Vec::new(),
                };
                // `AS v(a, b)` renames the first columns.
                for (c, name) in cols.iter_mut().zip(s.alias.iter().flat_map(|a| a.colnames.iter())) {
                    if let Some(name) = string(name) {
                        c.name = name.to_string();
                    }
                }
                let name = s.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default();
                rels.push(Rel {
                    name,
                    schema: String::new(),
                    cols: Some(Rc::from(cols)),
                });
            }
            NodeEnum::RangeFunction(r) => {
                let srf = crate::ast::emit::srf_from(r).ok().flatten();
                let Some(srf) = srf else {
                    rels.push(Rel {
                        name: r.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default(),
                        schema: String::new(),
                        cols: None,
                    });
                    return;
                };
                let arg = |k: usize, me: &mut Self| srf.args.get(k).map_or(Ty::ANY, |a| me.ty(a));
                let jsonish = |me: &mut Self| {
                    let t = arg(0, me);
                    if t.is(Base::Json) || t.is(Base::Jsonb) {
                        t
                    } else {
                        Ty::ANY
                    }
                };
                let mut tys: Vec<Ty> = match srf.name {
                    "unnest" => (0..srf.args.len())
                        .map(|k| {
                            let t = arg(k, self);
                            if t.array { Ty::of(t.base) } else { Ty::ANY }
                        })
                        .collect(),
                    "generate_series" => {
                        let t = common(&[arg(0, self), arg(1, self)]);
                        vec![if t.is_integer() { t } else { Ty::ANY }]
                    }
                    "generate_subscripts" => vec![Ty::of(Base::Int4)],
                    "json_object_keys" | "jsonb_object_keys" | "regexp_split_to_table" => vec![Ty::TEXT],
                    n if n.contains("_each_text") => vec![Ty::TEXT, Ty::TEXT],
                    n if n.contains("_each") => vec![Ty::TEXT, jsonish(self)],
                    n if n.ends_with("_text") => vec![Ty::TEXT],
                    _ => vec![jsonish(self)],
                };
                tys.push(Ty::of(Base::Int8));
                let cols: Vec<Col> = srf
                    .names
                    .iter()
                    .zip(tys)
                    .map(|(n, ty)| Col {
                        name: n.clone(),
                        ty,
                        mixed: false,
                        rowid: false,
                    })
                    .collect();
                rels.push(Rel {
                    name: srf.alias.clone(),
                    schema: String::new(),
                    cols: Some(Rc::from(cols)),
                });
            }
            _ => rels.push(Rel {
                name: String::new(),
                schema: String::new(),
                cols: None,
            }),
        }
    }

    fn range_var(&mut self, r: &pb::RangeVar, rels: &mut Vec<Rel>) {
        let name = r
            .alias
            .as_ref()
            .map_or_else(|| r.relname.clone(), |a| a.aliasname.clone());
        let schema = if r.alias.is_some() {
            String::new()
        } else {
            r.schemaname.clone()
        };
        let cte = r
            .schemaname
            .is_empty()
            .then(|| self.ctes.iter().rev().find(|(n, _)| *n == r.relname))
            .flatten()
            .map(|(_, c)| c.clone());
        let cols = cte.or_else(|| {
            self.ctx
                .table(&r.schemaname, &r.relname)
                .map(|cols| Rc::from(cols.iter().map(Col::of).collect::<Vec<_>>()))
        });
        rels.push(Rel { name, schema, cols });
    }

    /// The output columns of a select: names as Postgres gives them, types.
    pub(crate) fn select_cols(&mut self, s: &pb::SelectStmt) -> Vec<Col> {
        let popped = self.push_with(s.with_clause.as_ref());
        let out = if s.op != SetOperation::SetopNone as i32 {
            match s.larg.as_deref() {
                Some(l) => self.select_cols(l),
                None => Vec::new(),
            }
        } else if !s.values_lists.is_empty() {
            self.scopes.push(Vec::new());
            let out = match s.values_lists.first().and_then(node) {
                Some(NodeEnum::List(row)) => row
                    .items
                    .iter()
                    .enumerate()
                    .map(|(k, x)| Col {
                        name: format!("column{}", k + 1),
                        ty: node(x).map_or(Ty::ANY, |x| self.ty(x)),
                        mixed: false,
                        rowid: false,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            self.scopes.pop();
            out
        } else {
            self.push_from(&s.from_clause);
            let out = self.targets(&s.target_list);
            self.scopes.pop();
            out
        };
        self.pop_with(popped);
        out
    }

    /// The columns a target list gives, a star expanded.
    pub(crate) fn targets(&mut self, items: &[pb::Node]) -> Vec<Col> {
        let mut out = Vec::new();
        for t in items {
            let Some(NodeEnum::ResTarget(t)) = node(t) else {
                continue;
            };
            let Some(val) = boxed(t.val.as_deref()) else { continue };
            if let NodeEnum::ColumnRef(c) = val
                && matches!(c.fields.last().and_then(node), Some(NodeEnum::AStar(_)))
            {
                let quals: Vec<&str> = c.fields[..c.fields.len() - 1].iter().filter_map(string).collect();
                for rel in self.scopes.last().cloned().unwrap_or_default() {
                    if !quals.is_empty() && quals.last() != Some(&rel.name.as_str()) {
                        continue;
                    }
                    if let Some(cols) = &rel.cols {
                        out.extend(cols.iter().cloned());
                    }
                }
                continue;
            }
            let ty = self.ty(val);
            let col = match val {
                NodeEnum::ColumnRef(c) => self.column(&c.fields),
                _ => None,
            };
            let name = if t.name.is_empty() {
                match figure_colname(val) {
                    Label::Named(l) => l,
                    Label::Itself => match val {
                        NodeEnum::ColumnRef(c) => c.fields.last().and_then(string).unwrap_or("").to_string(),
                        _ => String::new(),
                    },
                }
            } else {
                t.name.clone()
            };
            out.push(Col {
                name,
                ty,
                mixed: col.as_ref().is_some_and(|c| c.mixed),
                rowid: col.as_ref().is_some_and(|c| c.rowid),
            });
        }
        out
    }

    /// The column a reference names, innermost scope first; None for one not found or
    /// found twice in one scope.
    pub(crate) fn column(&mut self, fields: &[pb::Node]) -> Option<Col> {
        let parts: Vec<&str> = fields.iter().map(string).collect::<Option<_>>()?;
        let (name, quals) = parts.split_last()?;
        if quals.is_empty()
            && let Some(outs) = &self.outputs
            && let Some(c) = outs.iter().find(|c| c.name == *name)
        {
            return Some(c.clone());
        }
        let found = self.lookup(name, quals);
        if found.as_ref().is_some_and(|c| c.mixed) {
            self.mixed = true;
        }
        found
    }

    fn lookup(&self, name: &str, quals: &[&str]) -> Option<Col> {
        let low = name.to_lowercase();
        for scope in self.scopes.iter().rev() {
            let mut hit: Option<Col> = None;
            let mut seen = false;
            for rel in scope {
                let named = match quals {
                    [] => true,
                    [t] => rel.name == *t,
                    [s, t] => rel.name == *t && (rel.schema.is_empty() || rel.schema == *s),
                    _ => false,
                };
                if !named {
                    continue;
                }
                match &rel.cols {
                    // A relation the catalog does not know may have the column: what is
                    // found is not to be trusted.
                    None => return if quals.is_empty() { None } else { Some(any_col(name)) },
                    Some(cols) => {
                        if let Some(c) = cols.iter().find(|c| c.name == low || c.name == name) {
                            if seen {
                                return None;
                            }
                            seen = true;
                            hit = Some(c.clone());
                        } else if !quals.is_empty() {
                            return None;
                        }
                    }
                }
            }
            if hit.is_some() {
                return hit;
            }
            if !quals.is_empty() && scope.iter().any(|r| r.name == *quals.last().unwrap_or(&"")) {
                return None;
            }
        }
        None
    }

    // ---- expressions ----

    /// The type of an expression in the current scopes.
    pub(crate) fn ty(&mut self, n: &NodeEnum) -> Ty {
        match n {
            NodeEnum::ColumnRef(c) => self.column(&c.fields).map_or(Ty::ANY, |c| c.ty),
            NodeEnum::ParamRef(_) => Ty::UNKNOWN,
            NodeEnum::AConst(k) => const_ty(k),
            NodeEnum::TypeCast(t) => t.type_name.as_ref().map_or(Ty::ANY, of_type_name),
            NodeEnum::AExpr(a) => self.a_expr(a),
            NodeEnum::BoolExpr(_) | NodeEnum::NullTest(_) | NodeEnum::BooleanTest(_) => Ty::BOOL,
            NodeEnum::CoalesceExpr(c) => {
                let tys: Vec<Ty> = c.args.iter().filter_map(node).map(|x| self.ty(x)).collect();
                common(&tys)
            }
            NodeEnum::MinMaxExpr(m) => {
                let tys: Vec<Ty> = m.args.iter().filter_map(node).map(|x| self.ty(x)).collect();
                common(&tys)
            }
            NodeEnum::CaseExpr(c) => {
                let mut tys: Vec<Ty> = c
                    .args
                    .iter()
                    .filter_map(|w| match node(w) {
                        Some(NodeEnum::CaseWhen(w)) => boxed(w.result.as_deref()),
                        _ => None,
                    })
                    .map(|x| self.ty(x))
                    .collect();
                if let Some(d) = boxed(c.defresult.as_deref()) {
                    tys.push(self.ty(d));
                }
                if tys.iter().any(|t| t.is_numeric()) {
                    return Ty::of(Base::Numeric);
                }
                common(&tys)
            }
            NodeEnum::FuncCall(f) => self.func(f),
            NodeEnum::SubLink(s) => match SubLinkType::try_from(s.sub_link_type) {
                Ok(SubLinkType::ExprSublink) => match boxed(s.subselect.as_deref()) {
                    Some(NodeEnum::SelectStmt(q)) => self.select_cols(q).first().map_or(Ty::ANY, |c| c.ty),
                    _ => Ty::ANY,
                },
                Ok(SubLinkType::ArraySublink) => match boxed(s.subselect.as_deref()) {
                    Some(NodeEnum::SelectStmt(q)) => match self.select_cols(q).first() {
                        Some(c) if c.ty.known() && !c.ty.array => Ty {
                            base: c.ty.base,
                            array: true,
                        },
                        _ => Ty::ANY,
                    },
                    _ => Ty::ANY,
                },
                _ => Ty::BOOL,
            },
            NodeEnum::AArrayExpr(a) => {
                let tys: Vec<Ty> = a.elements.iter().filter_map(node).map(|x| self.ty(x)).collect();
                let t = common(&tys);
                if t.known() && !t.array {
                    Ty {
                        base: t.base,
                        array: true,
                    }
                } else if tys.iter().all(|t| t.base == Base::Unknown) && !tys.is_empty() {
                    Ty {
                        base: Base::Text,
                        array: true,
                    }
                } else {
                    Ty::ANY
                }
            }
            // A subscript of an array is its element, a slice an array.
            NodeEnum::AIndirection(i) => {
                let t = boxed(i.arg.as_deref()).map_or(Ty::ANY, |a| self.ty(a));
                if !t.array {
                    return Ty::ANY;
                }
                let slice = i
                    .indirection
                    .iter()
                    .any(|x| matches!(node(x), Some(NodeEnum::AIndices(ix)) if ix.is_slice));
                if slice { t } else { Ty::of(t.base) }
            }
            NodeEnum::CollateClause(c) => boxed(c.arg.as_deref()).map_or(Ty::ANY, |a| self.ty(a)),
            NodeEnum::JsonArrayConstructor(_) | NodeEnum::JsonObjectConstructor(_) => Ty::of(Base::Jsonb),
            NodeEnum::SqlvalueFunction(f) => {
                use pb::SqlValueFunctionOp as O;
                Ty::of(match O::try_from(f.op) {
                    Ok(O::SvfopCurrentDate) => Base::Date,
                    Ok(O::SvfopCurrentTimestamp | O::SvfopCurrentTimestampN) => Base::Timestamptz,
                    Ok(O::SvfopLocaltimestamp | O::SvfopLocaltimestampN) => Base::Timestamp,
                    Ok(O::SvfopLocaltime | O::SvfopLocaltimeN) => Base::Time,
                    Ok(O::SvfopCurrentTime | O::SvfopCurrentTimeN) => Base::Timetz,
                    _ => Base::Name,
                })
            }
            _ => Ty::ANY,
        }
    }

    fn a_expr(&mut self, a: &pb::AExpr) -> Ty {
        use pb::AExprKind as K;
        let op = match a.name.as_slice() {
            [n] => string(n).unwrap_or(""),
            [_, n] => string(n).unwrap_or(""),
            _ => "",
        };
        match K::try_from(a.kind) {
            Ok(K::AexprOp) => {
                let r = boxed(a.rexpr.as_deref()).map_or(Ty::ANY, |r| self.ty(r));
                let Some(l) = boxed(a.lexpr.as_deref()) else {
                    return if matches!(op, "-" | "+") { r } else { Ty::ANY };
                };
                let l = self.ty(l);
                match op {
                    "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=" | "~" | "!~" | "~*" | "!~*" | "@>" | "<@" | "?"
                    | "?|" | "?&" | "&&" | "@@" => Ty::BOOL,
                    "%" if l.is_texty() || r.is_texty() || l.base == Base::Unknown || r.base == Base::Unknown => {
                        Ty::BOOL
                    }
                    "+" | "-" | "*" | "/" | "%" | "||" => arithmetic(op, l, r),
                    "->" | "#>" if l.is(Base::Jsonb) || l.is(Base::Json) => l,
                    "->>" | "#>>" if l.is(Base::Jsonb) || l.is(Base::Json) => Ty::TEXT,
                    _ => Ty::ANY,
                }
            }
            Ok(K::AexprNullif) => boxed(a.lexpr.as_deref()).map_or(Ty::ANY, |l| self.ty(l)),
            Ok(_) => Ty::BOOL,
            Err(_) => Ty::ANY,
        }
    }

    /// The result type of a function call, by Postgres' signature of the function
    /// overlite provides under that name.
    fn func(&mut self, f: &pb::FuncCall) -> Ty {
        let name = match f.funcname.as_slice() {
            [n] => string(n).unwrap_or(""),
            [s, n] if string(s) == Some("pg_catalog") => string(n).unwrap_or(""),
            _ => return Ty::ANY,
        };
        let args: Vec<&NodeEnum> = f.args.iter().filter_map(node).collect();
        let arg = |k: usize, me: &mut Self| args.get(k).map_or(Ty::ANY, |a| me.ty(a));
        let array_of = |t: Ty| {
            if t.known() && !t.array {
                Ty {
                    base: t.base,
                    array: true,
                }
            } else {
                Ty::ANY
            }
        };
        match name {
            "count" | "row_number" | "rank" | "dense_rank" | "ntile" | "nextval" | "currval" | "setval" | "lastval"
            | "pg_backend_pid" => Ty::of(Base::Int8),
            "percent_rank" | "cume_dist" | "random" => Ty::of(Base::Float8),
            "lag" | "lead" | "first_value" | "last_value" | "nth_value" => arg(0, self),
            "sum" => {
                let t = arg(0, self);
                match t.base {
                    _ if t.array => Ty::ANY,
                    Base::Int2 | Base::Int4 => Ty::of(Base::Int8),
                    Base::Int8 | Base::Numeric => Ty::of(Base::Numeric),
                    Base::Float4 => Ty::of(Base::Float4),
                    Base::Float8 => Ty::of(Base::Float8),
                    Base::Interval => t,
                    _ => Ty::ANY,
                }
            }
            "avg" => {
                let t = arg(0, self);
                if t.is_integer() || t.is_numeric() {
                    Ty::of(Base::Numeric)
                } else if t.is(Base::Interval) {
                    t
                } else if t.is_number() {
                    Ty::of(Base::Float8)
                } else {
                    Ty::ANY
                }
            }
            "array_agg" => array_of(arg(0, self)),
            "string_agg"
            | "array_to_string"
            | "btrim"
            | "ltrim"
            | "rtrim"
            | "substring"
            | "translate"
            | "regexp_replace"
            | "quote_ident"
            | "quote_literal"
            | "quote_nullable"
            | "format"
            | "encode"
            | "version"
            | "current_setting"
            | "format_type"
            | "unaccent"
            | "host"
            | "text"
            | "pg_get_userbyid"
            | "pg_get_expr"
            | "pg_get_triggerdef"
            | "obj_description"
            | "col_description"
            | "set_config"
            | "pg_encoding_to_char" => Ty::TEXT,
            "current_database" | "current_schema" => Ty::of(Base::Name),
            "gen_random_uuid" | "uuid_generate_v4" => Ty::of(Base::Uuid),
            "array_length" | "array_upper" | "array_lower" | "cardinality" | "json_array_length"
            | "jsonb_array_length" | "position" => Ty::of(Base::Int4),
            "regexp_split_to_array" | "string_to_array" | "akeys" | "avals" => Ty {
                base: Base::Text,
                array: true,
            },
            "json_build_object" | "json_build_array" | "json_agg" | "json_object_agg" | "to_json" => Ty::of(Base::Json),
            "jsonb_build_object" | "jsonb_build_array" | "jsonb_agg" | "jsonb_object_agg" | "to_jsonb" => {
                Ty::of(Base::Jsonb)
            }
            "to_tsvector" | "setweight" | "strip" | "tsvector_in" => Ty::of(Base::Tsvector),
            "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery" | "websearch_to_tsquery" => Ty::of(Base::Tsquery),
            "ts_rank" | "ts_rank_cd" | "similarity" | "word_similarity" | "strict_word_similarity" => {
                Ty::of(Base::Float4)
            }
            "pg_try_advisory_lock"
            | "pg_try_advisory_xact_lock"
            | "isempty"
            | "lower_inc"
            | "upper_inc"
            | "lower_inf"
            | "upper_inf"
            | "has_table_privilege"
            | "has_column_privilege" => Ty::BOOL,
            "hstore" => Ty::of(Base::Hstore),
            "int4range" => Ty::of(Base::Int4Range),
            "int8range" => Ty::of(Base::Int8Range),
            "numrange" => Ty::of(Base::NumRange),
            "daterange" => Ty::of(Base::DateRange),
            "tsrange" => Ty::of(Base::TsRange),
            "tstzrange" => Ty::of(Base::TstzRange),
            // lower and upper of a range are its bounds, of text text.
            "lower" | "upper" if args.len() == 1 => {
                let t = arg(0, self);
                match t.base {
                    _ if t.array => Ty::ANY,
                    Base::Int4Range => Ty::of(Base::Int4),
                    Base::Int8Range => Ty::of(Base::Int8),
                    Base::NumRange => Ty::of(Base::Numeric),
                    Base::DateRange => Ty::of(Base::Date),
                    Base::TsRange => Ty::of(Base::Timestamp),
                    Base::TstzRange => Ty::of(Base::Timestamptz),
                    _ => Ty::TEXT,
                }
            }
            "log" | "log10" | "cbrt" | "trunc" => {
                let t = arg(0, self);
                if t.is_numeric() { t } else { Ty::of(Base::Float8) }
            }
            "min" | "max" | "any_value" | "bit_and" | "bit_or" | "bit_xor" => arg(0, self),
            "abs" | "ceil" | "ceiling" | "floor" | "sign" => {
                let t = arg(0, self);
                if t.is_number() || t.is_numeric() { t } else { Ty::ANY }
            }
            "round" => {
                let t = arg(0, self);
                if t.is_numeric() || args.len() > 1 {
                    Ty::of(Base::Numeric)
                } else if t.is_number() {
                    Ty::of(Base::Float8)
                } else {
                    Ty::ANY
                }
            }
            "dec_add" | "dec_sub" | "dec_mul" | "dec_div" => Ty::of(Base::Numeric),
            "length" | "char_length" | "octet_length" | "strpos" | "ascii" => Ty::of(Base::Int4),
            "replace" | "substr" | "split_part" | "lpad" | "rpad" | "left" | "right" | "reverse" | "repeat"
            | "concat" | "initcap" | "chr" | "md5" | "hex" | "typeof" | "to_char" => Ty::TEXT,
            // Of integers and numerics a numeric, of floats a double.
            "stddev_pop" | "stddev_samp" | "var_pop" | "var_samp" => {
                let t = arg(0, self);
                if t.is_integer() || t.is_numeric() {
                    Ty::of(Base::Numeric)
                } else if t.is_number() {
                    Ty::of(Base::Float8)
                } else {
                    Ty::ANY
                }
            }
            "sqrt" | "power" | "exp" | "ln" | "pi" | "sin" | "cos" | "tan" | "cot" | "asin" | "acos" | "atan"
            | "atan2" | "degrees" | "radians" | "covar_pop" | "covar_samp" | "corr" | "regr_avgx" | "regr_avgy"
            | "regr_intercept" | "regr_r2" | "regr_slope" | "regr_sxx" | "regr_sxy" | "regr_syy" => {
                Ty::of(Base::Float8)
            }
            "regr_count" => Ty::of(Base::Int8),
            "mod" => {
                let (a, b) = (arg(0, self), arg(1, self));
                arithmetic("%", a, b)
            }
            "bool_and" | "bool_or" | "every" => Ty::BOOL,
            "date_trunc" => match arg(1, self).base {
                Base::Date | Base::Timestamptz => Ty::of(Base::Timestamptz),
                b @ (Base::Timestamp | Base::Interval) => Ty::of(b),
                _ => Ty::ANY,
            },
            "age" => Ty::of(Base::Interval),
            "timezone" if args.len() == 2 => match arg(1, self).base {
                Base::Timestamp => Ty::of(Base::Timestamptz),
                // What the environment does not know is taken for an instant, as the
                // printer writes it (overlite_column_at_time_zone).
                Base::Timestamptz | Base::Unknown | Base::Any => Ty::of(Base::Timestamp),
                _ => Ty::ANY,
            },
            // EXTRACT is a numeric since Postgres 14; date_part a double.
            "extract" => Ty::of(Base::Numeric),
            "date_part" => Ty::of(Base::Float8),
            "now" | "transaction_timestamp" | "statement_timestamp" | "clock_timestamp" => Ty::of(Base::Timestamptz),
            _ => Ty::ANY,
        }
    }
}

/// A column of a relation the catalog does not know.
fn any_col(name: &str) -> Col {
    Col {
        name: name.to_string(),
        ty: Ty::ANY,
        mixed: false,
        rowid: false,
    }
}

/// The type of a constant: an integer `int4`, or `int8` past it; a number with a point
/// or an exponent `numeric`; a string unknown until it meets a type; NULL unknown.
pub(crate) fn const_ty(k: &pb::AConst) -> Ty {
    if k.isnull {
        return Ty::UNKNOWN;
    }
    match &k.val {
        Some(Val::Ival(_)) => Ty::of(Base::Int4),
        Some(Val::Fval(f)) => {
            let digits = f.fval.trim_start_matches('-');
            if digits.bytes().all(|c| c.is_ascii_digit()) {
                if digits.parse::<i64>().is_ok() {
                    Ty::of(Base::Int8)
                } else {
                    Ty::of(Base::Numeric)
                }
            } else {
                Ty::of(Base::Numeric)
            }
        }
        Some(Val::Boolval(_)) => Ty::BOOL,
        Some(Val::Sval(_)) => Ty::UNKNOWN,
        _ => Ty::ANY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overlite_engine::TableTypes;
    use std::sync::Arc;

    fn typer(tables: &[(&str, &[(&str, &str)])]) -> Typer {
        Typer::new(Rc::new(Context::new(Arc::new(TableTypes::of(tables)), None)))
    }

    /// The type of each item of a SELECT's list.
    fn item_types(t: &mut Typer, sql: &str) -> Vec<Ty> {
        let parsed = crate::ast::parse::parse(sql).unwrap();
        let Some(NodeEnum::SelectStmt(s)) = parsed.stmts[0].stmt.as_deref().and_then(|s| s.node.as_ref()) else {
            panic!("{sql}");
        };
        t.select_cols(s).into_iter().map(|c| c.ty).collect()
    }

    #[test]
    fn expressions_are_typed_by_scope() {
        let mut t = typer(&[
            (
                "t",
                &[
                    ("id", "INTEGER"),
                    ("n", "DECIMALTEXT(6,2)"),
                    ("ts", "timestamp"),
                    ("b", "BIGINT"),
                ],
            ),
            ("u", &[("id", "INTEGER"), ("tz", "TIMESTAMPTZ"), ("d", "DATE")]),
        ]);
        let b = Ty::of;
        for (sql, want) in [
            (
                "SELECT id, n, ts FROM t",
                vec![b(Base::Int4), b(Base::Numeric), b(Base::Timestamp)],
            ),
            (
                "SELECT n + 1, b + id, ts + interval '1 day', 1.5 FROM t",
                vec![b(Base::Numeric), b(Base::Int8), b(Base::Timestamp), b(Base::Numeric)],
            ),
            (
                "SELECT x.tz, d - d, tz - tz FROM u AS x",
                vec![b(Base::Timestamptz), b(Base::Int4), b(Base::Interval)],
            ),
            // id is in both: ambiguous, so not known.
            ("SELECT id FROM t, u", vec![Ty::ANY]),
            (
                "SELECT s.m FROM (SELECT max(n) AS m FROM t) AS s",
                vec![b(Base::Numeric)],
            ),
            (
                "WITH c (k) AS (SELECT ts FROM t) SELECT k FROM c",
                vec![b(Base::Timestamp)],
            ),
            (
                "SELECT (SELECT d FROM u LIMIT 1), coalesce(d, ts) FROM t, u",
                vec![b(Base::Date), b(Base::Timestamp)],
            ),
            (
                "SELECT count(*), sum(t.id), avg(n), date_trunc('day', d) FROM u, t",
                vec![b(Base::Int8), b(Base::Int8), b(Base::Numeric), b(Base::Timestamptz)],
            ),
            (
                "SELECT * FROM u",
                vec![b(Base::Int4), b(Base::Timestamptz), b(Base::Date)],
            ),
            ("SELECT nope.x, y FROM nope", vec![Ty::ANY, Ty::ANY]),
        ] {
            assert_eq!(item_types(&mut t, sql), want, "{sql}");
        }
    }

    #[test]
    fn declared_types() {
        for (decl, want) in [
            ("DECIMALTEXT(10,2)", Ty::of(Base::Numeric)),
            ("timestamp with time zone", Ty::of(Base::Timestamptz)),
            ("TIMESTAMP(3)", Ty::of(Base::Timestamp)),
            (
                "text[]",
                Ty {
                    base: Base::Text,
                    array: true,
                },
            ),
            (
                "\"numeric(10,2)[]\"",
                Ty {
                    base: Base::Numeric,
                    array: true,
                },
            ),
            ("INTEGER", Ty::of(Base::Int4)),
            ("widget", Ty::ANY),
        ] {
            assert_eq!(Ty::of_decl(decl), want, "{decl}");
        }
    }
}
