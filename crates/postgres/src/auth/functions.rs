//! LANGUAGE sql functions and privileges: who may call one, who may change it, and
//! what its body reads, with whose privileges.
//!
//! A call is inlined: the engine stores the definitions and finds the calls, and this
//! module puts each body in its call's place before the statement runs, so that what
//! the body reads is in the statement the privilege check and row-level security see.
//! As in Postgres:
//!
//! - A SECURITY INVOKER body (the default) runs with the caller's privileges: it is
//!   inlined and checked as part of the statement, and read through the caller's
//!   policies.
//! - A SECURITY DEFINER body runs with its owner's: it is checked on its own as the
//!   owner, read through the owner's policies, and `current_user` in it is the owner.
//!   Its arguments are the caller's.
//! - Calling a function needs EXECUTE on it, which PUBLIC holds until it is revoked,
//!   and its owner always.
//! - Only its owner (or a role that bypasses the checks) replaces, drops, alters or
//!   grants on a function. A function from before owners were kept has none, and is a
//!   superuser's.
//!
//! A body the privilege check cannot read (more than one statement, or one Postgres'
//! grammar refuses) is refused to a caller that does not own the function. A call
//! nested deeper than [`MAX_DEPTH`] is refused, where Postgres runs out of stack.

use crate::auth::columns::accesses;
use crate::auth::privileges::StoredRelations;
use crate::error::{Result, errf};
use crate::session::Session;
use overlite_core::text::eq_fold;
use overlite_core::{Command, SqlState, Value};
use overlite_engine::{function_ddl_target, inline_function_body};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::cell::RefCell;
use std::collections::HashMap;

/// How deep calls may nest, a body's calls counted: past it the statement is refused.
pub(crate) const MAX_DEPTH: usize = 16;

/// What the calls of a statement are inlined for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inline {
    /// The privilege check: invoker bodies are inlined, definer bodies checked on their
    /// own as their owners and left as a call of no function, with the arguments.
    Check,
    /// Running the statement: each body read through the policies of the role it runs
    /// as.
    Exec,
    /// Describing it, or storing it (a view, a trigger): bodies as written.
    Plain,
}

/// A stored LANGUAGE sql function.
#[derive(Debug, Clone)]
pub(crate) struct FuncDef {
    name: String,
    params: Vec<String>,
    body: String,
    /// Empty for a function from before owners were kept.
    owner: String,
    secdef: bool,
    /// The roles holding EXECUTE, lower-cased; None for the default, PUBLIC.
    acl: Option<Vec<String>>,
}

/// Postgres' keywords that never name a call: `x IN (a, b)` is no call of a function
/// "in", nor `SET (a, b) = ...` of one "set". A function may be named so (quoted, in
/// Postgres), and is stored, but a call of that name is never inlined.
fn not_a_function(name: &str) -> bool {
    const SYNTAX: &[&str] = &[
        "over",
        "filter",
        "within",
        "set",
        "conflict",
        "key",
        "include",
        "sets",
        "cube",
        "rollup",
        "enum",
        "options",
        "exclude",
        "ordinality",
    ];
    std::thread_local! {
        static KEYWORDS: RefCell<HashMap<String, bool>> = RefCell::new(HashMap::new());
    }
    let low = name.to_ascii_lowercase();
    if SYNTAX.contains(&low.as_str()) {
        return true;
    }
    if let Some(k) = KEYWORDS.with(|k| k.borrow().get(&low).copied()) {
        return k;
    }
    let keyword = pg_query::scan(&low).is_ok_and(|r| {
        r.tokens.first().is_some_and(|t| {
            t.keyword_kind != pb::KeywordKind::NoKeyword as i32
                && t.keyword_kind != pb::KeywordKind::UnreservedKeyword as i32
        })
    });
    KEYWORDS.with(|k| k.borrow_mut().insert(low, keyword));
    keyword
}

