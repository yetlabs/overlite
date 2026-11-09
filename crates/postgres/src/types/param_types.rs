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

use crate::ast::params::{Meets, NamedTable, ParamColumn};
use crate::auth::rls::text_or_empty;
use crate::session::Session;
use crate::types::array::{OID_INT8_ARR, OID_TEXT_ARR, OID_UUID_ARR, is_array_decl};
use crate::types::{
    OID_BOOL, OID_BPCHAR, OID_DATE, OID_FLOAT8, OID_INT8, OID_TEXT, OID_TIMESTAMP, OID_TIMESTAMPTZ, OID_UUID,
    OID_VARCHAR, is_text_affinity, oid_from_decl_type, strict_decl_oid,
};
use overlite_core::text::{eq_fold, quote_literal};
use overlite_core::{Oid, lazy_regex};
use std::collections::HashMap;
use std::sync::LazyLock;

lazy_regex!(
    INTEGER_DECL,
    r"(?i)^\s*(?:(?:small|big)?int(?:eger|2|4|8)?|(?:small|big)?serial[248]?)\s*$"
);

/// Whether sql may read the catalog: a word starting pg_, or information_schema.
fn reads_catalog(sql: &str) -> bool {
    let low = sql.to_ascii_lowercase();
    low.contains("information_schema")
        || low
            .match_indices("pg_")
            .any(|(i, _)| i == 0 || !matches!(low.as_bytes()[i - 1], b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$'))
}

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

/// Types each parameter still unspecified that meets a catalog column of a known type
/// (the tree's reading of what the text reader's COMPARED and COMPARED_ANY find): a
/// statement reading the catalog, whose views the type environment does not know.
fn catalog_params(refs: &[ParamColumn], oids: &mut [Oid]) {
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
        // Typed from the tree: by the type environment where it knows what a parameter
        // meets (a column through any scope, an expression, a cast), then by the
        // tree's facts.
        if let Some(facts) = crate::ast::params::facts(sql) {
            let ctx = self.type_context(sql);
            if let Some(tys) = crate::ast::env::with(ctx, || crate::ast::param_types(sql)) {
                for (o, t) in oids.iter_mut().zip(tys) {
                    *o = param_oid(t.decl());
                }
            }
            if reads_catalog(sql) {
                catalog_params(&facts.refs, &mut oids);
            }
            self.type_by_tree(facts, &mut oids);
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
}

impl Session {
    /// Types each parameter still unspecified by the column of these tables it meets.
    fn type_by_refs(&mut self, tables: &[NamedTable], refs: Vec<ParamColumn>, oids: &mut [Oid]) {
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

    #[test]
    fn qualifiers() {
        let (tables, refs) = crate::ast::params::param_columns(
            r#"UPDATE "_prisma_migrations" SET "applied_steps_count" = ("applied_steps_count" + $1) WHERE "id" = $2 OR "id" = ANY ( $3 )"#,
        )
        .unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].text, "_prisma_migrations");
        let got: Vec<(usize, &str, Meets)> = refs.iter().map(|r| (r.param, r.column.as_str(), r.meets)).collect();
        assert_eq!(
            got,
            [
                (1, "applied_steps_count", Meets::Value),
                (2, "id", Meets::Value),
                (3, "id", Meets::Array)
            ]
        );
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
