//! Parameter types, for Describe: Postgres says what each $n is, and clients
//! encode their values by it. SQLite knows no such thing, so every parameter
//! went out unspecified, and a client sent a boolean as the text "t", stored as
//! that string, not SQLite's 1, never equal to true again.
//!
//! A parameter put straight into a column takes that column's type, from the
//! table's declaration: INSERT's VALUES by its column list or position, and
//! anywhere else a column written beside it (`col = $n`, `$n < col`, `col + $n`,
//! `col IN ($n, ...)`, `col BETWEEN $n AND $m`, `col = ANY($n)`), in a subquery,
//! EXISTS, a join or a CTE as at the top level. The column is looked up in the
//! table its qualifier names, or among the statement's tables that have it,
//! which must agree (overlite_engine::param_columns). A catalog query's
//! parameters take the type of the catalog column they meet: `name` for nspname
//! and its kin, `oid` for oids. Types a client declared in Parse are kept, as
//! Postgres keeps them.
//!
//! Every type advertised here is one decode_typed_param reads back in binary:
//! clients like tokio-postgres (Prisma's schema engine, sqlx, Diesel) send every
//! parameter in binary by the type they are told. They cannot send one of
//! unspecified type at all: tokio-postgres looks the type up in pg_type, whose
//! own parameter came back unspecified too, and recursed until it crashed.
//! Anything else is still left unspecified, which steers clients that can to
//! text, as before.

use crate::auth::rls::text_or_empty;
use crate::session::Session;
use crate::types::array::{OID_INT8_ARR, OID_TEXT_ARR, OID_UUID_ARR, is_array_decl};
use crate::types::{
    OID_BOOL, OID_BPCHAR, OID_DATE, OID_FLOAT8, OID_INT8, OID_TEXT, OID_TIMESTAMP, OID_TIMESTAMPTZ, OID_UUID,
    OID_VARCHAR, is_text_affinity, oid_from_decl_type, strict_decl_oid,
};
use overlite_core::text::{eq_fold, quote_literal, trim_start_chars};
use overlite_core::{Oid, lazy_regex};
use overlite_engine::{Meets, NamedTable, blank_literals, param_columns as statement_params};
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

const TABLE: &str = r#"((?:"[^"]+"|\w+)(?:\s*\.\s*(?:"[^"]+"|\w+))?)"#;

lazy_regex!(
    INSERT_INTO,
    &(r"(?is)^\s*INSERT\s+INTO\s+".to_string() + TABLE + r"\s*\(([^)]*)\)\s*VALUES\s*")
);
lazy_regex!(
    INSERT_BARE,
    &(r"(?is)^\s*INSERT\s+INTO\s+".to_string() + TABLE + r"\s+VALUES\s*")
);
lazy_regex!(
    COMPARED,
    r#"(?i)(?:\w+\s*\.\s*)?("[^"]+"|\b\w+)\s*(?:=|<>|!=|<=|>=|<|>|\+|-|\*|/)\s*\$(\d+)\b"#
);
lazy_regex!(
    COMPARED_ANY,
    r#"(?i)(?:\w+\s*\.\s*)?("[^"]+"|\b\w+)\s*=\s*ANY\s*\(\s*\$(\d+)\s*\)"#
);
lazy_regex!(PLACEHOLDER, r"^\$(\d+)$");
lazy_regex!(
    INTEGER_DECL,
    r"(?i)^\s*(?:(?:small|big)?int(?:eger|2|4|8)?|(?:small|big)?serial[248]?)\s*$"
);
lazy_regex!(READS_CATALOG, r"(?i)\b(?:pg_\w+|information_schema)\b");
lazy_regex!(
    LIMIT_OFFSET,
    r"(?i)\b(?:LIMIT|OFFSET|FETCH\s+(?:FIRST|NEXT))\s+\$(\d+)\b"
);

// Postgres' own types for the identifier and oid columns of its catalogs.
pub(crate) const OID_NAME: Oid = Oid::new(19);
pub(crate) const OID_OID: Oid = Oid::new(26);
pub(crate) const OID_NAME_ARR: Oid = Oid::new(1003);
pub(crate) const OID_OID_ARR: Oid = Oid::new(1028);

