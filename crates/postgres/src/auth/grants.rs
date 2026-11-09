//! GRANT/REVOKE handling and table-ownership tracking. These write the internal
//! _overlite_grants / _overlite_owners tables that privileges.rs reads.

use crate::auth::privileges::Privilege;
use crate::ddl::alter::split_qualified_ref;
use crate::ddl::sequences::is_ident_part;
use crate::ddl::tree::{self, Role};
use crate::error::{Result, errf};
use crate::session::Session;
use overlite_core::text::{eq_fold, quote_ident, quote_literal, quoted};
use overlite_core::{Command, SqlState, Value};
use overlite_engine::{forget_table_rows_sql, split_stored_name, stored_table_name};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// Whether sql is a GRANT or REVOKE statement, or an ALTER FUNCTION (ROUTINE,
/// PROCEDURE), which changes who owns a function and as whom its body runs.
pub(crate) fn is_grant(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::grant).unwrap_or_default()
}

impl Session {
    /// Parses a table GRANT/REVOKE and updates _overlite_grants. Role (GRANT role
    /// TO role) and non-table object grants are accepted as no-ops. Returns the
    /// command tag ("GRANT"/"REVOKE").
    pub(crate) fn apply_grant(&mut self, sql: &str) -> Result<Command> {
        if let Some(r) = self.try_function_acl(sql) {
            return r;
        }
        // What is left of is_grant's statements once try_function_acl has its own:
        // an ALTER of a function this server does not model.
        let Some(plan) = tree::statement(sql, |n| Ok(grant_plan(sql, n)))? else {
            return Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER FUNCTION supports OWNER TO and SECURITY DEFINER | INVOKER only"
            ));
        };
        let (revoke, privs, tables, grantees) = match plan {
            GrantPlan::Roles(rg) => {
                let revoke = rg.revoke;
                self.apply_role_grant(&rg)?;
                return Ok(Command::from(if revoke { "REVOKE" } else { "GRANT" }));
            }
            // A shape not modelled (schemas, sequences, ALL TABLES IN SCHEMA...): no-op.
            GrantPlan::Other { revoke } => return Ok(Command::from(if revoke { "REVOKE" } else { "GRANT" })),
            GrantPlan::Tables {
                revoke,
                privs,
                tables,
                grantees,
            } => (revoke, privs, tables, grantees),
        };
        let tag = Command::from(if revoke { "REVOKE" } else { "GRANT" });
        let grantees: Vec<String> = grantees.iter().map(|g| self.grantee_name(g)).collect();
        let tables: Vec<String> = tables.iter().map(|t| self.stored_name(t)).collect();
        for tbl in &tables {
            if self.table_ddl(tbl).is_empty() && !self.is_view(tbl) {
                return Err(errf!(SqlState::UNDEFINED_TABLE, "relation \"{tbl}\" does not exist"));
            }
        }
        for who in &grantees {
            if !eq_fold(who, "public") && !self.role_exists(who) {
                return Err(errf!(SqlState::UNDEFINED_OBJECT, "role \"{who}\" does not exist"));
            }
        }
        if !self.priv_bypass() {
            for tbl in &tables {
                if !self.has_privilege(tbl, Privilege::Owner) {
                    return Err(errf!(
                        SqlState::INSUFFICIENT_PRIVILEGE,
                        "permission denied for table {tbl}"
                    ));
                }
            }
        }

        // The columns named, checked on every table before anything is written.
        let mut columns: Vec<Vec<(String, Vec<String>)>> = Vec::with_capacity(tables.len());
        for tbl in &tables {
            let mut per_priv = Vec::new();
            for p in &privs {
                let Some(cols) = &p.columns else { continue };
                let kinds = column_privileges(&p.name)?;
                let found = self.grant_columns(tbl, cols)?;
                for k in kinds {
                    per_priv.push((k.to_string(), found.clone()));
                }
            }
            columns.push(per_priv);
        }

        for (tbl, per_priv) in tables.iter().zip(&columns) {
            for who in &grantees {
                for (priv_, cols) in per_priv {
                    for col in cols {
                        self.exec(
                            "DELETE FROM _overlite_column_grants WHERE lower(grantee) = lower(?1) AND tablename = ?2 AND columnname = ?3 AND privilege = ?4",
                            &[Value::from(who.as_str()), Value::from(tbl.as_str()), Value::from(col.as_str()), Value::from(priv_.as_str())],
                        )?;
                        if !revoke {
                            self.exec(
                                "INSERT INTO _overlite_column_grants (grantee, tablename, columnname, privilege) VALUES (?1, ?2, ?3, ?4)",
                                &[Value::from(who.as_str()), Value::from(tbl.as_str()), Value::from(col.as_str()), Value::from(priv_.as_str())],
                            )?;
                        }
                    }
                }
                for p in privs.iter().filter(|p| p.columns.is_none()) {
                    let priv_ = &p.name;
                    if revoke {
                        // As in Postgres, revoking on the table revokes it on every column.
                        self.exec(
                            "DELETE FROM _overlite_column_grants WHERE lower(grantee) = lower(?1) AND tablename = ?2 AND (upper(privilege) = upper(?3) OR upper(?3) = 'ALL')",
                            &[Value::from(who.as_str()), Value::from(tbl.as_str()), Value::from(priv_.as_str())],
                        )?;
                        let q = format!(
                            "DELETE FROM _overlite_grants WHERE lower(grantee)=lower({}) AND lower(tablename)=lower({}) AND (upper(privilege)=upper({}) OR upper({})='ALL')",
                            quote_literal(who),
                            quote_literal(tbl),
                            quote_literal(priv_),
                            quote_literal(priv_)
                        );
                        self.exec(&q, &[])?;
                        continue;
                    }
                    // Avoid duplicate rows for the same (grantee, table, privilege).
                    let q = format!(
                        "DELETE FROM _overlite_grants WHERE lower(grantee)=lower({}) AND lower(tablename)=lower({}) AND upper(privilege)=upper({})",
                        quote_literal(who),
                        quote_literal(tbl),
                        quote_literal(priv_)
                    );
                    self.exec(&q, &[])?;
                    let q = format!(
                        "INSERT INTO _overlite_grants (grantee, tablename, privilege) VALUES ({}, {}, {})",
                        quote_literal(who),
                        quote_literal(tbl),
                        quote_literal(&priv_.to_uppercase())
                    );
                    self.exec(&q, &[])?;
                }
            }
        }
        Ok(tag)
    }

    /// The columns of table a GRANT names, as the table spells them: 42703 for one it
    /// does not have. A quoted name must match exactly, an unquoted one in any case.
    fn grant_columns(&mut self, table: &str, cols: &[GrantColumn]) -> Result<Vec<String>> {
        let q = "SELECT name FROM pragma_table_xinfo(".to_string() + &quote_literal(table) + ") WHERE hidden <> 1";
        let have: Vec<String> = self
            .exec(&q, &[])?
            .rows
            .iter()
            .filter_map(|r| match &r[0] {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let mut out = Vec::with_capacity(cols.len());
        for c in cols {
            let found = have.iter().find(|h| *h == &c.name).or_else(|| {
                if c.quoted {
                    None
                } else {
                    have.iter().find(|h| eq_fold(h, &c.name))
                }
            });
            let Some(found) = found else {
                let (_, relation) = split_stored_name(table);
                return Err(errf!(
                    SqlState::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{relation}\" does not exist",
                    c.name
                ));
            };
            if !out.contains(found) {
                out.push(found.clone());
            }
        }
        Ok(out)
    }

    fn is_view(&mut self, name: &str) -> bool {
        let q = "SELECT 1 FROM pg_class WHERE lower(relname) = lower(".to_string() + &quote_literal(name) + ")";
        self.exec(&q, &[]).is_ok_and(|rs| !rs.rows.is_empty())
    }

    /// Records or removes role membership for GRANT <role> TO <member> / REVOKE
    /// [ADMIN OPTION FOR] <role> FROM <member>. Only a superuser or a holder of
    /// ADMIN OPTION on the role may administer its membership.
    fn apply_role_grant(&mut self, rg: &RoleGrant) -> Result<()> {
        let revoke = rg.revoke;
        let roles: Vec<String> = rg.roles.iter().map(|g| self.grantee_name(g)).collect();
        let members: Vec<String> = rg.members.iter().map(|g| self.grantee_name(g)).collect();
        for role in &roles {
            if !self.can_grant_membership(role) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must have admin option on role {:?}",
                    quoted(role)
                ));
            }
            for mem in &members {
                if revoke {
                    if rg.admin_only {
                        // REVOKE ADMIN OPTION FOR: keep membership, drop the option.
                        let q = format!(
                            "UPDATE _overlite_memberships SET admin_option=0 WHERE lower(member)=lower({}) AND lower(roleof)=lower({})",
                            quote_literal(mem),
                            quote_literal(role)
                        );
                        self.exec(&q, &[])?;
                        continue;
                    }
                    let q = format!(
                        "DELETE FROM _overlite_memberships WHERE lower(member)=lower({}) AND lower(roleof)=lower({})",
                        quote_literal(mem),
                        quote_literal(role)
                    );
                    self.exec(&q, &[])?;
                    continue;
                }
                if !self.role_exists(role) {
                    return Err(errf!(
                        SqlState::UNDEFINED_OBJECT,
                        "role {:?} does not exist",
                        quoted(role)
                    ));
                }
                if !self.role_exists(mem) {
                    return Err(errf!(
                        SqlState::UNDEFINED_OBJECT,
                        "role {:?} does not exist",
                        quoted(mem)
                    ));
                }
                let admin = if rg.admin { "1" } else { "0" };
                // Idempotent: don't stack duplicate membership rows.
                let q = format!(
                    "DELETE FROM _overlite_memberships WHERE lower(member)=lower({}) AND lower(roleof)=lower({})",
                    quote_literal(mem),
                    quote_literal(role)
                );
                self.exec(&q, &[])?;
                let q = format!(
                    "INSERT INTO _overlite_memberships (member, roleof, admin_option) VALUES ({}, {}, {})",
                    quote_literal(mem),
                    quote_literal(role),
                    admin
                );
                self.exec(&q, &[])?;
            }
        }
        Ok(())
    }

    /// The name a grantee or a granted role stands for.
    fn grantee_name(&self, r: &Role) -> String {
        match r {
            Role::Named(n) => n.clone(),
            Role::Current => self.current_role.clone(),
            Role::Session => self.session_user.clone(),
            Role::Public => "public".to_string(),
        }
    }

    /// Whether the current role may grant/revoke membership in role: a
    /// superuser/unmanaged role, or a holder of ADMIN OPTION.
    fn can_grant_membership(&mut self, role: &str) -> bool {
        let cur = self.current_role.clone();
        self.role_bypasses(&cur) || self.has_admin_option(role)
    }

    /// Keeps _overlite_owners in step with CREATE and DROP TABLE, run after the
    /// statement. A row holds the table's stored name, as privileges read it.
    pub(crate) fn record_ownership(&mut self, sql: &str) {
        self.record_function_owner(sql);
        let Some(plan) = tree::read(sql, ownership_plan) else {
            return;
        };
        match plan {
            Ownership::Create { table, replace } => {
                let conflict = if replace { "REPLACE" } else { "IGNORE" };
                let q = format!(
                    "INSERT OR {conflict} INTO _overlite_owners (tablename, owner) VALUES ({}, {})",
                    quote_literal(&self.stored_name(&table)),
                    quote_literal(&self.current_role)
                );
                let _ = self.exec(&q, &[]);
            }
            Ownership::DropViews(views) => {
                for v in views {
                    let stored = self.stored_name(&v);
                    self.forget_table(&stored);
                }
            }
            Ownership::DropTables(tables) => {
                for t in tables {
                    for name in self.names_of(&t) {
                        self.forget_table(&name);
                    }
                }
                self.rls_changed();
            }
            Ownership::Alter => self.follow_columns(sql),
        }
    }

    /// Keeps column grants on their columns after an ALTER TABLE run by the engine:
    /// a renamed column takes them along, and a dropped one takes them away, so that
    /// a column added later by that name starts with none.
    fn follow_columns(&mut self, sql: &str) {
        let Ok(parsed) = crate::ast::parse::shared(sql) else {
            return;
        };
        let stored = |s: &mut Self, r: &pb::RangeVar| {
            let reference = if r.schemaname.is_empty() {
                quote_ident(&r.relname)
            } else {
                quote_ident(&r.schemaname) + "." + &quote_ident(&r.relname)
            };
            s.stored_name(&reference)
        };
        for raw in &parsed.stmts {
            match raw.stmt.as_deref().and_then(|s| s.node.as_ref()) {
                Some(NodeEnum::RenameStmt(r)) if r.rename_type == pb::ObjectType::ObjectColumn as i32 => {
                    let Some(rel) = &r.relation else { continue };
                    let table = stored(self, rel);
                    let _ = self.exec(
                        "UPDATE _overlite_column_grants SET columnname = ?1 WHERE tablename = ?2 AND columnname = ?3",
                        &[
                            Value::from(r.newname.as_str()),
                            Value::from(table.as_str()),
                            Value::from(r.subname.as_str()),
                        ],
                    );
                }
                Some(NodeEnum::AlterTableStmt(a)) => {
                    let Some(rel) = &a.relation else { continue };
                    let table = stored(self, rel);
                    for cmd in &a.cmds {
                        if let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref()
                            && c.subtype == pb::AlterTableType::AtDropColumn as i32
                        {
                            self.forget_column_grants(&table, &c.name);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Drops the grants on a column that is gone.
    pub(crate) fn forget_column_grants(&mut self, table: &str, column: &str) {
        let _ = self.exec(
            "DELETE FROM _overlite_column_grants WHERE tablename = ?1 AND columnname = ?2 \
             AND NOT EXISTS (SELECT 1 FROM pragma_table_xinfo(?1) WHERE name = ?2)",
            &[Value::from(table), Value::from(column)],
        );
    }

    /// The stored names a reference may have meant, for after the statement has
    /// removed the table that told them apart.
    fn names_of(&mut self, reference: &str) -> Vec<String> {
        let (schema, table) = split_qualified_ref(reference);
        if !schema.is_empty() {
            return vec![stored_table_name(&schema, &table)];
        }
        let mut names = vec![table.clone()];
        for s in self.search_path.clone().unwrap_or_default() {
            names.push(stored_table_name(&s, &table));
        }
        names
    }

    fn stored_table_exists(&mut self, name: &str) -> bool {
        !self.table_ddl(name).is_empty()
    }

    /// Clears the rows of a table that is gone.
    fn forget_table(&mut self, name: &str) {
        if self.stored_table_exists(name) {
            return;
        }
        let _ = self.exec(&forget_table_rows_sql(name), &[]);
    }
}

/// What a statement changes of the owners _overlite_owners keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// A table or view made by the current role; replace: whether it takes the place
    /// of a row for that name (not after IF NOT EXISTS or OR REPLACE, which may have
    /// made nothing new).
    Create {
        table: String,
        replace: bool,
    },
    DropViews(Vec<String>),
    DropTables(Vec<String>),
    /// Any ALTER: its columns' grants follow it.
    Alter,
}

/// The ownership change a statement makes, read from its node. A temporary or
/// unlogged table records none, as before: only a permanent table's name is a stored
/// name the owners' rows can hold.
pub(crate) fn ownership_plan(n: &NodeEnum) -> Option<Ownership> {
    let refs = |objects: &[pb::Node]| -> Vec<String> {
        objects
            .iter()
            .filter_map(|o| match &o.node {
                Some(NodeEnum::List(l)) => Some(tree::reference_of(&tree::strings(&l.items))),
                _ => None,
            })
            .collect()
    };
    match n {
        NodeEnum::ViewStmt(v) => Some(Ownership::Create {
            table: tree::reference(v.view.as_ref()?),
            replace: !v.replace,
        }),
        NodeEnum::CreateStmt(c) => {
            let r = c.relation.as_ref()?;
            (r.relpersistence == "p").then(|| Ownership::Create {
                table: tree::reference(r),
                replace: !c.if_not_exists,
            })
        }
        NodeEnum::CreateTableAsStmt(c) if tree::object(c.objtype) == pb::ObjectType::ObjectTable => {
            let r = c.into.as_ref()?.rel.as_ref()?;
            (r.relpersistence == "p").then(|| Ownership::Create {
                table: tree::reference(r),
                replace: !c.if_not_exists,
            })
        }
        NodeEnum::DropStmt(d) => match tree::object(d.remove_type) {
            pb::ObjectType::ObjectView => Some(Ownership::DropViews(refs(&d.objects))),
            pb::ObjectType::ObjectTable => Some(Ownership::DropTables(refs(&d.objects))),
            _ => None,
        },
        NodeEnum::AlterTableStmt(_) | NodeEnum::RenameStmt(_) => Some(Ownership::Alter),
        _ => None,
    }
}

/// The parsed form of a role-membership GRANT/REVOKE.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RoleGrant {
    revoke: bool,
    roles: Vec<Role>,
    members: Vec<Role>,
    /// GRANT ... WITH ADMIN OPTION
    admin: bool,
    /// REVOKE ADMIN OPTION FOR ...
    admin_only: bool,
}

/// A column a GRANT names, as Postgres reads it (folded unless quoted).
#[derive(Clone, Debug, PartialEq, Eq)]
struct GrantColumn {
    name: String,
    /// Written quoted: it matches its column exactly, and only then.
    quoted: bool,
}

/// One privilege of a GRANT: SELECT, ALL, ..., with the columns it is limited to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrivSpec {
    name: String,
    columns: Option<Vec<GrantColumn>>,
}

/// What a GRANT or REVOKE asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GrantPlan {
    /// Privileges on tables (and views).
    Tables {
        revoke: bool,
        privs: Vec<PrivSpec>,
        /// References as `split_qualified_ref` reads them.
        tables: Vec<String>,
        grantees: Vec<Role>,
    },
    /// Membership in roles.
    Roles(RoleGrant),
    /// Privileges on what is not modelled: schemas, sequences, databases, ALL
    /// TABLES IN SCHEMA, default privileges.
    Other { revoke: bool },
}

/// The privileges a column form of name stands for: 0LP01 for one with no column
/// form, as in Postgres.
fn column_privileges(name: &str) -> Result<&'static [&'static str]> {
    Ok(match name {
        "ALL" => &["SELECT", "INSERT", "UPDATE", "REFERENCES"],
        "SELECT" => &["SELECT"],
        "INSERT" => &["INSERT"],
        "UPDATE" => &["UPDATE"],
        "REFERENCES" => &["REFERENCES"],
        other => {
            return Err(errf!(
                SqlState::INVALID_GRANT_OPERATION,
                "invalid privilege type {other} for column"
            ));
        }
    })
}

