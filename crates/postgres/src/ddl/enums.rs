//! Enum types: CREATE/ALTER/DROP TYPE ... AS ENUM are intercepted here and
//! recorded in the engine's internal enum tables; a column whose type is a known
//! enum is rewritten to TEXT plus a CHECK constraint (native SQLite, so the file
//! stays usable without overlite and membership is enforced). pg_type/pg_enum are
//! populated from the same tables for introspection. Ordering/comparison uses the
//! stored text, not the declared enum order.
//!
//! Each statement is read from its node; a type's name is its last part, as
//! Postgres reads it. Names are compared without case: a file from before kept an
//! unquoted name as the client wrote it.

use crate::ddl::sequences::int_text;
use crate::ddl::tree;
use crate::error::{Result, errf};
use crate::session::Session;
use overlite_core::text::{eq_fold, quote_literal, quoted, split_words};
use overlite_core::{Command, SqlState};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, ObjectType};
use std::collections::HashMap;

/// A type's statement, read from its node.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TypePlan {
    CreateEnum {
        name: String,
        labels: Vec<String>,
    },
    CreateComposite(String),
    /// A range or base type: accepted, kept nowhere.
    CreateOther,
    Rename {
        name: String,
        to: String,
    },
    RenameValue {
        name: String,
        from: String,
        to: String,
    },
    AddValue {
        name: String,
        label: String,
        if_not_exists: bool,
        /// BEFORE or AFTER a label: (after, label).
        anchor: Option<(bool, String)>,
    },
    /// Any other ALTER TYPE: accepted, kept nowhere.
    AlterOther,
    Drop {
        names: Vec<String>,
        if_exists: bool,
    },
}

/// The last part of a qualified name: a type is kept by its name alone.
fn last(names: &[pb::Node]) -> String {
    tree::strings(names).pop().unwrap_or_default()
}

fn type_plan(n: &NodeEnum) -> Option<TypePlan> {
    Some(match n {
        NodeEnum::CreateEnumStmt(c) => TypePlan::CreateEnum {
            name: last(&c.type_name),
            labels: tree::strings(&c.vals),
        },
        NodeEnum::CompositeTypeStmt(c) => TypePlan::CreateComposite(c.typevar.as_ref()?.relname.clone()),
        NodeEnum::CreateRangeStmt(_) => TypePlan::CreateOther,
        NodeEnum::DefineStmt(d) if tree::object(d.kind) == ObjectType::ObjectType => TypePlan::CreateOther,
        NodeEnum::RenameStmt(r) if tree::object(r.rename_type) == ObjectType::ObjectType => TypePlan::Rename {
            name: match r.object.as_deref()?.node.as_ref()? {
                NodeEnum::List(l) => last(&l.items),
                NodeEnum::TypeName(t) => last(&t.names),
                _ => return None,
            },
            to: r.newname.clone(),
        },
        NodeEnum::AlterEnumStmt(a) if !a.old_val.is_empty() => TypePlan::RenameValue {
            name: last(&a.type_name),
            from: a.old_val.clone(),
            to: a.new_val.clone(),
        },
        NodeEnum::AlterEnumStmt(a) => TypePlan::AddValue {
            name: last(&a.type_name),
            label: a.new_val.clone(),
            if_not_exists: a.skip_if_new_val_exists,
            anchor: (!a.new_val_neighbor.is_empty()).then(|| (a.new_val_is_after, a.new_val_neighbor.clone())),
        },
        NodeEnum::DropStmt(d) if tree::object(d.remove_type) == ObjectType::ObjectType => TypePlan::Drop {
            names: d
                .objects
                .iter()
                .filter_map(|o| match &o.node {
                    Some(NodeEnum::TypeName(t)) => Some(last(&t.names)),
                    Some(NodeEnum::List(l)) => Some(last(&l.items)),
                    _ => None,
                })
                .collect(),
            if_exists: d.missing_ok,
        },
        NodeEnum::AlterTableStmt(a) if tree::object(a.objtype) == ObjectType::ObjectType => TypePlan::AlterOther,
        NodeEnum::AlterOwnerStmt(a) if tree::object(a.object_type) == ObjectType::ObjectType => TypePlan::AlterOther,
        NodeEnum::AlterObjectSchemaStmt(a) if tree::object(a.object_type) == ObjectType::ObjectType => {
            TypePlan::AlterOther
        }
        NodeEnum::AlterTypeStmt(_) => TypePlan::AlterOther,
        _ => return None,
    })
}

/// Reports whether sql is a CREATE/ALTER/DROP TYPE statement (used by the
/// extended-query path to route it like other utility DDL).
pub(crate) fn is_type_ddl(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::type_ddl).unwrap_or_else(|| is_type_ddl_text(sql))
}

/// `is_type_ddl` by the text, for what Postgres' grammar does not read.
pub(crate) fn is_type_ddl_text(sql: &str) -> bool {
    if crate::ddl::objects::is_object_ddl(sql) {
        return true;
    }
    let f = split_words(sql);
    if f.len() < 2 || !eq_fold(f[1], "type") {
        return false;
    }
    matches!(f[0].to_uppercase().as_str(), "CREATE" | "ALTER" | "DROP")
}