/// The type of the catalog columns a client compares a parameter with. They only
/// count in a statement that reads the catalog, where these names cannot be a
/// table's own column.
static CATALOG_PARAM_OIDS: LazyLock<HashMap<&'static str, Oid>> = LazyLock::new(|| {
    HashMap::from([
        ("nspname", OID_NAME),
        ("relname", OID_NAME),
        ("typname", OID_NAME),
        ("attname", OID_NAME),
        ("conname", OID_NAME),
        ("proname", OID_NAME),
        ("rolname", OID_NAME),
        ("datname", OID_NAME),
        ("schemaname", OID_NAME),
        ("tablename", OID_NAME),
        ("viewname", OID_NAME),
        ("indexname", OID_NAME),
        ("sequencename", OID_NAME),
        ("usename", OID_NAME),
        ("enumlabel", OID_NAME),
        ("table_schema", OID_NAME),
        ("table_name", OID_NAME),
        ("column_name", OID_NAME),
        ("sequence_schema", OID_NAME),
        ("sequence_name", OID_NAME),
        ("constraint_name", OID_NAME),
        ("constraint_schema", OID_NAME),
        ("routine_schema", OID_NAME),
        ("udt_schema", OID_NAME),
        ("oid", OID_OID),
        ("typelem", OID_OID),
        ("typbasetype", OID_OID),
        ("typrelid", OID_OID),
        ("typnamespace", OID_OID),
        ("typarray", OID_OID),
        ("relnamespace", OID_OID),
        ("reltype", OID_OID),
        ("pronamespace", OID_OID),
        ("connamespace", OID_OID),
        ("conrelid", OID_OID),
        ("confrelid", OID_OID),
        ("contypid", OID_OID),
        ("indrelid", OID_OID),
        ("indexrelid", OID_OID),
        ("attrelid", OID_OID),
        ("atttypid", OID_OID),
        ("enumtypid", OID_OID),
        ("objoid", OID_OID),
        ("classoid", OID_OID),
        ("adrelid", OID_OID),
        ("starelid", OID_OID),
        ("evtfoid", OID_OID),
        ("tgrelid", OID_OID),
    ])
});

/// Reads a digit run: a value too big comes back clamped, which is out
/// of every parameter's range anyway.
fn atoi(s: &str) -> i64 {
    s.parse::<i64>().unwrap_or(i64::MAX)
}

/// Types each parameter still unspecified that meets a catalog column of a known type
/// (the tree's reading of what the text reader's COMPARED and COMPARED_ANY find): a
/// statement reading the catalog, whose views the type environment does not know.
fn catalog_params(refs: &[overlite_engine::ParamColumn], oids: &mut [Oid]) {
    for r in refs {
        let Some(o) = r.param.checked_sub(1).and_then(|i| oids.get_mut(i)) else {
            continue;
        };
        if !o.is_unspecified() {
            continue;
        }
        *o = match (catalog_param_oid(&r.column), r.meets == Meets::Array) {
            (OID_NAME, true) => OID_NAME_ARR,
            (OID_OID, true) => OID_OID_ARR,
            (oid, _) => oid,
        };
    }
}

fn catalog_param_oid(col: &str) -> Oid {
    CATALOG_PARAM_OIDS
        .get(col.to_lowercase().as_str())
        .copied()
        .unwrap_or(Oid::UNSPECIFIED)
}

impl Session {
    /// The OID of each of a statement's n parameters, 0 for unknown. declared holds
    /// the types the client gave in Parse, which win.
    pub(crate) fn param_types(&mut self, sql: &str, n: usize, declared: &[Oid]) -> Vec<Oid> {
        let mut oids = vec![Oid::UNSPECIFIED; n];
        if n == 0 {
            return oids;
        }
        // A statement the AST path translates is typed from its tree alone: by the
        // type environment where it knows what a parameter meets (a column through any
        // scope, an expression, a cast), then by the tree's facts.
        if *crate::ast::ENABLED
            && let Some(facts) = crate::ast::params::facts(sql)
        {
            let ctx = self.type_context(sql);
            if let Some(tys) = crate::ast::env::with(ctx, || crate::ast::param_types(sql)) {
                for (o, t) in oids.iter_mut().zip(tys) {
                    *o = param_oid(t.decl());
                }
            }
            if READS_CATALOG.is_match(sql) {
                catalog_params(&facts.refs, &mut oids);
            }
            self.type_by_tree(facts, &mut oids);
        } else {
            self.type_by_text(sql, &mut oids);
        }
        for (i, oid) in declared.iter().enumerate() {
            if i < n && !oid.is_unspecified() {
                oids[i] = *oid;
            }
        }
        oids
    }

