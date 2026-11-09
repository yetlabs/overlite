//! Identity columns: `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [ (options) ]`,
//! which Django makes every AutoField since 4.1. SQLite has none.
//!
//! A key column of an integer type with no options becomes SQLite's AUTOINCREMENT
//! rowid, as a serial key does, so that every way of inserting a row gives it its
//! id. Any other is a sequence of its own, `<table>_<column>_seq`, its column's
//! nextval default. Either way the column's definition ends in a comment,
//! `/*overlite identity by default bigint*/`, which says what it is and the type it
//! was declared with (a rowid is INTEGER whatever it was), and which SQLite keeps
//! with the table through renames and rebuilds. The catalog reads it back
//! (`overlite_identity::identity_columns`).

use crate::auth::rls::{parse_insert, text_or_empty};
use crate::ddl::alter::{join_def, parse_col_def};
use crate::ddl::sequences::split_top_level;
use crate::ddl::tree::{CodeTokKind, code_tokens};
use crate::error::{Result, errf};
use crate::session::Session;
use crate::session::utility::{first_word_upper, unquote_ident};
use overlite_core::SqlState;
use overlite_core::text::{eq_fold, quote_ident, quote_literal};
use overlite_engine::{Identity, identity_columns, split_stored_name};
use std::borrow::Cow;
use std::ops::Range;

/// A GENERATED ... AS IDENTITY clause of a column definition.
#[derive(Debug, PartialEq)]
pub(crate) struct IdentityClause {
    /// Where it is in the constraints it was read from.
    span: Range<usize>,
    always: bool,
    /// The sequence options inside its parentheses, as written.
    options: String,
}

/// The identity clause among a column's constraints.
pub(crate) fn identity_clause(cons: &str) -> Option<IdentityClause> {
    let toks = code_tokens(cons);
    let word = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(cons, w));
    let at = (0..toks.len()).find(|&i| toks[i].depth == 0 && word(i, "generated"))?;
    let (always, mut i) = if word(at + 1, "always") {
        (true, at + 2)
    } else if word(at + 1, "by") && word(at + 2, "default") {
        (false, at + 3)
    } else {
        return None;
    };
    if !(word(i, "as") && word(i + 1, "identity")) {
        return None;
    }
    i += 2;
    let mut end = toks[i - 1].span.end;
    let mut options = String::new();
    if toks.get(i).is_some_and(|t| t.is_punct(cons, "(")) {
        let close = (i + 1..toks.len()).find(|&k| toks[k].depth == toks[i].depth && toks[k].is_punct(cons, ")"))?;
        options = cons[toks[i].span.end..toks[close].span.start].trim().to_string();
        end = toks[close].span.end;
    }
    Some(IdentityClause {
        span: toks[at].span.start..end,
        always,
        options,
    })
}

/// The comment that marks an identity column.
fn mark(always: bool, type_name: &str) -> String {
    let kind = if always { "always" } else { "by default" };
    format!("/*overlite identity {kind} {type_name}*/")
}

/// The constraints without the identity mark.
fn without_mark(cons: &str) -> String {
    match cons.find("/*overlite identity") {
        Some(at) => {
            let end = cons[at..].find("*/").map_or(cons.len(), |e| at + e + 2);
            [cons[..at].trim_end(), cons[end..].trim_start()]
                .join(" ")
                .trim()
                .to_string()
        }
        None => cons.to_string(),
    }
}

/// The name Postgres gives an integer type, or None for any other.
fn integer_type(typ: &str) -> Option<&'static str> {
    match typ.trim().to_ascii_lowercase().as_str() {
        "int" | "integer" | "int4" | "serial" | "serial4" => Some("integer"),
        "bigint" | "int8" | "bigserial" | "serial8" => Some("bigint"),
        "smallint" | "int2" | "smallserial" | "serial2" => Some("smallint"),
        _ => None,
    }
}

