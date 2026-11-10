//! COMMENT ON `<objtype> <name>` IS `<'text'|NULL>` records an object comment in
//! _overlite_comments (SQLite has nowhere native to keep it). pg_description
//! reads that table, and obj_description()/col_description() are rewritten to
//! look it up, so psql's \d+ shows table and column comments.
//!
//! The object has to be there, as in Postgres: the kinds this server has a catalog of
//! are looked up, and a kind it has none of is accepted without effect, so a dump
//! proceeds.

use crate::ddl::drop_constraint::has_constraint;
use crate::ddl::tree;
use crate::error::{Error, Result};
use crate::session::Session;
use overlite_core::text::quote_literal;
use overlite_core::{Command, SqlState};
use pg_query::NodeEnum;
use pg_query::protobuf::ObjectType;

/// Whether sql is a COMMENT ON statement.
pub(crate) fn is_comment(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::comment).unwrap_or_default()
}

/// The kind of an object Postgres takes a comment on: what `_overlite_comments` calls
/// it, or nothing to keep for one with no catalog.
fn kind_of(t: ObjectType) -> Kind {
    use ObjectType as O;
    match t {
        O::ObjectTable => Kind::Relation("table", "table", "rp"),
        O::ObjectForeignTable => Kind::Relation("table", "foreign table", "f"),
        O::ObjectView => Kind::Relation("view", "view", "v"),
        O::ObjectMatview => Kind::Relation("view", "materialized view", "m"),
        O::ObjectIndex => Kind::Relation("index", "index", "iI"),
        O::ObjectSequence => Kind::Relation("sequence", "sequence", "S"),
        O::ObjectColumn => Kind::Column,
        O::ObjectType | O::ObjectDomain => Kind::Type,
        O::ObjectSchema => Kind::Schema,
        O::ObjectRole => Kind::Role,
        O::ObjectTabconstraint => Kind::Constraint,
        O::ObjectTrigger => Kind::OnTable("trigger", "pg_trigger", "tgname", "tgrelid"),
        O::ObjectPolicy => Kind::OnTable("policy", "pg_policy", "polname", "polrelid"),
        O::ObjectFunction => Kind::Routine,
        O::ObjectDatabase => Kind::Database,
        _ => Kind::Unmodeled,
    }
}

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

/// A statement read: the object's name, its parts as Postgres reads them (for a
/// constraint, trigger or policy the table's, then its own), and the comment, None for
/// NULL or an empty one, which Postgres takes for NULL.
#[derive(Debug, PartialEq, Eq)]
struct CommentOn {
    kind: Kind,
    target: Vec<String>,
    text: Option<String>,
}

fn comment_plan(n: &NodeEnum) -> Option<CommentOn> {
    let NodeEnum::CommentStmt(c) = n else {
        return None;
    };
    let kind = kind_of(tree::object(c.objtype));
    let object = c.object.as_deref().and_then(|o| o.node.as_ref());
    let target = match object {
        Some(NodeEnum::List(l)) => tree::strings(&l.items),
        Some(NodeEnum::String(s)) => vec![s.sval.clone()],
        Some(NodeEnum::TypeName(t)) => tree::strings(&t.names),
        Some(NodeEnum::ObjectWithArgs(f)) => tree::strings(&f.objname),
        _ => Vec::new(),
    };
    Some(CommentOn {
        kind,
        target,
        text: (!c.comment.is_empty()).then(|| c.comment.clone()),
    })
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
            tree::statement(sql, |n| comment_plan(n).ok_or_else(|| tree::parse_failure(sql)))
                .and_then(|c| self.run_comment(&c))
                .map(|()| Command::from("COMMENT")),
        )
    }

    fn run_comment(&mut self, c: &CommentOn) -> Result<()> {
        let (objkind, objname, subname) = match c.kind {
            Kind::Unmodeled => return Ok(()),
            Kind::Relation(stored, what, relkinds) => {
                let name = c.target.clone();
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
                let mut name = c.target.clone();
                let Some(column) = name.pop().filter(|_| !name.is_empty()) else {
                    return Err(tree::syntax_error());
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
                let last = c.target.last().cloned().unwrap_or_default();
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
                let Some((name, table)) = c.target.split_last() else {
                    return Err(tree::syntax_error());
                };
                let name = name.clone();
                let relation = self.find_relation(table)?;
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
                let Some((name, table)) = c.target.split_last() else {
                    return Err(tree::syntax_error());
                };
                let name = name.clone();
                let relation = self.find_relation(table)?;
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
                let last = c.target.last().cloned().unwrap_or_default();
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
        // IS NULL and IS '' remove the comment. The public schema's comes with the
        // database ('standard public schema'), so its removal is kept as a NULL row,
        // which pg_description reads as no comment.
        let text = c.text.as_deref().filter(|t| !t.is_empty());
        let public = objkind == "schema" && objname == "public";
        if text.is_none() && !public {
            return Ok(());
        }
        let ins = format!(
            "INSERT INTO _overlite_comments (objkind, objname, subname, comment) VALUES ({}, {}, {}, {})",
            quote_literal(objkind),
            quote_literal(&objname),
            quote_literal(&subname),
            text.map_or_else(|| "NULL".to_string(), quote_literal)
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
            _ => return Err(tree::syntax_error()),
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

fn single_name(name: &[String]) -> Result<String> {
    match name {
        [one] => Ok(one.clone()),
        _ => Err(tree::syntax_error()),
    }
}