    /// Types parameters from the tree's facts: the INSERT slot each fills, the column
    /// each meets, and the row counts.
    fn type_by_tree(&mut self, facts: crate::ast::params::Facts, oids: &mut [Oid]) {
        use crate::ast::params::Slot;
        if let Some((table, slots)) = &facts.insert
            && !slots.is_empty()
        {
            let types = self.column_types(table);
            let order = if slots.iter().any(|(_, s)| matches!(s, Slot::Position(_))) {
                self.column_order(table)
            } else {
                Vec::new()
            };
            for (p, slot) in slots {
                let col = match slot {
                    Slot::Column(c) => c.to_lowercase(),
                    Slot::Position(k) => match order.get(*k) {
                        Some(c) => c.clone(),
                        None => continue,
                    },
                };
                if let Some(o) = p.checked_sub(1).and_then(|i| oids.get_mut(i))
                    && o.is_unspecified()
                {
                    *o = param_oid(types.get(&col).map_or("", String::as_str));
                }
            }
        }
        self.type_by_refs(&facts.tables, facts.refs, oids);
        for p in facts.row_counts {
            if let Some(o) = p.checked_sub(1).and_then(|i| oids.get_mut(i))
                && o.is_unspecified()
            {
                *o = OID_INT8;
            }
        }
    }

    /// Types parameters by reading the text, for statements outside the AST path.
    fn type_by_text(&mut self, sql: &str, oids: &mut [Oid]) {
        let top = oids.len() as i64;
        if READS_CATALOG.is_match(sql) {
            for (i, col) in compared_params(sql, &COMPARED) {
                if i >= 1 && i <= top {
                    oids[(i - 1) as usize] = catalog_param_oid(&col);
                }
            }
            for (i, col) in compared_params(sql, &COMPARED_ANY) {
                if i < 1 || i > top {
                    continue;
                }
                match catalog_param_oid(&col) {
                    OID_NAME => oids[(i - 1) as usize] = OID_NAME_ARR,
                    OID_OID => oids[(i - 1) as usize] = OID_OID_ARR,
                    _ => {}
                }
            }
        }
        // What a parameter meets is in the code: a literal can read like a clause.
        let code = blank_literals(sql);
        let (tbl, mut cols, anys) = insert_columns(&code);
        if !tbl.is_empty() {
            let types = self.column_types(&tbl);
            if let Some(positional) = insert_positions(&code) {
                // INSERT INTO t VALUES (...) fills the columns in their order.
                let order = self.column_order(&tbl);
                for (p, pos) in positional {
                    if pos < order.len() {
                        cols.insert(p, order[pos].clone());
                    }
                }
            }
            let decl_of = |col: &str| types.get(&col.to_lowercase()).map_or("", std::string::String::as_str);
            for (i, col) in &cols {
                let i = *i;
                if i >= 1 && i <= top && oids[(i - 1) as usize].is_unspecified() {
                    oids[(i - 1) as usize] = param_oid(decl_of(col));
                }
            }
            for (i, col) in &anys {
                let i = *i;
                if i >= 1 && i <= top && oids[(i - 1) as usize].is_unspecified() {
                    oids[(i - 1) as usize] = array_param_oid(param_oid(decl_of(col)));
                }
            }
        }
        self.type_by_columns(sql, oids);
        // A row count is a bigint, whatever else the statement says. Left
        // unspecified, tokio-postgres (Prisma) looks type 0 up in pg_type, finds
        // nothing, and fails the query; Prisma writes OFFSET $n on every findFirst.
        for m in LIMIT_OFFSET.captures_iter(sql) {
            let i = atoi(&m[1]);
            if i >= 1 && i <= top && oids[(i - 1) as usize].is_unspecified() {
                oids[(i - 1) as usize] = OID_INT8;
            }
        }
    }
}

impl Session {
    /// Types each parameter still unspecified by the column it meets, wherever it is:
    /// a subquery, EXISTS, IN, a join, a CTE, through an alias or a qualified name.
    /// A qualified column is looked up in the table its qualifier names; a bare one in
    /// every table of the statement that has it, which must agree.
    fn type_by_columns(&mut self, sql: &str, oids: &mut [Oid]) {
        if oids.iter().all(|o| !o.is_unspecified()) {
            return;
        }
        // The tree knows each column's scope; the text reader is the fallback.
        let (tables, refs) = crate::ast::ENABLED
            .then(|| crate::ast::params::param_columns(sql))
            .flatten()
            .unwrap_or_else(|| statement_params(sql));
        self.type_by_refs(&tables, refs, oids);
    }