impl Session {
    /// Handles CREATE/ALTER/DROP TYPE; None when it is not one.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_type_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        if let Some(r) = self.try_object_ddl(sql) {
            return Some(r);
        }
        let plan = tree::read(sql, type_plan)?;
        Some(self.run_type_ddl(plan))
    }

    fn run_type_ddl(&mut self, plan: TypePlan) -> Result<Command> {
        match plan {
            TypePlan::CreateEnum { name, labels } => {
                self.exec(
                    "INSERT INTO _overlite_enum_types (typname) VALUES (?1)",
                    &[overlite_core::Value::from(name.as_str())],
                )
                .map_err(|e| match e.sql_state() {
                    SqlState::UNIQUE_VIOLATION => errf!(SqlState::DUPLICATE_OBJECT, "type \"{name}\" already exists"),
                    _ => e,
                })?;
                for (i, label) in labels.iter().enumerate() {
                    self.exec(
                        &("INSERT INTO _overlite_enums (typname, label, sortorder) VALUES (".to_string()
                            + &quote_literal(&name)
                            + ", "
                            + &quote_literal(label)
                            + ", "
                            + &int_text((i + 1) as i64)
                            + ")"),
                        &[],
                    )?;
                }
                Ok(Command::from("CREATE TYPE"))
            }
            TypePlan::CreateComposite(name) => {
                // Recorded so it shows in pg_type / \dT; its fields are not modeled.
                self.exec(
                    &("INSERT INTO _overlite_composite_types (typname) VALUES (".to_string()
                        + &quote_literal(&name)
                        + ")"),
                    &[],
                )?;
                Ok(Command::from("CREATE TYPE"))
            }
            TypePlan::CreateOther => Ok(Command::from("CREATE TYPE")),
            TypePlan::Rename { name, to } => {
                for table in ["_overlite_enum_types", "_overlite_enums"] {
                    self.exec(
                        &format!("UPDATE {table} SET typname = ?1 WHERE typname = ?2 COLLATE NOCASE"),
                        &[
                            overlite_core::Value::from(to.as_str()),
                            overlite_core::Value::from(name.as_str()),
                        ],
                    )?;
                }
                Ok(Command::from("ALTER TYPE"))
            }
            TypePlan::RenameValue { name, from, to } => {
                self.exec(
                    "UPDATE _overlite_enums SET label = ?1 WHERE typname = ?2 COLLATE NOCASE AND label = ?3",
                    &[
                        overlite_core::Value::from(to.as_str()),
                        overlite_core::Value::from(name.as_str()),
                        overlite_core::Value::from(from.as_str()),
                    ],
                )?;
                Ok(Command::from("ALTER TYPE"))
            }
            TypePlan::AddValue {
                name,
                label,
                if_not_exists,
                anchor,
            } => {
                self.add_enum_value(&name, &label, if_not_exists, anchor)?;
                Ok(Command::from("ALTER TYPE"))
            }
            TypePlan::AlterOther => Ok(Command::from("ALTER TYPE")),
            TypePlan::Drop { names, if_exists } => {
                self.drop_types(&names, if_exists)?;
                Ok(Command::from("DROP TYPE"))
            }
        }
    }

    fn add_enum_value(
        &mut self,
        name: &str,
        label: &str,
        if_not_exists: bool,
        anchor: Option<(bool, String)>,
    ) -> Result<()> {
        if !self.enum_type_exists(name) {
            return Err(errf!(
                SqlState::UNDEFINED_OBJECT,
                "type {:?} does not exist",
                quoted(name)
            ));
        }
        if self.enum_label_exists(name, label) {
            if if_not_exists {
                return Ok(());
            }
            return Err(errf!(
                SqlState::DUPLICATE_OBJECT,
                "label {:?} already exists in type {:?}",
                quoted(label),
                quoted(name)
            ));
        }
        let mut order = "(SELECT COALESCE(MAX(sortorder), 0) + 1 FROM _overlite_enums WHERE typname = ".to_string()
            + &quote_literal(name)
            + " COLLATE NOCASE)";
        if let Some((after, anchor)) = anchor {
            let delta = if after { "+ 0.5" } else { "- 0.5" };
            order = "(SELECT sortorder ".to_string()
                + delta
                + " FROM _overlite_enums WHERE typname = "
                + &quote_literal(name)
                + " COLLATE NOCASE AND label = "
                + &quote_literal(&anchor)
                + ")";
        }
        // The type as it is kept, whatever the case it was named in.
        let kept = "(SELECT typname FROM _overlite_enum_types WHERE typname = ".to_string()
            + &quote_literal(name)
            + " COLLATE NOCASE)";
        self.exec(
            &("INSERT INTO _overlite_enums (typname, label, sortorder) VALUES (".to_string()
                + &kept
                + ", "
                + &quote_literal(label)
                + ", "
                + &order
                + ")"),
            &[],
        )?;
        Ok(())
    }

    fn drop_types(&mut self, names: &[String], if_exists: bool) -> Result<()> {
        for name in names {
            if !self.enum_type_exists(name) && !self.composite_type_exists(name) {
                if if_exists {
                    self.c
                        .send_notice(&format!("type \"{name}\" does not exist, skipping"))?;
                    continue;
                }
                return Err(errf!(SqlState::UNDEFINED_OBJECT, "type \"{name}\" does not exist"));
            }
            for table in ["_overlite_enum_types", "_overlite_enums", "_overlite_composite_types"] {
                self.exec(
                    &format!("DELETE FROM {table} WHERE typname = ?1 COLLATE NOCASE"),
                    &[overlite_core::Value::from(name.as_str())],
                )?;
            }
        }
        Ok(())
    }

    fn composite_type_exists(&mut self, name: &str) -> bool {
        let q = "SELECT 1 FROM _overlite_composite_types WHERE typname = ".to_string()
            + &quote_literal(name)
            + " COLLATE NOCASE";
        self.exec(&q, &[]).is_ok_and(|rs| !rs.rows.is_empty())
    }

    fn enum_type_exists(&mut self, name: &str) -> bool {
        match self.exec(
            &("SELECT 1 FROM _overlite_enum_types WHERE typname = ".to_string()
                + &quote_literal(name)
                + " COLLATE NOCASE"),
            &[],
        ) {
            Ok(rs) => !rs.rows.is_empty(),
            Err(_) => false,
        }
    }

    fn enum_label_exists(&mut self, name: &str, label: &str) -> bool {
        match self.exec(
            &("SELECT 1 FROM _overlite_enums WHERE typname = ".to_string()
                + &quote_literal(name)
                + " COLLATE NOCASE AND label = "
                + &quote_literal(label)),
            &[],
        ) {
            Ok(rs) => !rs.rows.is_empty(),
            Err(_) => false,
        }
    }

    // --- enum column rewriting ------------------------------------------------

    /// Rewrites enum columns in a CREATE TABLE to TEXT + CHECK. Runs before the
    /// dialect rewrite (like expand_sequences), so the labels it emits are
    /// protected by the string-literal-aware rewrite. The columns are the tree's
    /// ColumnDefs whose type names a known enum; each type is replaced where Postgres'
    /// scanner finds it, and the rest of the statement stays as written.
    pub(crate) fn expand_enums(&mut self, sql: &str) -> Result<String> {
        let Some(columns) = tree::read(sql, enum_candidates) else {
            return Ok(sql.to_string());
        };
        let enums = self.load_enums()?;
        if enums.is_empty() {
            return Ok(sql.to_string());
        }
        Ok(rewrite_enum_columns(sql, &columns, &enums))
    }

    fn load_enums(&mut self) -> Result<HashMap<String, Vec<String>>> {
        let rs = self.exec(
            "SELECT typname, label FROM _overlite_enums ORDER BY typname, sortorder",
            &[],
        )?;
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rs.rows {
            if row.len() < 2 {
                continue;
            }
            let key = row[0].to_string().to_lowercase();
            m.entry(key).or_default().push(row[1].to_string());
        }
        Ok(m)
    }
}

