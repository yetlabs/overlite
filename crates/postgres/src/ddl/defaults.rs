//! DEFAULT nextval('seq') can't be a SQLite column default (non-constant). On
//! CREATE TABLE we record the (table, column, sequence) and strip the default;
//! on INSERT that omits such a column we inject nextval('seq') (which the
//! sequence-expansion pass then turns into the next integer).

use crate::auth::grants::index_word;
use crate::auth::rls::{balanced_paren, bare_table_name, parse_insert, text_or_empty, top_level_keyword};
use crate::ddl::alter::{drop_default, join_def, parse_col_def, split_qualified_ref};
use crate::ddl::assign::insert_source_for_sqlite;
use crate::ddl::sequences::split_top_level;
use crate::rewrite::strings::end_of_string_literal;
use crate::rewrite::tokens::code_tokens;
use crate::session::Session;
use crate::session::utility::{first_word_upper, second_word_upper, unquote_ident};
use overlite_core::text::{eq_fold, quote_literal, slice, split_words};
use overlite_engine::{code_only, stored_table_name};
use std::collections::HashSet;

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
        let table = create_table_name(sql);
        let Some(open) = sql.find('(').filter(|_| !table.is_empty()) else {
            return sql.to_string();
        };
        let stored = self.created_table(&table);
        let (_, relation) = overlite_engine::split_stored_name(&stored);
        let (inner, after) = balanced_paren(sql, open);
        let mut defs = split_top_level(&inner);
        let mut changed = false;
        for d in &mut defs {
            let (name, typ, cons) = parse_col_def(d);
            let int = match typ.to_ascii_lowercase().as_str() {
                "serial" | "serial4" => "integer",
                "bigserial" | "serial8" => "bigint",
                "smallserial" | "serial2" => "smallint",
                _ => continue,
            };
            if name.is_empty() || cons.to_ascii_lowercase().contains("primary") {
                continue;
            }
            let seq = [relation.as_str(), "_", &unquote_ident(&name), "_seq"].concat();
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
            *d = " ".to_string() + &join_def(&name, int, cons.trim()) + " ";
            changed = true;
        }
        if !changed {
            return sql.to_string();
        }
        sql[..=open].to_string() + &defs.join(",") + slice(sql, after - 1, sql.len())
    }

    /// Records each column's nextval default and removes the DEFAULT clause from
    /// the CREATE TABLE.
    fn record_nextval_defaults(&mut self, sql: &str) -> String {
        let table = create_table_name(sql);
        let Some(open) = sql.find('(').filter(|_| !table.is_empty()) else {
            return sql.to_string();
        };
        let table = self.created_table(&table);
        let (inner, after) = balanced_paren(sql, open);
        let mut defs = split_top_level(&inner);
        let mut changed = false;
        for d in &mut defs {
            let (name, typ, cons) = parse_col_def(d);
            if name.is_empty() {
                continue;
            }
            let seq = nextval_seq(&cons);
            if seq.is_empty() {
                continue;
            }
            // Once: CREATE TABLE IF NOT EXISTS may run again and create nothing.
            let (table, col) = (quote_literal(&table), quote_literal(&unquote_ident(&name)));
            let _ = self.exec(
                &format!(
                    "INSERT INTO _overlite_defaults (tablename, colname, seqname) SELECT {table}, {col}, {} \
                     WHERE NOT EXISTS (SELECT 1 FROM _overlite_defaults WHERE tablename = {table} AND colname = {col})",
                    quote_literal(&seq)
                ),
                &[],
            );
            *d = " ".to_string() + &join_def(&name, &typ, &drop_default(&cons)) + " ";
            changed = true;
        }
        if !changed {
            return sql.to_string();
        }
        sql[..=open].to_string() + &defs.join(",") + slice(sql, after - 1, sql.len())
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

/// Extracts the sequence name from a `DEFAULT nextval('seq'[::regclass])`
/// constraint fragment, or "".
fn nextval_seq(cons: &str) -> String {
    let low = code_only(cons).to_ascii_lowercase();
    let Some(n) = index_word(&low, "nextval") else {
        return String::new();
    };
    let Some(op) = cons[n..].find('(') else {
        return String::new();
    };
    let (inner, _) = balanced_paren(cons, n + op);
    let mut inner: &str = inner.trim();
    // strip a ::regclass cast
    if let Some(c) = inner.to_ascii_lowercase().find("::") {
        inner = inner[..c].trim();
    }
    let ib = inner.as_bytes();
    if ib.len() >= 2 && (ib[0] == b'\'' || ib[0] == b'"') {
        inner = slice(inner, 1, ib.len() - 1);
    }
    // a schema-qualified name keeps just the object
    if let Some(d) = inner.rfind('.') {
        inner = &inner[d + 1..];
    }
    inner.to_string()
}

/// The table of a CREATE TABLE as written (before its columns).
pub(crate) fn create_table_name(sql: &str) -> String {
    let low = sql.to_ascii_lowercase();
    let Some(t) = index_word(&low, "table") else {
        return String::new();
    };
    let mut seg = &sql[t + "table".len()..];
    if let Some(op) = seg.find('(') {
        seg = &seg[..op];
    }
    for w in split_words(seg) {
        match w.to_lowercase().as_str() {
            "if" | "not" | "exists" => {}
            _ => return w.to_string(),
        }
    }
    String::new()
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