    /// Types each parameter still unspecified by the column of these tables it meets.
    fn type_by_refs(&mut self, tables: &[NamedTable], refs: Vec<overlite_engine::ParamColumn>, oids: &mut [Oid]) {
        if oids.iter().all(|o| !o.is_unspecified()) {
            return;
        }
        if tables.is_empty() || refs.is_empty() {
            return;
        }
        let mut decls: Vec<Option<HashMap<String, String>>> = vec![None; tables.len()];
        for r in refs {
            let Some(slot) = r.param.checked_sub(1).filter(|i| *i < oids.len()) else {
                continue;
            };
            if !oids[slot].is_unspecified() {
                continue;
            }
            let named: Vec<usize> = if r.qualifier.is_empty() {
                Vec::new()
            } else {
                (0..tables.len())
                    .filter(|&i| qualifies(&tables[i], &r.qualifier))
                    .collect()
            };
            let candidates: Vec<usize> = if named.is_empty() {
                (0..tables.len()).collect()
            } else {
                named
            };
            let mut found: Option<String> = None;
            let mut agree = true;
            for i in candidates {
                if decls[i].is_none() {
                    decls[i] = Some(self.column_types(&tables[i].text));
                }
                let Some(d) = decls[i].as_ref().and_then(|cols| cols.get(&r.column)) else {
                    continue;
                };
                match &found {
                    Some(f) if !eq_fold(f, d) => agree = false,
                    Some(_) => {}
                    None => found = Some(d.clone()),
                }
            }
            if let (true, Some(decl)) = (agree, found) {
                oids[slot] = match r.meets {
                    Meets::Value => param_oid(&decl),
                    Meets::Array => array_param_oid(param_oid(&decl)),
                };
            }
        }
    }
}

/// Whether a column's qualifier names this table: its alias, or the end of its name.
fn qualifies(t: &NamedTable, qualifier: &[String]) -> bool {
    match &t.alias {
        Some(a) => qualifier.len() == 1 && &qualifier[0] == a,
        None => t.parts.ends_with(qualifier),
    }
}

/// The type to advertise for a parameter going into a column declared decl: one
/// decode_typed_param can read in binary, or 0. Integers are int8, as their values
/// are advertised.
fn param_oid(decl: &str) -> Oid {
    if decl.is_empty()
        || is_array_decl(decl)
        || eq_fold(decl.trim(), "hstore")
        || decl.to_uppercase().contains("DECIMALTEXT")
        || decl.trim().to_uppercase().starts_with("INTERVAL")
    {
        return Oid::UNSPECIFIED;
    }
    if let Some(oid) = strict_decl_oid(decl) {
        return oid;
    }
    let oid = oid_from_decl_type(decl);
    match oid {
        OID_BOOL | OID_FLOAT8 | OID_TIMESTAMP | OID_TIMESTAMPTZ | OID_DATE => return oid,
        OID_INT8 => {
            // SQLite's affinity finds INT in an interval or a point too.
            if INTEGER_DECL.is_match(decl) {
                return oid;
            }
        }
        OID_TEXT
            // Text is also where any type SQLite does not recognize lands (an
            // interval, an enum); only a declared text type is one.
            if is_text_affinity(decl) => {
                return oid;
            }
        _ => {}
    }
    Oid::UNSPECIFIED
}

/// The array of a scalar parameter type, for `= ANY($n)`.
fn array_param_oid(elem: Oid) -> Oid {
    match elem {
        OID_TEXT | OID_VARCHAR | OID_BPCHAR => OID_TEXT_ARR,
        OID_INT8 => OID_INT8_ARR,
        OID_UUID => OID_UUID_ARR,
        _ => Oid::UNSPECIFIED,
    }
}