/// Whether the constraints declare the column the table's key.
fn is_key(cons: &str) -> bool {
    let toks = code_tokens(cons);
    toks.windows(2)
        .any(|w| w[0].depth == 0 && w[0].is_word(cons, "primary") && w[1].is_word(cons, "key"))
}

/// AUTOINCREMENT after the PRIMARY KEY [ASC | DESC] of a column's constraints.
fn with_autoincrement(cons: &str) -> String {
    let toks = code_tokens(cons);
    let Some(k) = (1..toks.len()).find(|&i| toks[i - 1].is_word(cons, "primary") && toks[i].is_word(cons, "key"))
    else {
        return cons.to_string();
    };
    let mut at = toks[k].span.end;
    if toks
        .get(k + 1)
        .is_some_and(|t| t.is_word(cons, "asc") || t.is_word(cons, "desc"))
    {
        at = toks[k + 1].span.end;
    }
    if toks.iter().any(|t| t.is_word(cons, "autoincrement")) {
        return cons.to_string();
    }
    [&cons[..at], " AUTOINCREMENT", &cons[at..]].concat()
}

/// The constraints without AUTOINCREMENT.
fn without_autoincrement(cons: &str) -> String {
    let toks = code_tokens(cons);
    match toks
        .iter()
        .find(|t| t.kind == CodeTokKind::Word && t.is_word(cons, "autoincrement"))
    {
        Some(t) => [cons[..t.span.start].trim_end(), cons[t.span.end..].trim_start()]
            .join(" ")
            .trim()
            .to_string(),
        None => cons.to_string(),
    }
}

/// NOT NULL among the constraints, once.
fn with_not_null(cons: &str) -> String {
    let toks = code_tokens(cons);
    if toks
        .windows(2)
        .any(|w| w[0].depth == 0 && w[0].is_word(cons, "not") && w[1].is_word(cons, "null"))
    {
        return cons.to_string();
    }
    (cons.to_string() + " NOT NULL").trim().to_string()
}