/// A GRANT's or REVOKE's plan, from its node.
pub(crate) fn grant_plan(sql: &str, n: &NodeEnum) -> Option<GrantPlan> {
    let quoted = tree::quoted_names(sql);
    match n {
        NodeEnum::GrantStmt(g) => {
            let revoke = !g.is_grant;
            if pb::GrantTargetType::try_from(g.targtype) != Ok(pb::GrantTargetType::AclTargetObject)
                || tree::object(g.objtype) != pb::ObjectType::ObjectTable
            {
                return Some(GrantPlan::Other { revoke });
            }
            let mut privs: Vec<PrivSpec> = g
                .privileges
                .iter()
                .filter_map(|p| match &p.node {
                    Some(NodeEnum::AccessPriv(a)) => Some(PrivSpec {
                        name: if a.priv_name.is_empty() {
                            "ALL".to_string()
                        } else {
                            a.priv_name.to_uppercase()
                        },
                        columns: (!a.cols.is_empty()).then(|| {
                            tree::strings(&a.cols)
                                .into_iter()
                                .map(|name| GrantColumn {
                                    quoted: quoted.contains(&name),
                                    name,
                                })
                                .collect()
                        }),
                    }),
                    _ => None,
                })
                .collect();
            if privs.is_empty() {
                // ALL [PRIVILEGES]
                privs.push(PrivSpec {
                    name: "ALL".to_string(),
                    columns: None,
                });
            }
            Some(GrantPlan::Tables {
                revoke,
                privs,
                tables: tree::references(&g.objects),
                grantees: g.grantees.iter().filter_map(tree::role_spec).map(tree::role).collect(),
            })
        }
        NodeEnum::GrantRoleStmt(g) => {
            let admin = tree::defs(&g.opt).any(|d| d.defname == "admin" && (!g.is_grant || tree::def_bool(d)));
            let roles = |list: &[pb::Node]| -> Vec<Role> {
                list.iter()
                    .filter_map(|n| match &n.node {
                        Some(NodeEnum::AccessPriv(a)) => Some(Role::Named(a.priv_name.clone())),
                        Some(NodeEnum::RoleSpec(r)) => Some(tree::role(r)),
                        _ => None,
                    })
                    .collect()
            };
            Some(GrantPlan::Roles(RoleGrant {
                revoke: !g.is_grant,
                roles: roles(&g.granted_roles),
                members: roles(&g.grantee_roles),
                admin: g.is_grant && admin,
                admin_only: !g.is_grant && admin,
            }))
        }
        _ => None,
    }
}

/// The byte index of a whole-word keyword in a lower-cased string, if any.
pub(crate) fn index_word(low: &str, kw: &str) -> Option<usize> {
    let b = low.as_bytes();
    let mut i = 0;
    while i + kw.len() <= b.len() {
        let j = low[i..].find(kw)?;
        let p = i + j;
        let before_ok = p == 0 || !is_ident_part(b[p - 1]);
        let after_ok = p + kw.len() == b.len() || !is_ident_part(b[p + kw.len()]);
        if before_ok && after_ok {
            return Some(p);
        }
        i = p + kw.len();
    }
    None
}