/// What the text readers find in sql, as the tree's facts state it: the INSERT's
/// table and each parameter's slot, and the row counts. For the differential test of
/// parameter typing (ast/diff.rs).
#[cfg(test)]
pub(crate) fn text_slots(sql: &str) -> (String, Vec<(usize, crate::ast::params::Slot)>, Vec<usize>) {
    use crate::ast::params::Slot;
    let code = blank_literals(sql);
    let (table, cols, _) = insert_columns(&code);
    let mut slots: Vec<(usize, Slot)> = cols.into_iter().map(|(p, c)| (p as usize, Slot::Column(c))).collect();
    if let Some(positions) = insert_positions(&code) {
        slots.extend(positions.into_iter().map(|(p, k)| (p as usize, Slot::Position(k))));
    }
    slots.sort_by_key(|(p, _)| *p);
    let mut counts: Vec<usize> = LIMIT_OFFSET.captures_iter(sql).map(|m| atoi(&m[1]) as usize).collect();
    counts.sort_unstable();
    counts.dedup();
    (table, slots, counts)
}

/// The column each parameter re matches meets, by number.
fn compared_params(sql: &str, re: &Regex) -> HashMap<i64, String> {
    let mut cols = HashMap::new();
    for m in re.captures_iter(sql) {
        cols.insert(atoi(&m[2]), unquote(&m[1]));
    }
    cols
}

/// The table an INSERT writes, and the column each parameter goes into, by its
/// number, from its column list. An empty table name for any other statement, whose
/// parameters type_by_columns reads.
fn insert_columns(sql: &str) -> (String, HashMap<i64, String>, HashMap<i64, String>) {
    let mut cols = HashMap::new();
    if let Some(m) = INSERT_INTO.captures(sql) {
        let tbl = m[1].to_string();
        let names = split_top(&m[2]);
        for tuple in tuples(&sql[m.get_match().end()..]) {
            for (i, item) in split_top(&tuple).iter().enumerate() {
                if let Some(p) = PLACEHOLDER.captures(item.trim())
                    && i < names.len()
                {
                    cols.insert(atoi(&p[1]), unquote(&names[i]));
                }
            }
        }
        return (tbl, cols, HashMap::new());
    }
    if let Some(m) = INSERT_BARE.captures(sql) {
        return (m[1].to_string(), cols, HashMap::new()); // filled by position, in param_types
    }
    (String::new(), HashMap::new(), HashMap::new())
}

/// For an INSERT with no column list, the position in the row of each parameter
/// that is a whole value, by its number.
fn insert_positions(sql: &str) -> Option<HashMap<i64, usize>> {
    let m = INSERT_BARE.find(sql)?;
    let mut out = HashMap::new();
    for tuple in tuples(&sql[m.end()..]) {
        for (i, item) in split_top(&tuple).iter().enumerate() {
            if let Some(p) = PLACEHOLDER.captures(item.trim()) {
                out.insert(atoi(&p[1]), i);
            }
        }
    }
    Some(out)
}

impl Session {
    /// A table's column names, lowercase, in declaration order.
    fn column_order(&mut self, tbl: &str) -> Vec<String> {
        let name = self.stored_name(tbl);
        let Ok(rs) = self.exec(
            &("SELECT name FROM pragma_table_info(".to_string() + &quote_literal(&name) + ") ORDER BY cid"),
            &[],
        ) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(rs.rows.len());
        for row in &rs.rows {
            if row.len() == 1 {
                out.push(text_or_empty(&row[0]).to_lowercase());
            }
        }
        out
    }

    /// A table's declared column types, by lowercase name.
    fn column_types(&mut self, tbl: &str) -> HashMap<String, String> {
        let name = self.stored_name(tbl);
        let Ok(rs) = self.exec(
            &("SELECT name, type FROM pragma_table_info(".to_string() + &quote_literal(&name) + ")"),
            &[],
        ) else {
            return HashMap::new();
        };
        let mut out = HashMap::new();
        for row in &rs.rows {
            if row.len() == 2 {
                out.insert(text_or_empty(&row[0]).to_lowercase(), text_or_empty(&row[1]));
            }
        }
        out
    }
}

pub(crate) fn unquote(name: &str) -> String {
    let name = name.trim();
    if name.len() >= 2 && name.starts_with('"') && name.ends_with('"') {
        return name[1..name.len() - 1].replace("\"\"", "\"");
    }
    name.to_string()
}

