//! DEFAULT nextval('seq') can't be a SQLite column default (non-constant). On
//! CREATE TABLE we record the (table, column, sequence) and strip the default;
//! on INSERT that omits such a column we inject nextval('seq') (which the
//! sequence-expansion pass then turns into the next integer).

use crate::auth::grants::index_word;
use crate::auth::rls::{balanced_paren, bare_table_name, parse_insert, text_or_empty, top_level_keyword};
use crate::ddl::alter::{drop_default, join_def, parse_col_def, split_qualified_ref};
use crate::ddl::assign::insert_source_for_sqlite;
use crate::ddl::tree::{self, code_tokens};
use crate::session::Session;
use crate::session::utility::{first_word_upper, second_word_upper, unquote_ident};
use overlite_core::text::end_of_string_literal;
use overlite_core::text::{eq_fold, quote_literal, slice};
use overlite_engine::stored_table_name;
use pg_query::NodeEnum;
use std::collections::HashSet;
use std::ops::Range;

/// A CREATE TABLE's column list as its node and Postgres' scanner read it: the table
/// as a reference, where the list's parentheses are, and each item with what the
/// session's rewrites of a column ask of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TablePlan {
    pub(crate) table: String,
    /// The list's opening and closing parentheses, as byte offsets.
    pub(crate) open: usize,
    pub(crate) close: usize,
    /// Each item of the list, in order: its text's extent between the commas, and
    /// what it is when it is a column.
    pub(crate) items: Vec<(Range<usize>, Option<ColumnPlan>)>,
}

/// A column of a CREATE TABLE, read from its ColumnDef.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnPlan {
    pub(crate) name: String,
    /// The last name of its type, in lower case, with no modifier or array bounds.
    pub(crate) type_name: Option<String>,
    /// PRIMARY KEY among its constraints.
    pub(crate) key: bool,
    /// GENERATED ... AS IDENTITY among its constraints.
    pub(crate) identity: bool,
    /// The sequence its DEFAULT nextval('seq') names, without a schema.
    pub(crate) nextval: Option<String>,
}

/// The sequence a DEFAULT's nextval('s'[::regclass]) names, its last part.
fn nextval_of(expr: &pg_query::protobuf::Node) -> Option<String> {
    let Some(NodeEnum::FuncCall(f)) = &expr.node else {
        return None;
    };
    if tree::strings(&f.funcname).last().map(String::as_str) != Some("nextval") {
        return None;
    }
    let arg = match &f.args.first()?.node {
        Some(NodeEnum::TypeCast(c)) => c.arg.as_deref()?,
        _ => f.args.first()?,
    };
    let name = tree::arg_text(arg)?;
    let name = name.rsplit('.').next().unwrap_or(&name);
    let name = name.strip_prefix('"').and_then(|n| n.strip_suffix('"')).unwrap_or(name);
    Some(name.to_string())
}