impl Session {
    /// Turns the identity columns of a CREATE TABLE into what SQLite can hold,
    /// making the sequences of those that need one.
    pub(crate) fn create_identity_columns<'a>(&mut self, sql: &'a str) -> Cow<'a, str> {
        if !sql.to_ascii_lowercase().contains("identity") {
            return Cow::Borrowed(sql);
        }
        let Some(plan) = crate::ddl::defaults::table_plan(sql) else {
            return Cow::Borrowed(sql);
        };
        if !plan.items.iter().any(|(_, c)| c.as_ref().is_some_and(|c| c.identity)) {
            return Cow::Borrowed(sql);
        }
        let stored = self.created_table(&plan.table);
        let mut failed = false;
        let out = plan.rewrite(sql, |d, column| {
            if !column.identity || failed {
                return None;
            }
            let (name, typ, cons) = parse_col_def(d);
            let clause = identity_clause(&cons)?;
            let column = column.name.clone();
            let rest = [
                cons[..clause.span.start].trim_end(),
                cons[clause.span.end..].trim_start(),
            ]
            .join(" ")
            .trim()
            .to_string();
            let type_name = integer_type(&typ).map_or_else(|| typ.to_ascii_lowercase(), str::to_string);
            let def = if integer_type(&typ).is_some() && is_key(&rest) && clause.options.is_empty() {
                // The rowid, as a serial key is: a statement Postgres' grammar reads, which
                // the translation writes as INTEGER ... AUTOINCREMENT.
                join_def(&name, "serial", &(rest + " " + &mark(clause.always, &type_name)))
            } else {
                if self.identity_sequence(&stored, &column, &clause.options, None).is_err() {
                    failed = true;
                    return None;
                }
                join_def(
                    &name,
                    &typ,
                    &(with_not_null(&rest) + " " + &mark(clause.always, &type_name)),
                )
            };
            Some(" ".to_string() + &def + " ")
        });
        match out {
            Some(out) if !failed => Cow::Owned(out),
            _ => Cow::Borrowed(sql),
        }
    }

    /// Makes `<table>_<column>_seq` with the options given, starting past `after` when
    /// that is given, and makes it the column's default.
    fn identity_sequence(&mut self, stored: &str, column: &str, options: &str, after: Option<i64>) -> Result<()> {
        let (_, relation) = split_stored_name(stored);
        let seq = relation + "_" + column + "_seq";
        let mut opts = crate::ddl::sequences::sequence_options_text(options);
        if let Some(after) = after.filter(|_| !opts.start_set) {
            opts.start = after + 1;
            opts.start_set = true;
        }
        self.create_sequence(&seq, false, &opts)?;
        let (t, c) = (quote_literal(stored), quote_literal(column));
        self.exec(
            &format!(
                "INSERT INTO _overlite_defaults (tablename, colname, seqname) SELECT {t}, {c}, {} \
                 WHERE NOT EXISTS (SELECT 1 FROM _overlite_defaults WHERE tablename = {t} AND colname = {c})",
                quote_literal(&seq)
            ),
            &[],
        )?;
        Ok(())
    }

    /// The identity column of table called col, if it is one.
    pub(crate) fn identity_of(&mut self, table: &str, col: &str) -> Option<Identity> {
        identity_columns(&self.table_ddl(table))
            .into_iter()
            .find(|i| eq_fold(&i.column, col))
    }

    /// ALTER COLUMN col ADD GENERATED ... AS IDENTITY [(options)].
    pub(crate) fn alter_add_identity(&mut self, table: &str, col: &str, action: &str) -> Result<()> {
        let Some(clause) = identity_clause(action) else {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in ADD GENERATED"));
        };
        let (_, relation) = split_stored_name(table);
        if self.identity_of(table, col).is_some() {
            return Err(errf!(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "column \"{col}\" of relation \"{relation}\" is already an identity column"
            ));
        }
        let ddl = self.table_ddl(table);
        let Some((name, typ, cons)) = column_def(&ddl, col) else {
            return Err(no_column(col, &relation));
        };
        let type_name = integer_type(&typ).map_or_else(|| typ.to_ascii_lowercase(), str::to_string);
        let new = if integer_type(&typ).is_some() && is_key(&cons) && clause.options.is_empty() {
            join_def(
                &name,
                "INTEGER",
                &(with_autoincrement(&cons) + " " + &mark(clause.always, &type_name)),
            )
        } else {
            // It goes on past the rows there are, as a key that is a rowid does.
            let q = "SELECT max(".to_string() + &quote_ident(col) + ") FROM " + &quote_ident(table);
            let max = self
                .exec(&q, &[])?
                .rows
                .first()
                .map_or(0, |r| crate::types::to_int64(&r[0]));
            self.identity_sequence(table, col, &clause.options, Some(max))?;
            join_def(
                &name,
                &typ,
                &(with_not_null(&cons) + " " + &mark(clause.always, &type_name)),
            )
        };
        self.rebuild_with_column(table, &ddl, col, &new)
    }

    /// ALTER COLUMN col DROP IDENTITY [IF EXISTS].
    pub(crate) fn alter_drop_identity(&mut self, table: &str, col: &str, if_exists: bool) -> Result<()> {
        let (_, relation) = split_stored_name(table);
        let Some(identity) = self.identity_of(table, col) else {
            if if_exists {
                self.c.send_notice(&format!(
                    "column \"{col}\" of relation \"{relation}\" is not an identity column, skipping"
                ))?;
                return Ok(());
            }
            return Err(not_identity(col, &relation));
        };
        let ddl = self.table_ddl(table);
        let Some((name, typ, cons)) = column_def(&ddl, col) else {
            return Err(no_column(col, &relation));
        };
        let cons = without_mark(&cons);
        let new = if eq_fold(&typ, "integer") && cons.to_ascii_lowercase().contains("autoincrement") {
            // A rowid would go on giving ids: the column takes a type that is not one.
            let typ = match identity.type_name.as_str() {
                "integer" => "int",
                other => other,
            };
            join_def(&name, typ, &without_autoincrement(&cons))
        } else {
            let seq = "DELETE FROM _overlite_sequences WHERE seqname IN (SELECT seqname FROM _overlite_defaults \
                       WHERE lower(tablename) = lower("
                .to_string()
                + &quote_literal(table)
                + ") AND lower(colname) = lower("
                + &quote_literal(col)
                + "))";
            self.exec(&seq, &[])?;
            let row = "DELETE FROM _overlite_defaults WHERE lower(tablename) = lower(".to_string()
                + &quote_literal(table)
                + ") AND lower(colname) = lower("
                + &quote_literal(col)
                + ")";
            self.exec(&row, &[])?;
            join_def(&name, &typ, &cons)
        };
        self.rebuild_with_column(table, &ddl, col, &new)
    }

    /// ALTER COLUMN col SET GENERATED { ALWAYS | BY DEFAULT }.
    pub(crate) fn alter_set_generated(&mut self, table: &str, col: &str, always: bool) -> Result<()> {
        let (_, relation) = split_stored_name(table);
        let Some(identity) = self.identity_of(table, col) else {
            return Err(not_identity(col, &relation));
        };
        let ddl = self.table_ddl(table);
        let Some((name, typ, cons)) = column_def(&ddl, col) else {
            return Err(no_column(col, &relation));
        };
        let new = join_def(
            &name,
            &typ,
            &(without_mark(&cons) + " " + &mark(always, &identity.type_name)),
        );
        self.rebuild_with_column(table, &ddl, col, &new)
    }

    fn rebuild_with_column(&mut self, table: &str, ddl: &str, col: &str, new: &str) -> Result<()> {
        let Some(new_ddl) = replace_column(ddl, col, new) else {
            let (_, relation) = split_stored_name(table);
            return Err(no_column(col, &relation));
        };
        self.rebuild_table(table, &new_ddl)
    }

    /// Holds an INSERT to its GENERATED ALWAYS columns and takes OVERRIDING SYSTEM
    /// VALUE out of it: a value for one is 428C9 without that clause.
    pub(crate) fn identity_insert<'a>(&mut self, sql: &'a str) -> Result<Cow<'a, str>> {
        if !first_word_upper(sql).eq_ignore_ascii_case("INSERT") {
            return Ok(Cow::Borrowed(sql));
        }
        let toks = code_tokens(sql);
        let word = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
        let overriding = (0..toks.len()).find(|&i| {
            toks[i].depth == 0
                && word(i, "overriding")
                && (word(i + 1, "system") || word(i + 1, "user"))
                && word(i + 2, "value")
        });
        let mut out = Cow::Borrowed(sql);
        if let Some(i) = overriding {
            if word(i + 1, "user") {
                return Err(errf!(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "OVERRIDING USER VALUE is not supported"
                ));
            }
            out = Cow::Owned([&sql[..toks[i].span.start], &sql[toks[i + 2].span.end..]].concat());
        }
        let (target, cols, src_start) = parse_insert(&out);
        if target.is_empty() {
            return Ok(out);
        }
        let stored = self.stored_name(&target);
        let q = "SELECT sql FROM main.sqlite_master WHERE type = 'table' AND lower(name) = lower(".to_string()
            + &quote_literal(&stored)
            + ") AND instr(sql, '/*overlite identity always')";
        let Some(row) = self.exec(&q, &[])?.rows.into_iter().next() else {
            return Ok(out);
        };
        if overriding.is_some() {
            return Ok(out);
        }
        let ddl = text_or_empty(&row[0]);
        let default_values = src_start >= 0 && {
            let src = out[src_start as usize..].trim_start();
            src.len() >= 7 && src[..7].eq_ignore_ascii_case("default")
        };
        for identity in identity_columns(&ddl).into_iter().filter(|i| i.always) {
            let given = if cols.is_empty() {
                !default_values
            } else {
                cols.iter().any(|c| eq_fold(&unquote_ident(c), &identity.column))
            };
            if given {
                return Err(errf!(
                    SqlState::GENERATED_ALWAYS,
                    "cannot insert a non-DEFAULT value into column \"{}\"",
                    identity.column
                ));
            }
        }
        Ok(out)
    }
}

