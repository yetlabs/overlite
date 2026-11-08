//! COMMENT ON `<objtype> <name>` IS `<'text'|NULL>` records an object comment in
//! _overlite_comments (SQLite has nowhere native to keep it). pg_description
//! reads that table, and obj_description()/col_description() are rewritten to
//! look it up, so psql's \d+ shows table and column comments.
//!
//! The object has to be there, as in Postgres: the kinds this server has a catalog of
//! are looked up, and a kind it has none of is accepted without effect, so a dump
//! proceeds.

use crate::ddl::drop_constraint::has_constraint;
use crate::ddl::words::{Word, ddl_words, syntax_error_at};
use crate::error::{Error, Result};
use crate::session::Session;
use overlite_core::text::{eq_fold, quote_literal, split_words};
use overlite_core::{Command, SqlState};

/// Whether sql is a COMMENT ON statement.
pub(crate) fn is_comment(sql: &str) -> bool {
    let f = split_words(sql);
    f.len() >= 2 && eq_fold(f[0], "comment") && eq_fold(f[1], "on")
}

/// The kinds of object Postgres takes a comment on, by the words that name them. Each
/// is what `_overlite_comments` calls it, or nothing to keep for one with no catalog.
const KINDS: &[(&[&str], Kind)] = &[
    (&["table"], Kind::Relation("table", "table", "rp")),
    (&["foreign", "table"], Kind::Relation("table", "foreign table", "f")),
    (&["view"], Kind::Relation("view", "view", "v")),
    (
        &["materialized", "view"],
        Kind::Relation("view", "materialized view", "m"),
    ),
    (&["index"], Kind::Relation("index", "index", "iI")),
    (&["sequence"], Kind::Relation("sequence", "sequence", "S")),
    (&["column"], Kind::Column),
    (&["type"], Kind::Type),
    (&["domain"], Kind::Type),
    (&["schema"], Kind::Schema),
    (&["role"], Kind::Role),
    (&["constraint"], Kind::Constraint),
    (
        &["trigger"],
        Kind::OnTable("trigger", "pg_trigger", "tgname", "tgrelid"),
    ),
    (&["policy"], Kind::OnTable("policy", "pg_policy", "polname", "polrelid")),
    (&["function"], Kind::Routine),
    (&["procedure"], Kind::Unmodeled),
    (&["routine"], Kind::Unmodeled),
    (&["aggregate"], Kind::Unmodeled),
    (&["database"], Kind::Database),
    (&["extension"], Kind::Unmodeled),
    (&["access", "method"], Kind::Unmodeled),
    (&["cast"], Kind::Unmodeled),
    (&["collation"], Kind::Unmodeled),
    (&["conversion"], Kind::Unmodeled),
    (&["event", "trigger"], Kind::Unmodeled),
    (&["foreign", "data", "wrapper"], Kind::Unmodeled),
    (&["language"], Kind::Unmodeled),
    (&["procedural", "language"], Kind::Unmodeled),
    (&["large", "object"], Kind::Unmodeled),
    (&["operator"], Kind::Unmodeled),
    (&["publication"], Kind::Unmodeled),
    (&["rule"], Kind::Unmodeled),
    (&["server"], Kind::Unmodeled),
    (&["statistics"], Kind::Unmodeled),
    (&["subscription"], Kind::Unmodeled),
    (&["tablespace"], Kind::Unmodeled),
    (&["text", "search"], Kind::Unmodeled),
    (&["transform"], Kind::Unmodeled),
];

/// How a kind of object is found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// What _overlite_comments calls it, what messages call it, and its relkinds.
    Relation(&'static str, &'static str, &'static str),
    Column,
    Type,
    Schema,
    Role,
    Constraint,
    /// What it is called, and its catalog's table, name and relation columns.
    OnTable(&'static str, &'static str, &'static str, &'static str),
    Routine,
    Database,
    /// No catalog here to say whether it exists.
    Unmodeled,
}

/// A statement read: the words before IS, and the comment, None for NULL.
struct CommentOn {
    kind: Kind,
    target: Vec<Word>,
    text: Option<String>,
}