/// The columns of a CREATE TABLE as (where the column's name starts, where its type
/// starts, the type's last name): those that may be of an enum. None for any other
/// statement, or a table without columns.
fn enum_candidates(n: &NodeEnum) -> Option<Vec<(usize, usize, String)>> {
    let NodeEnum::CreateStmt(c) = n else {
        return None;
    };
    let out: Vec<_> = c
        .table_elts
        .iter()
        .filter_map(|e| match &e.node {
            Some(NodeEnum::ColumnDef(d)) => {
                let t = d.type_name.as_ref()?;
                if !t.array_bounds.is_empty() || !t.typmods.is_empty() {
                    return None;
                }
                Some((
                    usize::try_from(d.location).ok()?,
                    usize::try_from(t.location).ok()?,
                    last(&t.names).to_lowercase(),
                ))
            }
            _ => None,
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// Replaces each column whose declared type is a known enum with
/// "col text CHECK (col IN ('a','b',...))", the column's name as written.
fn rewrite_enum_columns(sql: &str, columns: &[(usize, usize, String)], enums: &HashMap<String, Vec<String>>) -> String {
    let toks = tree::tokens(sql);
    let text = |t: &tree::Token| &sql[t.start..t.end];
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for (col, typ, name) in columns {
        let Some(labels) = enums.get(name) else { continue };
        let Some(c) = toks.iter().find(|t| t.start == *col) else {
            continue;
        };
        let Some(first) = toks.iter().position(|t| t.start == *typ) else {
            continue;
        };
        // The type's name: its words and the dots between them.
        let mut end = toks[first].end;
        let mut i = first + 1;
        while i + 1 < toks.len() && text(&toks[i]) == "." {
            end = toks[i + 1].end;
            i += 2;
        }
        let column = text(c);
        let quoted: Vec<String> = labels.iter().map(|l| quote_literal(l)).collect();
        out.push_str(&sql[at..*typ]);
        out.push_str(&["text CHECK (", column, " IN (", &quoted.join(", "), "))"].concat());
        at = end;
    }
    out.push_str(&sql[at..]);
    out
}