/// The definition of col in a CREATE TABLE, as (name, type, constraints).
fn column_def(ddl: &str, col: &str) -> Option<(String, String, String)> {
    let open = ddl.find('(')?;
    let (inner, _) = crate::auth::rls::balanced_paren(ddl, open);
    split_top_level(&inner)
        .iter()
        .map(|d| parse_col_def(d))
        .find(|(name, _, _)| !name.is_empty() && eq_fold(&unquote_ident(name), col))
}

/// The CREATE TABLE with col's definition replaced by new.
fn replace_column(ddl: &str, col: &str, new: &str) -> Option<String> {
    let open = ddl.find('(')?;
    let (inner, after) = crate::auth::rls::balanced_paren(ddl, open);
    let mut defs = split_top_level(&inner);
    let d = defs.iter_mut().find(|d| {
        let (name, _, _) = parse_col_def(d);
        !name.is_empty() && eq_fold(&unquote_ident(&name), col)
    })?;
    *d = " ".to_string() + new + " ";
    Some(ddl[..=open].to_string() + &defs.join(",") + &ddl[after - 1..])
}

/// For ALTER COLUMN TYPE: the new type of an identity column's definition, and its
/// constraints, with the mark saying the type. A key that is a rowid stays INTEGER.
pub(crate) fn retype_identity(typ: &str, cons: &str, new_type: &str) -> Option<(String, String)> {
    let start = cons.find("/*overlite identity ")?;
    let always = cons[start..].starts_with("/*overlite identity always");
    let type_name = integer_type(new_type).map_or_else(|| new_type.to_ascii_lowercase(), str::to_string);
    let cons = without_mark(cons) + " " + &mark(always, &type_name);
    let rowid = eq_fold(typ, "integer") && cons.to_ascii_lowercase().contains("autoincrement");
    let typ = if rowid {
        "INTEGER".to_string()
    } else {
        new_type.to_string()
    };
    Some((typ, cons))
}