fn parse_comment(sql: &str) -> Result<CommentOn> {
    let mut words = ddl_words(sql)?;
    if words.last() == Some(&Word::Punct(';')) {
        words.pop();
    }
    let rest = words.get(2..).unwrap_or(&[]);
    let found = KINDS
        .iter()
        .filter(|(kw, _)| kw.len() <= rest.len() && kw.iter().zip(rest).all(|(w, t)| t.is(w)))
        .max_by_key(|(kw, _)| kw.len());
    let Some((keywords, kind)) = found else {
        return Err(syntax_error_at(rest.first()));
    };
    let rest = &rest[keywords.len()..];
    // The object's name may be IS in quotes, never in words.
    let Some(is) = rest.iter().rposition(|w| w.is("is")) else {
        return Err(syntax_error_at(None));
    };
    let text = match rest.get(is + 1) {
        Some(Word::Str(s)) => Some(s.clone()),
        Some(w) if w.is("null") => None,
        other => return Err(syntax_error_at(other)),
    };
    if rest.len() > is + 2 {
        return Err(syntax_error_at(rest.get(is + 2)));
    }
    Ok(CommentOn {
        kind: *kind,
        target: rest[..is].to_vec(),
        text,
    })
}

/// Reads `a.b.c` from words, as many parts as there are.
fn dotted_name(words: &[Word]) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut i = 0;
    loop {
        let Some(name) = words.get(i).and_then(Word::name) else {
            return Err(syntax_error_at(words.get(i)));
        };
        parts.push(name.to_string());
        if words.get(i + 1) != Some(&Word::Punct('.')) {
            return Ok(parts);
        }
        i += 2;
    }
}

fn undefined(state: SqlState, message: String) -> Error {
    Error::with_state(state, message)
}