/// The sequence a default expression (as written) takes nextval of, without a schema.
pub(crate) fn nextval_in(expr: &str) -> Option<String> {
    tree::read_any(&["SELECT ", expr].concat(), |n| match n {
        NodeEnum::SelectStmt(s) => match s.target_list.as_slice() {
            [t] => match &t.node {
                Some(NodeEnum::ResTarget(r)) => nextval_of(r.val.as_deref()?),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    })
}

/// sql's column plan, when it is a CREATE TABLE with a column list.
pub(crate) fn table_plan(sql: &str) -> Option<TablePlan> {
    use pg_query::protobuf::ConstrType;
    let create = tree::read(sql, |n| match n {
        NodeEnum::CreateStmt(c) => Some(c.clone()),
        _ => None,
    })?;
    let table = tree::reference(create.relation.as_ref()?);
    let toks = code_tokens(sql);
    let open = toks.iter().position(|t| t.depth == 0 && t.is_punct(sql, "("))?;
    let close = (open + 1..toks.len()).find(|&k| toks[k].depth == 0 && toks[k].is_punct(sql, ")"))?;
    let mut cuts = vec![toks[open].span.end];
    for t in &toks[open + 1..close] {
        if t.depth == 1 && t.is_punct(sql, ",") {
            cuts.push(t.span.start);
            cuts.push(t.span.end);
        }
    }
    cuts.push(toks[close].span.start);
    let columns: Vec<(usize, ColumnPlan)> = create
        .table_elts
        .iter()
        .filter_map(|e| match &e.node {
            Some(NodeEnum::ColumnDef(d)) => {
                let constraints: Vec<&pg_query::protobuf::Constraint> = d
                    .constraints
                    .iter()
                    .filter_map(|c| match &c.node {
                        Some(NodeEnum::Constraint(k)) => Some(&**k),
                        _ => None,
                    })
                    .collect();
                let is = |t: ConstrType| constraints.iter().any(|k| k.contype == t as i32);
                let type_name = d
                    .type_name
                    .as_ref()
                    .filter(|t| t.typmods.is_empty() && t.array_bounds.is_empty())
                    .and_then(|t| tree::strings(&t.names).pop())
                    .map(|t| t.to_ascii_lowercase());
                Some((
                    usize::try_from(d.location).ok()?,
                    ColumnPlan {
                        name: d.colname.clone(),
                        type_name,
                        key: is(ConstrType::ConstrPrimary),
                        identity: is(ConstrType::ConstrIdentity),
                        nextval: constraints
                            .iter()
                            .filter(|k| k.contype == ConstrType::ConstrDefault as i32)
                            .find_map(|k| nextval_of(k.raw_expr.as_deref()?)),
                    },
                ))
            }
            _ => None,
        })
        .collect();
    let items = cuts
        .chunks(2)
        .map(|c| {
            let span = c[0]..c[1];
            let column = columns.iter().find(|(at, _)| span.contains(at)).map(|(_, p)| p.clone());
            (span, column)
        })
        .collect();
    Some(TablePlan {
        table,
        open: toks[open].span.start,
        close: toks[close].span.start,
        items,
    })
}

impl TablePlan {
    /// sql with each item's text given by f (None keeps it).
    pub(crate) fn rewrite(&self, sql: &str, mut f: impl FnMut(&str, &ColumnPlan) -> Option<String>) -> Option<String> {
        let mut changed = false;
        let items: Vec<String> = self
            .items
            .iter()
            .map(|(span, column)| {
                let text = &sql[span.clone()];
                match column.as_ref().and_then(|c| f(text, c)) {
                    Some(new) => {
                        changed = true;
                        new
                    }
                    None => text.to_string(),
                }
            })
            .collect();
        changed.then(|| [&sql[..=self.open], &items.join(","), &sql[self.close..]].concat())
    }
}

impl Session {
    /// Records/strips nextval defaults on CREATE TABLE and injects them on INSERT,
    /// returning the possibly-modified statement.
    pub(crate) fn apply_column_defaults(&mut self, sql: &str) -> String {
        match first_word_upper(sql).as_str() {
            "CREATE" => {
                if second_word_upper(sql) == "TABLE" {
                    let serial = self.create_serial_columns(sql);
                    let sql = self.create_identity_columns(&serial);
                    if sql.to_lowercase().contains("nextval") {
                        return self.record_nextval_defaults(&sql);
                    }
                    return sql.into_owned();
                }
            }
            "INSERT" => {
                let sql = insert_source_for_sqlite(sql);
                let sql = self.inject_nextval_defaults(&sql);
                return self.fit_written_values(&sql);
            }
            "UPDATE" | "WITH" => return self.fit_written_values(sql),
            _ => {}
        }
        sql.to_string()
    }

    /// A SERIAL column that is not the key, as Postgres makes one: an integer NOT NULL
    /// whose default is the next value of the sequence `<table>_<column>_seq`, made here.
    /// A key SERIAL is SQLite's rowid (rewrite_serial).
    fn create_serial_columns(&mut self, sql: &str) -> String {
        if !sql.to_ascii_lowercase().contains("serial") {
            return sql.to_string();
        }
        let Some(plan) = table_plan(sql) else {
            return sql.to_string();
        };
        let stored = self.created_table(&plan.table);
        let (_, relation) = overlite_engine::split_stored_name(&stored);
        plan.rewrite(sql, |d, column| {
            let int = match column.type_name.as_deref()? {
                "serial" | "serial4" => "integer",
                "bigserial" | "serial8" => "bigint",
                "smallserial" | "serial2" => "smallint",
                _ => return None,
            };
            if column.key {
                return None;
            }
            let (name, _, cons) = parse_col_def(d);
            let seq = [relation.as_str(), "_", &column.name, "_seq"].concat();
            let _ = self.create_sequence(&seq, true, &crate::ddl::sequences::default_seq_options());
            // The mark says the sequence is the column's own, as pg_depend and
            // pg_get_serial_sequence read it; a default on any other sequence is no serial.
            let cons = [
                cons.trim(),
                " NOT NULL /*overlite owned sequence*/ DEFAULT nextval(",
                &quote_literal(&seq),
                ")",
            ]
            .concat();
            Some(" ".to_string() + &join_def(&name, int, cons.trim()) + " ")
        })
        .unwrap_or_else(|| sql.to_string())
    }

    /// Records each column's nextval default and removes the DEFAULT clause from
    /// the CREATE TABLE.
    fn record_nextval_defaults(&mut self, sql: &str) -> String {
        let Some(plan) = table_plan(sql) else {
            return sql.to_string();
        };
        let table = self.created_table(&plan.table);
        plan.rewrite(sql, |d, column| {
            let seq = column.nextval.as_ref()?;
            // Once: CREATE TABLE IF NOT EXISTS may run again and create nothing.
            let (t, col) = (quote_literal(&table), quote_literal(&column.name));
            let _ = self.exec(
                &format!(
                    "INSERT INTO _overlite_defaults (tablename, colname, seqname) SELECT {t}, {col}, {} \
                     WHERE NOT EXISTS (SELECT 1 FROM _overlite_defaults WHERE tablename = {t} AND colname = {col})",
                    quote_literal(seq)
                ),
                &[],
            );
            let (name, typ, cons) = parse_col_def(d);
            Some(" ".to_string() + &join_def(&name, &typ, &drop_default(&cons)) + " ")
        })
        .unwrap_or_else(|| sql.to_string())
    }

    /// The stored name CREATE TABLE gives a reference, before the table exists to be
    /// found: its schema if it names one, or the first schema of the search_path, as
    /// the engine resolves it.
    pub(crate) fn created_table(&mut self, reference: &str) -> String {
        let (schema, table) = split_qualified_ref(reference);
        if !schema.is_empty() {
            return stored_table_name(&schema, &table);
        }
        for schema in self.search_path.clone().unwrap_or_default() {
            if eq_fold(&schema, "public") || schema == "$user" {
                continue;
            }
            let q = "SELECT 1 FROM _overlite_schemas WHERE lower(name) = lower(".to_string()
                + &quote_literal(&schema)
                + ")";
            if self.exec(&q, &[]).is_ok_and(|rs| !rs.rows.is_empty()) {
                return stored_table_name(&schema, &table);
            }
        }
        table
    }
}

impl Session {
    /// Adds nextval('seq') for any nextval-default column that an INSERT's explicit
    /// column list omits.
    fn inject_nextval_defaults(&mut self, sql: &str) -> String {
        let (target, cols, src_start) = parse_insert(sql);
        if target.is_empty() || src_start < 0 {
            return sql.to_string();
        }
        let src_start = src_start as usize;
        // DEFAULT VALUES: the columns with a sequence get its next value.
        let default_values = cols.is_empty() && {
            let toks = code_tokens(&sql[src_start..]);
            toks.len() >= 2
                && toks[0].is_word(&sql[src_start..], "default")
                && toks[1].is_word(&sql[src_start..], "values")
        };
        if cols.is_empty() && !default_values {
            return sql.to_string(); // no explicit column list -> all columns supplied
        }
        if bare_table_name(&target).is_empty() {
            return sql.to_string(); // the catalogs have no defaults of this kind
        }
        let stored = self.stored_name(&target);
        let rs = match self.exec(
            &("SELECT colname, seqname FROM _overlite_defaults WHERE lower(tablename)=lower(".to_string()
                + &quote_literal(&stored)
                + ")"),
            &[],
        ) {
            Ok(rs) if !rs.rows.is_empty() => rs,
            _ => return sql.to_string(),
        };
        let mut have: HashSet<String> = HashSet::new();
        for c in &cols {
            have.insert(unquote_ident(c).to_lowercase());
        }
        let mut add_cols: Vec<String> = Vec::new();
        let mut add_vals: Vec<String> = Vec::new();
        for row in &rs.rows {
            let col = text_or_empty(&row[0]);
            if col.is_empty() || have.contains(&col.to_lowercase()) {
                continue;
            }
            add_vals.push("nextval('".to_string() + &text_or_empty(&row[1]) + "')");
            add_cols.push(col);
        }
        if add_cols.is_empty() {
            return sql.to_string();
        }
        if default_values {
            let rest = &sql[src_start..];
            let toks = code_tokens(rest);
            return "INSERT INTO ".to_string()
                + &target
                + " ("
                + &add_cols.join(", ")
                + ") VALUES ("
                + &add_vals.join(", ")
                + ")"
                + &rest[toks[1].span.end..];
        }
        let source = sql[src_start..].trim();
        if !source.to_uppercase().starts_with("VALUES") {
            return sql.to_string(); // only the VALUES form is handled
        }
        let Some(new_values) = inject_into_tuples(&sql[src_start..], &add_vals) else {
            return sql.to_string();
        };
        let mut new_cols = cols.clone();
        new_cols.extend(add_cols);
        "INSERT INTO ".to_string() + &target + " (" + &new_cols.join(", ") + ") " + &new_values
    }
}

/// Appends the given values to each top-level (...) tuple in a VALUES clause,
/// leaving any trailing clause (RETURNING ...) untouched.
fn inject_into_tuples(values_clause: &str, add: &[String]) -> Option<String> {
    let v = index_word(&values_clause.to_ascii_lowercase(), "values")?;
    let head = &values_clause[..v + "values".len()];
    let mut rest = &values_clause[v + "values".len()..];
    let mut suffix = String::new();
    if let Some(r) = top_level_keyword(rest, "returning") {
        suffix = " ".to_string() + &rest[r..];
        rest = &rest[..r];
    }
    let mut b = String::with_capacity(values_clause.len());
    b.push_str(head);
    let added = ", ".to_string() + &add.join(", ");
    let rb = rest.as_bytes();
    let mut i = 0;
    let mut injected = false;
    while i < rb.len() {
        let c = rb[i];
        if c == b'\'' {
            let j = end_of_string_literal(rest, i);
            b.push_str(&rest[i..j]);
            i = j;
            continue;
        }
        if c == b'(' {
            let (_, e) = balanced_paren(rest, i);
            b.push_str(slice(rest, i, e - 1)); // the tuple minus its closing ")"
            b.push_str(&added);
            b.push(')');
            i = e;
            injected = true;
            continue;
        }
        let Some(ch) = rest[i..].chars().next() else { break };
        b.push(ch);
        i += ch.len_utf8();
    }
    if !injected {
        return None;
    }
    Some(b + &suffix)
}