impl Session {
    /// Runs f as role, as a SECURITY DEFINER body or a view runs as its owner.
    pub(crate) fn as_role<T>(&mut self, role: &str, f: impl FnOnce(&mut Self) -> T) -> T {
        if role.is_empty() || eq_fold(role, &self.current_role) {
            return f(self);
        }
        let saved = std::mem::replace(&mut self.current_role, role.to_string());
        let out = f(self);
        self.current_role = saved;
        out
    }

    /// Whether sql calls a stored function.
    pub(crate) fn calls_functions(&self, sql: &str) -> bool {
        self.db.has_sql_functions() && !self.db.function_calls(sql, &not_a_function).is_empty()
    }

    /// sql with every call of a stored function inlined, for mode.
    pub(crate) fn inline_functions(&mut self, sql: &str, mode: Inline) -> Result<String> {
        self.inline_at(sql, mode, 0)
    }

    fn inline_at(&mut self, sql: &str, mode: Inline, depth: usize) -> Result<String> {
        if !self.db.has_sql_functions() {
            return Ok(sql.to_string());
        }
        let calls = self.db.function_calls(sql, &not_a_function);
        if calls.is_empty() {
            return Ok(sql.to_string());
        }
        if depth >= MAX_DEPTH {
            return Err(errf!(
                SqlState::STATEMENT_TOO_COMPLEX,
                "function calls nest deeper than {MAX_DEPTH}"
            ));
        }
        let mut out = String::with_capacity(sql.len() * 2);
        let mut prev = 0;
        for call in calls {
            out.push_str(&sql[prev..call.start]);
            prev = call.end;
            let mut args = Vec::with_capacity(call.args.len());
            for a in &call.args {
                args.push(self.inline_at(a, mode, depth + 1)?);
            }
            let Some(f) = self.function_def(&call.name, args.len()) else {
                // No stored function takes that many arguments: the call is someone
                // else's, and only its arguments are inlined.
                let open = call.start + sql[call.start..].find('(').unwrap_or(0);
                out.push_str(&sql[call.start..=open]);
                out.push_str(&args.join(", "));
                out.push(')');
                continue;
            };
            if mode != Inline::Plain && !self.may_execute(&f) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "permission denied for function {}",
                    f.name
                ));
            }
            let trusted = self.priv_bypass() || self.owns_function(&f);
            let role = if f.secdef { f.owner.clone() } else { String::new() };
            let body = self.as_role(&role, |s| s.function_body(&f, mode, depth + 1, trusted))?;
            if mode == Inline::Check && f.secdef {
                // Checked as its owner; the arguments remain the caller's to read.
                out.push_str("overlite_definer(");
                out.push_str(&args.join(", "));
                out.push(')');
                continue;
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            out.push('(');
            out.push_str(&inline_function_body(&[], &body, &refs));
            out.push(')');
        }
        out.push_str(&sql[prev..]);
        Ok(out)
    }

    /// f's body as the role it runs as reads it, its parameters left as $1..$n.
    fn function_body(&mut self, f: &FuncDef, mode: Inline, depth: usize, trusted: bool) -> Result<String> {
        let holders: Vec<String> = (1..=f.params.len()).map(|i| format!("${i}")).collect();
        let holders: Vec<&str> = holders.iter().map(String::as_str).collect();
        let template = inline_function_body(&f.params, &f.body, &holders);
        match mode {
            Inline::Plain => self.inline_at(&template, mode, depth),
            Inline::Exec => {
                // Policies first: the bodies inlined into this one are read through
                // their own roles' policies, and must not be read through these.
                let body = self.apply_rls(&template);
                let body = if f.secdef {
                    self.expand_session_user(&body)
                } else {
                    body
                };
                self.inline_at(&body, mode, depth)
            }
            Inline::Check => {
                let body = self.inline_at(&template, mode, depth)?;
                if !trusted && !self.priv_bypass() {
                    let mut rels = StoredRelations::new(self, false);
                    if accesses(&body, &mut rels).is_none() {
                        return Err(errf!(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            "the privileges the body of function {} needs cannot be checked",
                            f.name
                        ));
                    }
                }
                if f.secdef {
                    self.check_statement(&body, depth)?;
                }
                Ok(body)
            }
        }
    }

    /// The stored LANGUAGE sql function name with arity parameters, as this
    /// connection sees it.
    fn function_def(&mut self, name: &str, arity: usize) -> Option<FuncDef> {
        let rs = self
            .exec(
                "SELECT name, params, body, coalesce(owner, ''), coalesce(secdef, 0), acl FROM _overlite_functions WHERE name = ?1 AND arity = ?2 AND lower(lang) = 'sql'",
                &[Value::from(name), Value::Int(arity as i64)],
            )
            .ok()?;
        let row = rs.rows.first()?;
        let text = |v: &Value| v.to_text_lossy();
        let params = text(&row[1]);
        Some(FuncDef {
            name: text(&row[0]),
            params: if params.is_empty() {
                vec![String::new(); arity]
            } else {
                params.split('\x1f').map(str::to_string).collect()
            },
            body: text(&row[2]),
            owner: text(&row[3]),
            secdef: crate::auth::privileges::truthy(&row[4]),
            acl: (!row[5].is_null()).then(|| {
                text(&row[5])
                    .split(',')
                    .filter(|g| !g.is_empty())
                    .map(str::to_lowercase)
                    .collect()
            }),
        })
    }

    fn owns_function(&mut self, f: &FuncDef) -> bool {
        !f.owner.is_empty() && self.effective_roles().iter().any(|r| eq_fold(r, &f.owner))
    }

    /// Whether the current role holds EXECUTE on f.
    fn may_execute(&mut self, f: &FuncDef) -> bool {
        if self.priv_bypass() || self.owns_function(f) {
            return true;
        }
        let Some(acl) = &f.acl else { return true };
        if acl.iter().any(|g| g == "public") {
            return true;
        }
        let roles = self.effective_roles();
        acl.iter().any(|g| roles.iter().any(|r| eq_fold(r, g)))
    }

    /// The owner check of CREATE OR REPLACE and DROP FUNCTION, before the engine runs
    /// them: only the owner of a function that exists may replace or drop it.
    pub(crate) fn check_function_ddl(&mut self, sql: &str) -> Result<()> {
        let Some((name, arity, _)) = function_ddl_target(sql) else {
            return Ok(());
        };
        if self.priv_bypass() {
            return Ok(());
        }
        let rs = self.exec(
            "SELECT name, coalesce(owner, '') FROM _overlite_functions WHERE name = ?1 AND (arity = ?2 OR ?2 = -1)",
            &[Value::from(name.as_str()), Value::Int(arity)],
        )?;
        let roles = self.effective_roles();
        for row in &rs.rows {
            let owner = row[1].to_text_lossy();
            if !roles.iter().any(|r| eq_fold(r, &owner)) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be owner of function {}",
                    row[0].to_text_lossy()
                ));
            }
        }
        Ok(())
    }

    /// Records the creator as the owner of a function a CREATE FUNCTION made; one it
    /// replaced keeps its owner.
    pub(crate) fn record_function_owner(&mut self, sql: &str) {
        let Some((name, arity, true)) = function_ddl_target(sql) else {
            return;
        };
        let role = self.current_role.clone();
        let _ = self.exec(
            "UPDATE _overlite_functions SET owner = ?3 WHERE name = ?1 AND arity = ?2 AND coalesce(owner, '') = ''",
            &[
                Value::from(name.as_str()),
                Value::Int(arity),
                Value::from(role.as_str()),
            ],
        );
    }

    /// GRANT and REVOKE EXECUTE ON FUNCTION, and ALTER FUNCTION's OWNER TO and
    /// SECURITY DEFINER | INVOKER. None for any other statement, and for a function
    /// that is not a stored LANGUAGE sql one, which these leave alone as before.
    pub(crate) fn try_function_acl(&mut self, sql: &str) -> Option<Result<Command>> {
        let head = sql.trim_start().as_bytes();
        let starts = |w: &str| head.len() > w.len() && head[..w.len()].eq_ignore_ascii_case(w.as_bytes());
        if !(starts("grant") || starts("revoke") || starts("alter")) {
            return None;
        }
        let parsed = crate::ast::parse::shared(sql).ok()?;
        let [raw] = parsed.stmts.as_slice() else {
            return None;
        };
        let node = raw.stmt.as_deref()?.node.as_ref()?;
        match node {
            NodeEnum::GrantStmt(g) if is_routine(g.objtype) => Some(self.function_grant(g)),
            NodeEnum::AlterFunctionStmt(a) => {
                let func = a.func.as_ref()?;
                let mut secdef = None;
                for act in &a.actions {
                    if let Some(NodeEnum::DefElem(d)) = act.node.as_ref()
                        && d.defname == "security"
                    {
                        secdef = Some(matches!(
                            d.arg.as_deref().and_then(|n| n.node.as_ref()),
                            Some(NodeEnum::Boolean(b)) if b.boolval
                        ));
                    }
                }
                let secdef = secdef?;
                Some(self.alter_function(func, |s, name, arity| {
                    s.exec(
                        "UPDATE _overlite_functions SET secdef = ?3 WHERE name = ?1 AND (arity = ?2 OR ?2 = -1)",
                        &[Value::from(name), Value::Int(arity), Value::Int(i64::from(secdef))],
                    )
                    .map(|_| ())
                }))
            }
            NodeEnum::AlterOwnerStmt(a) if is_routine(a.object_type) => {
                let Some(NodeEnum::ObjectWithArgs(func)) = a.object.as_deref().and_then(|n| n.node.as_ref()) else {
                    return None;
                };
                let new_owner = a.newowner.as_ref().map(|r| r.rolename.clone()).unwrap_or_default();
                Some(self.alter_owner(func, &new_owner))
            }
            _ => None,
        }
    }

    /// The stored functions an ObjectWithArgs names: (name, arity), -1 for every
    /// arity when no argument list is given. Empty when there is none.
    fn named_functions(&mut self, func: &pb::ObjectWithArgs) -> Result<Vec<(String, i64, String)>> {
        let Some(name) = func.objname.last().and_then(|n| match n.node.as_ref() {
            Some(NodeEnum::String(s)) => Some(s.sval.clone()),
            _ => None,
        }) else {
            return Ok(Vec::new());
        };
        let arity = if func.args_unspecified {
            -1
        } else {
            func.objargs.len() as i64
        };
        let rs = self.exec(
            "SELECT name, arity, coalesce(owner, '') FROM _overlite_functions WHERE name = ?1 AND (arity = ?2 OR ?2 = -1) AND lower(lang) = 'sql'",
            &[Value::from(name.as_str()), Value::Int(arity)],
        )?;
        Ok(rs
            .rows
            .iter()
            .map(|r| {
                (
                    r[0].to_text_lossy(),
                    r[1].to_string().parse().unwrap_or(-1),
                    r[2].to_text_lossy(),
                )
            })
            .collect())
    }

    /// Whether the current role may change a function owned by owner.
    fn may_alter_function(&mut self, owner: &str) -> bool {
        self.priv_bypass() || (!owner.is_empty() && self.effective_roles().iter().any(|r| eq_fold(r, owner)))
    }

    fn alter_function(
        &mut self,
        func: &pb::ObjectWithArgs,
        apply: impl Fn(&mut Self, &str, i64) -> Result<()>,
    ) -> Result<Command> {
        for (name, arity, owner) in self.named_functions(func)? {
            if !self.may_alter_function(&owner) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be owner of function {name}"
                ));
            }
            apply(self, &name, arity)?;
        }
        Ok(Command::from("ALTER FUNCTION"))
    }

    /// ALTER FUNCTION ... OWNER TO: by the owner, to a role it may become, or by a
    /// superuser.
    fn alter_owner(&mut self, func: &pb::ObjectWithArgs, new_owner: &str) -> Result<Command> {
        let functions = self.named_functions(func)?;
        if functions.is_empty() {
            return Ok(Command::from("ALTER FUNCTION"));
        }
        if !self.role_exists(new_owner) {
            return Err(errf!(SqlState::UNDEFINED_OBJECT, "role \"{new_owner}\" does not exist"));
        }
        for (name, arity, owner) in functions {
            if !self.may_alter_function(&owner) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be owner of function {name}"
                ));
            }
            if !self.priv_bypass() && !self.effective_roles().iter().any(|r| eq_fold(r, new_owner)) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be able to SET ROLE \"{new_owner}\""
                ));
            }
            self.exec(
                "UPDATE _overlite_functions SET owner = ?3 WHERE name = ?1 AND arity = ?2",
                &[Value::from(name.as_str()), Value::Int(arity), Value::from(new_owner)],
            )?;
        }
        Ok(Command::from("ALTER FUNCTION"))
    }

    /// GRANT or REVOKE EXECUTE (or ALL) ON FUNCTION | ROUTINE | PROCEDURE, and ON ALL
    /// FUNCTIONS IN SCHEMA. As in Postgres, a role that does not own the function
    /// grants nothing, without an error.
    fn function_grant(&mut self, g: &pb::GrantStmt) -> Result<Command> {
        let tag = Command::from(if g.is_grant { "GRANT" } else { "REVOKE" });
        let executes = g.privileges.is_empty()
            || g.privileges.iter().any(|p| {
                matches!(p.node.as_ref(), Some(NodeEnum::AccessPriv(a))
                    if eq_fold(&a.priv_name, "execute") || eq_fold(&a.priv_name, "all"))
            });
        if !executes {
            return Ok(tag);
        }
        let mut grantees = Vec::new();
        for r in &g.grantees {
            let Some(NodeEnum::RoleSpec(r)) = r.node.as_ref() else {
                continue;
            };
            if r.roletype == pb::RoleSpecType::RolespecPublic as i32 {
                grantees.push("public".to_string());
            } else if !self.role_exists(&r.rolename) {
                return Err(errf!(
                    SqlState::UNDEFINED_OBJECT,
                    "role \"{}\" does not exist",
                    r.rolename
                ));
            } else {
                grantees.push(r.rolename.to_lowercase());
            }
        }
        let mut functions = Vec::new();
        if g.targtype == pb::GrantTargetType::AclTargetAllInSchema as i32 {
            let rs = self.exec(
                "SELECT name, arity, coalesce(owner, '') FROM _overlite_functions WHERE lower(lang) = 'sql'",
                &[],
            )?;
            for r in &rs.rows {
                functions.push((
                    r[0].to_text_lossy(),
                    r[1].to_string().parse().unwrap_or(-1),
                    r[2].to_text_lossy(),
                ));
            }
        } else {
            for o in &g.objects {
                if let Some(NodeEnum::ObjectWithArgs(func)) = o.node.as_ref() {
                    functions.extend(self.named_functions(func)?);
                }
            }
        }
        for (name, arity, owner) in functions {
            if !self.may_alter_function(&owner) {
                continue;
            }
            let rs = self.exec(
                "SELECT acl FROM _overlite_functions WHERE name = ?1 AND arity = ?2",
                &[Value::from(name.as_str()), Value::Int(arity)],
            )?;
            let mut acl: Vec<String> = match rs.rows.first().map(|r| &r[0]) {
                Some(v) if !v.is_null() => v
                    .to_text_lossy()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => vec!["public".to_string()],
            };
            for who in &grantees {
                acl.retain(|a| a != who);
                if g.is_grant {
                    acl.push(who.clone());
                }
            }
            self.exec(
                "UPDATE _overlite_functions SET acl = ?3 WHERE name = ?1 AND arity = ?2",
                &[
                    Value::from(name.as_str()),
                    Value::Int(arity),
                    Value::from(acl.join(",")),
                ],
            )?;
        }
        Ok(tag)
    }
}

fn is_routine(objtype: i32) -> bool {
    objtype == pb::ObjectType::ObjectFunction as i32
        || objtype == pb::ObjectType::ObjectRoutine as i32
        || objtype == pb::ObjectType::ObjectProcedure as i32
}
