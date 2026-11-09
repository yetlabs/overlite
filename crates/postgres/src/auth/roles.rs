//! CREATE/ALTER/DROP ROLE|USER|GROUP, translated to the engine's internal
//! _overlite_roles table (so they succeed and show up in \du / pg_roles).

use crate::auth::scram::build_scram_verifier;
use crate::ddl::tree::{self, Role};
use crate::error::{Result, errf};
use crate::session::Session;
use overlite_core::Value;
use overlite_core::text::{eq_fold, quote_literal};
use overlite_core::{Command, SqlState};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// Whether try_role_ddl takes this statement.
pub(crate) fn is_role_ddl(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::role_ddl).unwrap_or_default()
}

/// A role statement as the session runs it, read from its node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RolePlan {
    /// CREATE ROLE, USER (LOGIN unless said otherwise) or GROUP.
    Create {
        name: Role,
        user: bool,
        attrs: Vec<(&'static str, i64)>,
        password: Option<String>,
    },
    /// ALTER ROLE's options, or ALTER ROLE ... SET (no options).
    Alter {
        name: Role,
        attrs: Vec<(&'static str, i64)>,
        password: Option<String>,
    },
    Rename {
        name: String,
        to: String,
    },
    Drop {
        names: Vec<Role>,
        if_exists: bool,
    },
    /// ALTER DEFAULT PRIVILEGES: accepted, kept nowhere.
    DefaultPrivileges,
}

/// The column of _overlite_roles a role option sets, and to what.
fn role_attr(d: &pb::DefElem) -> Option<(&'static str, i64)> {
    let col = match d.defname.as_str() {
        "superuser" => "rolsuper",
        "createdb" => "rolcreatedb",
        "createrole" => "rolcreaterole",
        "canlogin" => "rolcanlogin",
        "inherit" => "rolinherit",
        "isreplication" => "rolreplication",
        "bypassrls" => "rolbypassrls",
        _ => return None,
    };
    Some((col, i64::from(tree::def_bool(d))))
}

/// The options' attributes, each once, the last word winning, and the password
/// (PASSWORD NULL sets none).
fn role_options(options: &[pb::Node]) -> (Vec<(&'static str, i64)>, Option<String>) {
    let mut attrs: Vec<(&'static str, i64)> = Vec::new();
    let mut password = None;
    for d in tree::defs(options) {
        if d.defname == "password" {
            password = tree::def_text(d);
        } else if let Some((col, v)) = role_attr(d) {
            attrs.retain(|(c, _)| *c != col);
            attrs.push((col, v));
        }
    }
    (attrs, password)
}

/// What a role statement asks for: None for a statement that is none.
pub(crate) fn role_plan(n: &NodeEnum) -> Option<RolePlan> {
    use pb::RoleStmtType as T;
    match n {
        NodeEnum::CreateRoleStmt(c) => {
            let (attrs, password) = role_options(&c.options);
            Some(RolePlan::Create {
                name: Role::Named(c.role.clone()),
                user: T::try_from(c.stmt_type) == Ok(T::RolestmtUser),
                attrs,
                password,
            })
        }
        NodeEnum::AlterRoleStmt(a) => {
            let (attrs, password) = role_options(&a.options);
            Some(RolePlan::Alter {
                name: tree::role(a.role.as_ref()?),
                attrs,
                password,
            })
        }
        NodeEnum::AlterRoleSetStmt(a) => Some(RolePlan::Alter {
            name: tree::role(a.role.as_ref()?),
            attrs: Vec::new(),
            password: None,
        }),
        NodeEnum::RenameStmt(r) if tree::object(r.rename_type) == pb::ObjectType::ObjectRole => {
            Some(RolePlan::Rename {
                name: r.subname.clone(),
                to: r.newname.clone(),
            })
        }
        NodeEnum::DropRoleStmt(d) => Some(RolePlan::Drop {
            names: d.roles.iter().filter_map(tree::role_spec).map(tree::role).collect(),
            if_exists: d.missing_ok,
        }),
        NodeEnum::AlterDefaultPrivilegesStmt(_) => Some(RolePlan::DefaultPrivileges),
        _ => None,
    }
}

impl Session {
    /// Handles CREATE/ALTER/DROP ROLE|USER|GROUP and ALTER DEFAULT PRIVILEGES (accepted
    /// as a no-op).
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_role_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_role_ddl(sql)
            && !crate::ast::classify::read(sql, |n| matches!(n, NodeEnum::AlterDefaultPrivilegesStmt(_)))
                .unwrap_or(false)
        {
            return None;
        }
        Some(
            tree::statement(sql, |n| role_plan(n).ok_or_else(tree::syntax_error))
                .and_then(|plan| self.run_role_ddl(plan)),
        )
    }

    fn run_role_ddl(&mut self, plan: RolePlan) -> Result<Command> {
        match plan {
            RolePlan::DefaultPrivileges => Ok(Command::from("ALTER DEFAULT PRIVILEGES")),
            RolePlan::Create {
                name,
                user,
                attrs,
                password,
            } => {
                let name = self.role_named(&name)?;
                self.require_role_admin("CREATE", &name)?;
                self.create_role(&name, attrs, password, user)?;
                self.grant_creator_admin(&name);
                Ok(Command::from("CREATE ROLE"))
            }
            RolePlan::Alter { name, attrs, password } => {
                let name = self.role_named(&name)?;
                self.require_role_admin("ALTER", &name)?;
                self.alter_role(&name, attrs, password)?;
                Ok(Command::from("ALTER ROLE"))
            }
            RolePlan::Rename { name, to } => {
                self.require_role_admin("ALTER", &name)?;
                if !self.role_exists(&name) {
                    if name == self.current_role {
                        return Ok(Command::from("ALTER ROLE"));
                    }
                    return Err(errf!(SqlState::UNDEFINED_OBJECT, "role \"{name}\" does not exist"));
                }
                self.rename_role(&name, &to)?;
                Ok(Command::from("ALTER ROLE"))
            }
            RolePlan::Drop { names, if_exists } => {
                let mut roles = Vec::with_capacity(names.len());
                for n in &names {
                    roles.push(self.role_named(n)?);
                }
                self.require_role_admin("DROP", roles.first().map_or("", String::as_str))?;
                for role in &roles {
                    self.drop_role(role, if_exists)?;
                }
                Ok(Command::from("DROP ROLE"))
            }
        }
    }

    /// The role a specification names: CURRENT_USER and the like stand for one.
    fn role_named(&self, r: &Role) -> Result<String> {
        Ok(match r {
            Role::Named(n) => n.clone(),
            Role::Current => self.current_role.clone(),
            Role::Session => self.session_user.clone(),
            Role::Public => {
                return Err(errf!(SqlState::RESERVED_NAME, "role name \"public\" is reserved"));
            }
        })
    }

    /// Enforces who may manage roles: a superuser or unmanaged role (bypass) or a
    /// role with CREATEROLE. A role may always ALTER itself (e.g. to change its
    /// own password).
    fn require_role_admin(&mut self, verb: &str, target: &str) -> Result<()> {
        let cur = self.current_role.clone();
        if self.role_bypasses(&cur) || self.has_create_role(&cur) {
            return Ok(());
        }
        if verb == "ALTER" && !target.is_empty() && eq_fold(target, &self.current_role) {
            return Ok(());
        }
        Err(errf!(
            SqlState::INSUFFICIENT_PRIVILEGE,
            "permission denied to {} role",
            verb.to_lowercase()
        ))
    }

    /// Gives a non-bypass creator ADMIN OPTION (an inheriting membership) on the
    /// role it just created, so it can administer and grant it.
    fn grant_creator_admin(&mut self, name: &str) {
        let cur = self.current_role.clone();
        if name.is_empty() || self.role_bypasses(&cur) {
            return;
        }
        let q = format!(
            "INSERT INTO _overlite_memberships (member, roleof, admin_option) VALUES ({}, {}, 1)",
            quote_literal(&cur),
            quote_literal(name)
        );
        let _ = self.exec(&q, &[]);
    }

    fn create_role(
        &mut self,
        name: &str,
        mut attrs: Vec<(&'static str, i64)>,
        password: Option<String>,
        user: bool,
    ) -> Result<()> {
        if name == "public" || name == "none" || name.starts_with("pg_") {
            return Err(errf!(SqlState::RESERVED_NAME, "role name \"{name}\" is reserved"));
        }
        if user && !attrs.iter().any(|(c, _)| *c == "rolcanlogin") {
            attrs.push(("rolcanlogin", 1)); // CREATE USER defaults to LOGIN
        }

        let mut cols: Vec<String> = vec!["rolname".to_string()];
        let mut ph: Vec<String> = vec!["?".to_string()];
        let mut vals: Vec<Value> = vec![Value::Text(name.to_string())];
        for (col, v) in attrs {
            cols.push(col.to_string());
            ph.push("?".to_string());
            vals.push(Value::Int(v));
        }
        if let Some(pw) = password {
            let verifier = build_scram_verifier(&pw)?;
            cols.push("rolpassword".to_string());
            ph.push("?".to_string());
            vals.push(Value::Text(verifier));
        }
        let q = format!(
            "INSERT INTO _overlite_roles ({}) VALUES ({})",
            cols.join(","),
            ph.join(",")
        );
        self.exec(&q, &vals).map_err(|e| match e.sql_state() {
            SqlState::UNIQUE_VIOLATION => errf!(SqlState::DUPLICATE_OBJECT, "role \"{name}\" already exists"),
            _ => e,
        })?;
        Ok(())
    }

    fn alter_role(&mut self, name: &str, attrs: Vec<(&'static str, i64)>, password: Option<String>) -> Result<()> {
        if !self.role_exists(name) {
            // The role logged in may be one the catalog never held.
            if name == self.current_role {
                return Ok(());
            }
            return Err(errf!(SqlState::UNDEFINED_OBJECT, "role \"{name}\" does not exist"));
        }

        let mut set: Vec<String> = Vec::with_capacity(2);
        let mut vals: Vec<Value> = Vec::with_capacity(3);
        if let Some(pw) = password {
            let verifier = build_scram_verifier(&pw)?;
            set.push("rolpassword = ?".to_string());
            vals.push(Value::Text(verifier));
        }
        for (col, v) in attrs {
            set.push(format!("{col} = ?"));
            vals.push(Value::Int(v));
        }
        if set.is_empty() {
            return Ok(()); // ALTER ROLE ... SET config / other: accept as no-op
        }
        vals.push(Value::Text(name.to_string()));
        let q = format!("UPDATE _overlite_roles SET {} WHERE rolname = ?", set.join(", "));
        self.exec(&q, &vals)?;
        Ok(())
    }

    /// Moves a role and what names it to its new name: Postgres keeps these by oid.
    fn rename_role(&mut self, name: &str, to: &str) -> Result<()> {
        if self.role_exists(to) {
            return Err(errf!(SqlState::DUPLICATE_OBJECT, "role \"{to}\" already exists"));
        }
        for q in [
            "UPDATE _overlite_roles SET rolname = ?1 WHERE rolname = ?2",
            "UPDATE _overlite_memberships SET member = ?1 WHERE member = ?2",
            "UPDATE _overlite_memberships SET roleof = ?1 WHERE roleof = ?2",
            "UPDATE _overlite_grants SET grantee = ?1 WHERE grantee = ?2",
            "UPDATE _overlite_column_grants SET grantee = ?1 WHERE grantee = ?2",
            "UPDATE _overlite_owners SET owner = ?1 WHERE owner = ?2",
        ] {
            self.exec(q, &[Value::from(to), Value::from(name)])?;
        }
        Ok(())
    }

    fn drop_role(&mut self, name: &str, if_exists: bool) -> Result<()> {
        if !self.role_exists(name) {
            if if_exists {
                self.c
                    .send_notice(&format!("role \"{name}\" does not exist, skipping"))?;
                return Ok(());
            }
            return Err(errf!(SqlState::UNDEFINED_OBJECT, "role \"{name}\" does not exist"));
        }
        self.exec("DELETE FROM _overlite_roles WHERE rolname = ?", &[Value::from(name)])?;
        // Drop any membership edges touching the removed role.
        self.exec(
            "DELETE FROM _overlite_memberships WHERE member = ? OR roleof = ?",
            &[Value::from(name), Value::from(name)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn plan(sql: &str) -> RolePlan {
        tree::read(sql, role_plan).unwrap()
    }

    #[test]
    fn passwords_and_options() {
        let RolePlan::Alter { password, .. } = plan("ALTER ROLE bob ENCRYPTED PASSWORD 'it''s me'") else {
            panic!()
        };
        assert_eq!(password.as_deref(), Some("it's me"));
        let RolePlan::Alter { password, attrs, .. } = plan("ALTER ROLE bob PASSWORD NULL CREATEDB") else {
            panic!()
        };
        assert_eq!((password, attrs), (None, vec![("rolcreatedb", 1)]));
        // A quoted word is a name, never an option.
        assert_eq!(
            plan(r#"CREATE ROLE "password" NOLOGIN"#),
            RolePlan::Create {
                name: Role::Named("password".into()),
                user: false,
                attrs: vec![("rolcanlogin", 0)],
                password: None,
            }
        );
        assert_eq!(
            plan(r#"DROP USER IF EXISTS "A b", c, CURRENT_USER"#),
            RolePlan::Drop {
                names: vec![Role::Named("A b".into()), Role::Named("c".into()), Role::Current],
                if_exists: true,
            }
        );
    }
}