fn not_identity(col: &str, relation: &str) -> crate::error::Error {
    errf!(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        "column \"{col}\" of relation \"{relation}\" is not an identity column"
    )
}

fn no_column(col: &str, relation: &str) -> crate::error::Error {
    errf!(
        SqlState::UNDEFINED_COLUMN,
        "column \"{col}\" of relation \"{relation}\" does not exist"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_clauses() {
        let c = identity_clause("NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY").unwrap();
        assert!(!c.always);
        assert_eq!(c.span, 21..53);
        let cons = "GENERATED ALWAYS AS IDENTITY (START WITH 10 INCREMENT BY 2) NOT NULL";
        let c = identity_clause(cons).unwrap();
        assert!(c.always);
        assert_eq!(c.options, "START WITH 10 INCREMENT BY 2");
        assert_eq!(&cons[c.span.end..], " NOT NULL");
        assert_eq!(identity_clause("DEFAULT 'generated always as identity'"), None);
        assert_eq!(
            with_autoincrement("NOT NULL PRIMARY KEY DESC UNIQUE"),
            "NOT NULL PRIMARY KEY DESC AUTOINCREMENT UNIQUE"
        );
        assert_eq!(
            without_mark("NOT NULL /*overlite identity always bigint*/ UNIQUE"),
            "NOT NULL UNIQUE"
        );
    }
}