/// The parenthesised rows a VALUES list starts with: their insides.
fn tuples(s: &str) -> Vec<String> {
    let mut s = s;
    let mut out = Vec::new();
    loop {
        s = trim_start_chars(s, " \t\r\n,");
        if !s.starts_with('(') {
            return out;
        }
        let Some(end) = closing(s) else { return out };
        out.push(s[1..end].to_string());
        s = &s[end + 1..];
    }
}

/// The index of the parenthesis closing the one s starts with.
pub(crate) fn closing(s: &str) -> Option<usize> {
    let mut depth = 0;
    let mut quoted = false;
    for (i, c) in s.bytes().enumerate() {
        if c == b'\'' {
            quoted = !quoted;
        } else if quoted {
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Splits on the commas outside parentheses and quotes.
pub(crate) fn split_top(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut quoted = false;
    let mut start = 0;
    for (i, c) in s.bytes().enumerate() {
        if c == b'\'' {
            quoted = !quoted;
        } else if quoted {
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
        } else if c == b',' && depth == 0 {
            out.push(s[start..i].to_string());
            start = i + 1;
        }
    }
    out.push(s[start..].to_string());
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::{OID_BYTEA, OID_JSONB};

    fn map(entries: &[(i64, &str)]) -> HashMap<i64, String> {
        entries.iter().map(|(n, c)| (*n, c.to_string())).collect()
    }

    #[test]
    fn insert_columns_cases() {
        let (tbl, cols, _) = insert_columns(
            r#"INSERT INTO "public"."c" ("id","on",n) VALUES ($1,$2,coalesce($3, 'a,b')),($4,$5,$6) ON CONFLICT ("id") DO UPDATE SET "on"="excluded"."on" RETURNING "id""#,
        );
        assert_eq!(tbl, r#""public"."c""#);
        assert_eq!(cols, map(&[(1, "id"), (2, "on"), (4, "id"), (5, "on"), (6, "n")]));
        let (tbl, _, _) = insert_columns(r#"UPDATE "c" SET "on"=$1 WHERE "c"."id" = $2"#);
        assert!(tbl.is_empty(), "type_by_columns reads the rest");
    }

    #[test]
    fn qualifiers() {
        let (tables, refs) = statement_params(
            r#"UPDATE "_prisma_migrations" SET "applied_steps_count" = ("applied_steps_count" + $1) WHERE "id" = $2 OR "id" = ANY ( $3 )"#,
        );
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].text, r#""_prisma_migrations""#);
        let got: Vec<(usize, &str, Meets)> = refs.iter().map(|r| (r.param, r.column.as_str(), r.meets)).collect();
        assert_eq!(
            got,
            [
                (1, "applied_steps_count", Meets::Value),
                (2, "id", Meets::Value),
                (3, "id", Meets::Array)
            ]
        );
        let (tables, _) = statement_params(r#"SELECT 1 FROM "public"."c" AS "t1" JOIN d ON d.id = "t1".id"#);
        assert!(qualifies(&tables[0], &["t1".to_string()]));
        assert!(!qualifies(&tables[0], &["c".to_string()]), "an alias hides the name");
        assert!(qualifies(&tables[1], &["d".to_string()]));
        assert!(!qualifies(&tables[1], &["public".to_string(), "d".to_string()]));
    }

    #[test]
    fn param_oid_cases() {
        for (decl, oid) in [
            ("TEXT", OID_TEXT),
            ("VARCHAR(36)", OID_VARCHAR),
            ("INTEGER", OID_INT8),
            ("bigint", OID_INT8),
            ("BOOLEAN", OID_BOOL),
            ("TIMESTAMPTZ", OID_TIMESTAMPTZ),
            ("TIMESTAMP(3)", OID_TIMESTAMP),
            ("DATE", OID_DATE),
            ("DOUBLE PRECISION", OID_FLOAT8),
            ("UUID", OID_UUID),
            ("JSONB", OID_JSONB),
            // Types whose binary form is not read back stay unspecified.
            ("", Oid::UNSPECIFIED),
            ("text[]", Oid::UNSPECIFIED),
            ("NUMERIC(10,2) DECIMALTEXT", Oid::UNSPECIFIED),
            ("BYTEA", OID_BYTEA),
            ("hstore", Oid::UNSPECIFIED),
            ("INTERVAL", Oid::UNSPECIFIED),
            ("POINT", Oid::UNSPECIFIED),
            ("mood", Oid::UNSPECIFIED),
        ] {
            assert_eq!(param_oid(decl), oid, "{decl}");
        }
    }
}