impl Session {
    /// Handles COMMENT ON.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_comment(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_comment(sql) {
            return None;
        }
        Some(
            parse_comment(sql)
                .and_then(|c| self.run_comment(&c))
                .map(|()| Command::from("COMMENT")),
        )
    }

    fn run_comment(&mut self, c: &CommentOn) -> Result<()> {
        let (objkind, objname, subname) = match c.kind {
            Kind::Unmodeled => return Ok(()),
            Kind::Relation(stored, what, relkinds) => {
                let name = dotted_name(&c.target)?;
                let relation = self.find_relation(&name)?;
                if !relkinds.contains(relation.relkind.as_str()) {
                    return Err(undefined(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not {}", relation.name, article(what)),
                    ));
                }
                (stored, relation.name, String::new())
            }
            Kind::Column => {
                let mut name = dotted_name(&c.target)?;
                let Some(column) = name.pop().filter(|_| !name.is_empty()) else {
                    return Err(syntax_error_at(None));
                };
                let relation = self.find_relation(&name)?;
                let q = format!(
                    "SELECT attname FROM pg_attribute WHERE attrelid = {} AND lower(attname) = lower({})",
                    relation.oid,
                    quote_literal(&column)
                );
                if self.exec(&q, &[])?.rows.is_empty() {
                    return Err(undefined(
                        SqlState::UNDEFINED_COLUMN,
                        format!("column \"{column}\" of relation \"{}\" does not exist", relation.name),
                    ));
                }
                ("column", relation.name, column)
            }
            Kind::Type => {
                let name = dotted_name(&c.target)?;
                let last = name.last().cloned().unwrap_or_default();
                self.require_row(
                    &format!(
                        "SELECT 1 FROM pg_type WHERE lower(typname) = lower({})",
                        quote_literal(&last)
                    ),
                    SqlState::UNDEFINED_OBJECT,
                    format!("type \"{last}\" does not exist"),
                )?;
                ("type", last, String::new())
            }
            Kind::Schema => {
                let name = single_name(&c.target)?;
                self.require_schema(&name)?;
                ("schema", name, String::new())
            }
            Kind::Role => {
                let name = single_name(&c.target)?;
                self.require_row(
                    &format!("SELECT 1 FROM pg_roles WHERE rolname = {}", quote_literal(&name)),
                    SqlState::UNDEFINED_OBJECT,
                    format!("role \"{name}\" does not exist"),
                )?;
                ("role", name, String::new())
            }
            Kind::OnTable(what, catalog, name_col, rel_col) => {
                let Some(on) = c.target.iter().position(|w| w.is("on")) else {
                    return Err(syntax_error_at(None));
                };
                let name = single_name(&c.target[..on])?;
                let relation = self.find_relation(&dotted_name(&c.target[on + 1..])?)?;
                self.require_row(
                    &format!(
                        "SELECT 1 FROM {catalog} WHERE {rel_col} = {} AND {name_col} = {}",
                        relation.oid,
                        quote_literal(&name)
                    ),
                    SqlState::UNDEFINED_OBJECT,
                    format!("{what} \"{name}\" for table \"{}\" does not exist", relation.name),
                )?;
                (what, relation.name, name)
            }
            Kind::Constraint => {
                let Some(on) = c.target.iter().position(|w| w.is("on")) else {
                    return Err(syntax_error_at(None));
                };
                let name = single_name(&c.target[..on])?;
                let relation = self.find_relation(&dotted_name(&c.target[on + 1..])?)?;
                // The table's definition has them all; pg_constraint not a check.
                let ddl = self.table_ddl(&relation.name);
                if !has_constraint(&ddl, &relation.name, &name) {
                    return Err(undefined(
                        SqlState::UNDEFINED_OBJECT,
                        format!("constraint \"{name}\" for table \"{}\" does not exist", relation.name),
                    ));
                }
                ("constraint", relation.name, name)
            }
            Kind::Routine => {
                let open = c
                    .target
                    .iter()
                    .position(|w| *w == Word::Punct('('))
                    .unwrap_or(c.target.len());
                let name = dotted_name(&c.target[..open])?;
                let last = name.last().cloned().unwrap_or_default();
                self.require_row(
                    &format!(
                        "SELECT 1 FROM pg_proc WHERE lower(proname) = lower({})",
                        quote_literal(&last)
                    ),
                    SqlState::UNDEFINED_FUNCTION,
                    format!("function {last}() does not exist"),
                )?;
                ("function", last, String::new())
            }
            Kind::Database => {
                let name = single_name(&c.target)?;
                // A comment on a database from a dump of another: Postgres lets it go.
                if !self.cluster.databases()?.contains(&name) {
                    return self.c.send_warning(
                        SqlState::INVALID_CATALOG_NAME,
                        &format!("database \"{name}\" does not exist"),
                    );
                }
                ("database", name, String::new())
            }
        };

        // Replace any existing comment on the object (COMMENT is idempotent).
        let del = format!(
            "DELETE FROM _overlite_comments WHERE objkind = {} AND objname = {} AND subname = {}",
            quote_literal(objkind),
            quote_literal(&objname),
            quote_literal(&subname)
        );
        self.exec(&del, &[])?;
        let Some(text) = &c.text else {
            return Ok(()); // IS NULL removes the comment
        };
        let ins = format!(
            "INSERT INTO _overlite_comments (objkind, objname, subname, comment) VALUES ({}, {}, {}, {})",
            quote_literal(objkind),
            quote_literal(&objname),
            quote_literal(&subname),
            quote_literal(text)
        );
        self.exec(&ins, &[]).map(|_| ())
    }

    fn require_row(&mut self, q: &str, state: SqlState, message: String) -> Result<()> {
        if self.exec(q, &[])?.rows.is_empty() {
            return Err(undefined(state, message));
        }
        Ok(())
    }

    fn require_schema(&mut self, name: &str) -> Result<()> {
        self.require_row(
            &format!(
                "SELECT 1 FROM pg_namespace WHERE lower(nspname) = lower({})",
                quote_literal(name)
            ),
            SqlState::INVALID_SCHEMA_NAME,
            format!("schema \"{name}\" does not exist"),
        )
    }

    /// The relation a name means: in its schema, or first in the search_path.
    fn find_relation(&mut self, name: &[String]) -> Result<Relation> {
        let (schemas, relname) = match name {
            [schema, relname] => {
                self.require_schema(schema)?;
                (vec![schema.clone()], relname.clone())
            }
            [relname] => {
                let mut path = self.search_path.clone().unwrap_or_default();
                path.push("public".to_string());
                (path, relname.clone())
            }
            _ => return Err(syntax_error_at(None)),
        };
        for schema in &schemas {
            let q = format!(
                "SELECT c.oid, c.relname, c.relkind FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE lower(c.relname) = lower({}) AND lower(n.nspname) = lower({})",
                quote_literal(&relname),
                quote_literal(schema)
            );
            if let Some(row) = self.exec(&q, &[])?.rows.first() {
                return Ok(Relation {
                    oid: row[0].to_string(),
                    name: row[1].to_string(),
                    relkind: row[2].to_string(),
                });
            }
        }
        Err(undefined(
            SqlState::UNDEFINED_TABLE,
            format!("relation \"{}\" does not exist", name.join(".")),
        ))
    }
}

/// What pg_class says of a relation.
struct Relation {
    oid: String,
    name: String,
    relkind: String,
}

fn article(what: &str) -> String {
    let a = if what.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    format!("{a} {what}")
}

fn single_name(words: &[Word]) -> Result<String> {
    let name = dotted_name(words)?;
    match &name[..] {
        [one] => Ok(one.clone()),
        _ => Err(syntax_error_at(None)),
    }
}
