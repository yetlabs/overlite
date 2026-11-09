//! Row-level security. SQLite has none, so overlite records the flags/policies
//! (see the engine's catalog::rls) and injects each policy's USING/WITH CHECK expression:
//!   - SELECT: RLS tables in FROM/JOIN are wrapped in a filtering subquery.
//!   - UPDATE/DELETE: the USING expression is ANDed into the WHERE clause.
//!   - INSERT: the WITH CHECK (or USING) expression validates every new row.
//!
//! A superuser, a BYPASSRLS role, an unmanaged role, or the table owner (unless
//! FORCE) is not subject. With RLS on and no permissive policy, access is denied.

use crate::auth::functions::Inline;
use crate::auth::privileges::{Privilege, lowercase, tables_after, truthy};
use crate::ddl::alter::split_qualified_ref;
use crate::ddl::sequences::{is_ident_part, is_ident_start};
use crate::ddl::tree::{self, Role};
use crate::error::{Error, Result, errf};
use crate::rewrite::rewrite;
use crate::session::Session;
use crate::session::utility::{first_word_upper, second_word_upper};
use overlite_core::text::{
    end_of_string_literal, eq_fold, quote_literal, quoted, slice, text_from_bytes, trim_chars, trim_end_chars,
};
use overlite_core::{Command, ResultSet, SqlState, Value};
use overlite_engine::code_only;
use pg_query::NodeEnum;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Whether sql is an RLS DDL statement handled by try_rls_ddl.
pub(crate) fn is_rls_ddl(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::rls_ddl).unwrap_or_else(|| is_rls_ddl_text(sql))
}

/// `is_rls_ddl` by the text, for what Postgres' grammar does not read.
pub(crate) fn is_rls_ddl_text(sql: &str) -> bool {
    if eq_fold(&first_word_upper(sql), "ALTER")
        && eq_fold(&second_word_upper(sql), "TABLE")
        && code_only(sql).to_lowercase().contains("row level security")
    {
        return true;
    }
    let (w, w2) = (first_word_upper(sql), second_word_upper(sql));
    w2 == "POLICY" && (w == "CREATE" || w == "DROP" || w == "ALTER")
}

/// What a policy applies to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PolicyCommand {
    #[default]
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl PolicyCommand {
    /// A policy's command, as `CREATE POLICY ... FOR` writes it and the catalog holds it.
    fn parse(word: &str) -> Option<PolicyCommand> {
        match word.to_ascii_uppercase().as_str() {
            "ALL" => Some(PolicyCommand::All),
            "SELECT" => Some(PolicyCommand::Select),
            "INSERT" => Some(PolicyCommand::Insert),
            "UPDATE" => Some(PolicyCommand::Update),
            "DELETE" => Some(PolicyCommand::Delete),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            PolicyCommand::All => "ALL",
            PolicyCommand::Select => "SELECT",
            PolicyCommand::Insert => "INSERT",
            PolicyCommand::Update => "UPDATE",
            PolicyCommand::Delete => "DELETE",
        }
    }
}

/// An RLS statement, read from its node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RlsPlan {
    /// ALTER TABLE's row-level security actions, in order.
    Table {
        table: String,
        actions: Vec<RlsAction>,
    },
    Create(PolicyDef),
    Alter(AlterPolicy),
    Drop {
        name: String,
        table: String,
        if_exists: bool,
    },
}

/// What an ALTER TABLE does to row-level security.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RlsAction {
    Enable,
    Disable,
    Force,
    NoForce,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PolicyDef {
    name: String,
    table: String,
    command: PolicyCommand,
    roles: Vec<Role>,
    using: String,
    check: String,
    permissive: bool,
}

/// ALTER POLICY: None in a clause is a clause not given.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AlterPolicy {
    name: String,
    table: String,
    rename: Option<String>,
    roles: Option<Vec<Role>>,
    using: Option<String>,
    check: Option<String>,
}

/// The roles of a policy statement's TO.
fn roles_of(list: &[pg_query::protobuf::Node]) -> Vec<Role> {
    list.iter().filter_map(tree::role_spec).map(tree::role).collect()
}

/// An RLS statement's plan. A policy's expressions are kept as the client wrote them,
/// between the parentheses the tree's clause stands in.
pub(crate) fn rls_plan(sql: &str, n: &NodeEnum) -> Option<RlsPlan> {
    use pg_query::protobuf::{AlterTableType as A, ObjectType};
    match n {
        NodeEnum::AlterTableStmt(a) => {
            let actions: Vec<RlsAction> = a
                .cmds
                .iter()
                .filter_map(|c| match &c.node {
                    Some(NodeEnum::AlterTableCmd(c)) => match A::try_from(c.subtype).ok()? {
                        A::AtEnableRowSecurity => Some(RlsAction::Enable),
                        A::AtDisableRowSecurity => Some(RlsAction::Disable),
                        A::AtForceRowSecurity => Some(RlsAction::Force),
                        A::AtNoForceRowSecurity => Some(RlsAction::NoForce),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            (!actions.is_empty()).then(|| RlsPlan::Table {
                table: a.relation.as_ref().map(tree::reference).unwrap_or_default(),
                actions,
            })
        }
        NodeEnum::CreatePolicyStmt(c) => {
            let toks = tree::tokens(sql);
            Some(RlsPlan::Create(PolicyDef {
                name: c.policy_name.clone(),
                table: tree::reference(c.table.as_ref()?),
                command: PolicyCommand::parse(&c.cmd_name)?,
                roles: roles_of(&c.roles),
                using: c
                    .qual
                    .is_some()
                    .then(|| tree::parenthesized_after(sql, &toks, &["using"]))
                    .flatten()
                    .unwrap_or_default()
                    .to_string(),
                check: c
                    .with_check
                    .is_some()
                    .then(|| tree::parenthesized_after(sql, &toks, &["with", "check"]))
                    .flatten()
                    .unwrap_or_default()
                    .to_string(),
                permissive: c.permissive,
            }))
        }
        NodeEnum::AlterPolicyStmt(a) => {
            let toks = tree::tokens(sql);
            Some(RlsPlan::Alter(AlterPolicy {
                name: a.policy_name.clone(),
                table: tree::reference(a.table.as_ref()?),
                rename: None,
                roles: (!a.roles.is_empty()).then(|| roles_of(&a.roles)),
                using: a
                    .qual
                    .is_some()
                    .then(|| tree::parenthesized_after(sql, &toks, &["using"]).map(str::to_string))
                    .flatten(),
                check: a
                    .with_check
                    .is_some()
                    .then(|| tree::parenthesized_after(sql, &toks, &["with", "check"]).map(str::to_string))
                    .flatten(),
            }))
        }
        NodeEnum::RenameStmt(r) if tree::object(r.rename_type) == ObjectType::ObjectPolicy => {
            Some(RlsPlan::Alter(AlterPolicy {
                name: r.subname.clone(),
                table: tree::reference(r.relation.as_ref()?),
                rename: Some(r.newname.clone()),
                ..Default::default()
            }))
        }
        NodeEnum::DropStmt(d) if tree::object(d.remove_type) == ObjectType::ObjectPolicy => {
            // One policy, its table's name and its own: [schema,] table, policy.
            let [o] = d.objects.as_slice() else { return None };
            let Some(NodeEnum::List(l)) = &o.node else { return None };
            let mut parts = tree::strings(&l.items);
            let name = parts.pop()?;
            Some(RlsPlan::Drop {
                name,
                table: tree::reference_of(&parts),
                if_exists: d.missing_ok,
            })
        }
        _ => None,
    }
}

impl Session {
    /// Handles ALTER TABLE ... ROW LEVEL SECURITY and CREATE/DROP/ALTER POLICY,
    /// which SQLite cannot run.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_rls_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_rls_ddl(sql) {
            return None;
        }
        let plan = tree::statement(sql, |n| rls_plan(sql, n).ok_or_else(|| tree::parse_failure(sql)));
        Some(plan.and_then(|plan| {
            match plan {
                RlsPlan::Table { table, actions } => {
                    // Who may do this is settled by check_alter_table, before any route.
                    let table = self.stored_name(&table);
                    let r = self.alter_table_rls_write(&table, &actions);
                    self.rls_changed();
                    r.map(|()| Command::from("ALTER TABLE"))
                }
                RlsPlan::Create(p) => self.create_policy(p).map(|()| Command::from("CREATE POLICY")),
                RlsPlan::Alter(a) => self.alter_policy(a).map(|()| Command::from("ALTER POLICY")),
                RlsPlan::Drop { name, table, if_exists } => self
                    .drop_policy(&name, &table, if_exists)
                    .map(|()| Command::from("DROP POLICY")),
            }
        }))
    }

    fn alter_table_rls_write(&mut self, table: &str, actions: &[RlsAction]) -> Result<()> {
        let q = format!(
            "INSERT OR IGNORE INTO _overlite_rls (tablename, enabled, forced) VALUES ({}, 0, 0)",
            quote_literal(table)
        );
        self.exec(&q, &[])?;
        for action in actions {
            let set = match action {
                RlsAction::Enable => "enabled = 1",
                RlsAction::Disable => "enabled = 0",
                RlsAction::Force => "forced = 1",
                RlsAction::NoForce => "forced = 0",
            };
            let q = format!(
                "UPDATE _overlite_rls SET {} WHERE lower(tablename) = lower({})",
                set,
                quote_literal(table)
            );
            self.exec(&q, &[])?;
        }
        Ok(())
    }

    fn create_policy(&mut self, p: PolicyDef) -> Result<()> {
        let table = self.stored_name(&p.table);
        let cur = self.current_role.clone();
        if !self.role_bypasses(&cur) {
            let roles = self.effective_roles();
            if !self.owns_table(&table, &roles) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be owner of table {:?}",
                    quoted(table)
                ));
            }
        }
        let q = format!(
            "INSERT INTO _overlite_policies (polname, tablename, command, roles, permissive, using_expr, check_expr) VALUES ({}, {}, {}, {}, {}, {}, {})",
            quote_literal(&p.name),
            quote_literal(&table),
            quote_literal(p.command.as_str()),
            quote_literal(&self.created_policy_roles(&p.roles)),
            i32::from(p.permissive),
            quote_literal(&p.using),
            quote_literal(&p.check)
        );
        self.exec(&q, &[])?;
        Ok(())
    }

    /// ALTER POLICY name ON table RENAME TO new, or [TO roles] [USING (expr)] [WITH
    /// CHECK (expr)]: what a clause names changes, and only that.
    fn alter_policy(&mut self, a: AlterPolicy) -> Result<()> {
        let table = self.stored_name(&a.table);
        let (_, bare) = split_qualified_ref(&a.table);
        if self.table_ddl(&table).is_empty() {
            return Err(Error::with_state(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{bare}\" does not exist"),
            ));
        }
        let cur = self.current_role.clone();
        if !self.role_bypasses(&cur) {
            let roles = self.effective_roles();
            if !self.owns_table(&table, &roles) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be owner of table {:?}",
                    quoted(table)
                ));
            }
        }
        let name_is = format!(
            "lower(polname) = lower({}) AND lower(tablename) = lower({})",
            quote_literal(&a.name),
            quote_literal(&table)
        );
        let rs = self.exec(&format!("SELECT command FROM _overlite_policies WHERE {name_is}"), &[])?;
        let Some(row) = rs.rows.first() else {
            return Err(Error::with_state(
                SqlState::UNDEFINED_OBJECT,
                format!("policy \"{}\" for table \"{bare}\" does not exist", a.name),
            ));
        };
        let command = PolicyCommand::parse(&text_or_empty(&row[0])).unwrap_or_default();
        if a.check.is_some() && matches!(command, PolicyCommand::Select | PolicyCommand::Delete) {
            return Err(Error::with_state(
                SqlState::SYNTAX_ERROR,
                "only USING expression allowed for SELECT, DELETE",
            ));
        }
        if a.using.is_some() && command == PolicyCommand::Insert {
            return Err(Error::with_state(
                SqlState::SYNTAX_ERROR,
                "only WITH CHECK expression allowed for INSERT",
            ));
        }

        let mut set = Vec::new();
        if let Some(new) = &a.rename {
            let q = format!(
                "SELECT 1 FROM _overlite_policies WHERE lower(polname) = lower({}) AND lower(tablename) = lower({})",
                quote_literal(new),
                quote_literal(&table)
            );
            if !self.exec(&q, &[])?.rows.is_empty() {
                return Err(Error::with_state(
                    SqlState::DUPLICATE_OBJECT,
                    format!("policy \"{new}\" for table \"{bare}\" already exists"),
                ));
            }
            set.push(format!("polname = {}", quote_literal(new)));
        }
        if let Some(spec) = &a.roles {
            set.push(format!("roles = {}", quote_literal(&self.policy_roles(spec)?)));
        }
        if let Some(expr) = &a.using {
            set.push(format!("using_expr = {}", quote_literal(expr)));
        }
        if let Some(expr) = &a.check {
            set.push(format!("check_expr = {}", quote_literal(expr)));
        }
        if set.is_empty() {
            return Ok(());
        }
        let q = format!("UPDATE _overlite_policies SET {} WHERE {name_is}", set.join(", "));
        self.exec(&q, &[])?;
        Ok(())
    }

    /// The role list of ALTER POLICY as a policy row holds it. A name that is no role
    /// is refused as in Postgres: a restrictive policy kept from the role it was meant
    /// for would open what it guards. The connected user counts, known or not.
    fn policy_roles(&mut self, spec: &[Role]) -> Result<String> {
        let mut out = Vec::new();
        for role in spec {
            let role = match role {
                Role::Public => return Ok(String::new()),
                Role::Current => {
                    out.push(self.current_role.to_lowercase());
                    continue;
                }
                Role::Session => {
                    out.push(self.session_user.to_lowercase());
                    continue;
                }
                Role::Named(n) => n.to_lowercase(),
            };
            let role = role.as_str();
            match role {
                _ if eq_fold(role, &self.auth_user) || eq_fold(role, &self.session_user) => out.push(role.to_string()),
                _ if eq_fold(role, &self.current_role) || self.role_exists(role) => out.push(role.to_string()),
                _ => {
                    return Err(Error::with_state(
                        SqlState::UNDEFINED_OBJECT,
                        format!("role \"{role}\" does not exist"),
                    ));
                }
            }
        }
        Ok(out.join(","))
    }

    /// The roles of a new policy as its row holds them: names in lower case, none for
    /// PUBLIC alone, CURRENT_USER and SESSION_USER the roles they stand for.
    fn created_policy_roles(&self, roles: &[Role]) -> String {
        if roles == [Role::Public] {
            return String::new();
        }
        let names: Vec<String> = roles
            .iter()
            .map(|r| match r {
                Role::Public => "public".to_string(),
                Role::Current => self.current_role.to_lowercase(),
                Role::Session => self.session_user.to_lowercase(),
                Role::Named(n) => n.to_lowercase(),
            })
            .collect();
        names.join(",")
    }

    fn drop_policy(&mut self, name: &str, table: &str, if_exists: bool) -> Result<()> {
        let name = name.to_string();
        let table = self.stored_name(table);
        let cur = self.current_role.clone();
        if !self.role_bypasses(&cur) {
            let roles = self.effective_roles();
            if !self.owns_table(&table, &roles) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be owner of table {:?}",
                    quoted(table)
                ));
            }
        }
        let q = format!(
            "DELETE FROM _overlite_policies WHERE lower(polname) = lower({}) AND lower(tablename) = lower({})",
            quote_literal(&name),
            quote_literal(&table)
        );
        let dropped = self.exec(&q, &[])?.rows_affected > 0;
        if !dropped && if_exists {
            let message = format!("policy \"{name}\" for relation \"{table}\" does not exist, skipping");
            self.c.send_notice(&message)?;
        } else if !dropped {
            return Err(errf!(
                SqlState::UNDEFINED_OBJECT,
                "policy \"{name}\" for table \"{table}\" does not exist"
            ));
        }
        Ok(())
    }

    // --- enforcement --------------------------------------------------------

    /// Rewrites a statement to enforce row-level security. INSERT is handled
    /// separately (try_rls_insert) since it needs a runtime row check.
    pub(crate) fn apply_rls(&mut self, sql: &str) -> String {
        match first_word_upper(sql).as_str() {
            "SELECT" | "WITH" | "TABLE" | "VALUES" => self.wrap_rls_select(sql),
            // What an UPDATE or DELETE reads besides its target (a subquery, FROM,
            // USING) is read through its own policies, as in Postgres.
            "UPDATE" => {
                let read = self.wrap_rls_tables(sql, false);
                self.inject_rls_where(&read, PolicyCommand::Update)
            }
            "DELETE" => {
                let read = self.wrap_rls_tables(sql, true);
                self.inject_rls_where(&read, PolicyCommand::Delete)
            }
            // Filter the read side of INSERT ... SELECT: wrap_rls_select only touches
            // tables after FROM/JOIN, i.e. the source query, never the target.
            "INSERT" => self.wrap_rls_select(sql),
            // CREATE TABLE AS reads its query as a SELECT does.
            "CREATE" if crate::auth::privileges::create_table_as_query(sql).is_some() => self.wrap_rls_select(sql),
            _ => sql.to_string(),
        }
    }

    /// Whether table is known to have no row-level security, which spares a query
    /// on every statement. Anything less certain answers false and leaves the
    /// decision to the database: inside a transaction, because the index holds
    /// what is committed and this session may have changed it.
    fn rls_known_off(&mut self, table: &str) -> bool {
        if self.in_tx() {
            return false;
        }
        matches!(self.db.rls_enabled(table), Ok(false))
    }

    /// Records that this session changed which tables have RLS. Other sessions
    /// must not see that before it commits, so inside a transaction the index is
    /// only told when the transaction ends.
    pub(crate) fn rls_changed(&mut self) {
        if self.in_tx() {
            self.rls_dirty = true;
            return;
        }
        self.invalidate_rls();
    }

    pub(crate) fn invalidate_rls(&mut self) {
        self.db.invalidate_rls();
    }

    /// The combined USING filter for command on table, by its stored name; None when
    /// the current role is not subject to RLS there.
    pub(crate) fn rls_filter(&mut self, table: &str, command: PolicyCommand) -> Option<String> {
        if self.rls_known_off(table) {
            return None;
        }
        let q = format!(
            "SELECT enabled, forced FROM _overlite_rls WHERE lower(tablename) = lower({})",
            quote_literal(table)
        );
        let rs = match self.exec(&q, &[]) {
            Ok(rs) if !rs.rows.is_empty() && truthy(&rs.rows[0][0]) => rs,
            _ => return None,
        };
        let forced = truthy(&rs.rows[0][1]);
        if self.rls_bypasses(table, forced) {
            return None;
        }
        let (perm, restr) = self.gather_policies(table, command, false);
        Some(combine_rls(&perm, &restr))
    }

    /// The WITH CHECK filter for INSERT; None when not subject.
    pub(crate) fn rls_check_expr(&mut self, table: &str) -> Option<String> {
        if self.rls_known_off(table) {
            return None;
        }
        let q = format!(
            "SELECT enabled, forced FROM _overlite_rls WHERE lower(tablename) = lower({})",
            quote_literal(table)
        );
        let rs = match self.exec(&q, &[]) {
            Ok(rs) if !rs.rows.is_empty() && truthy(&rs.rows[0][0]) => rs,
            _ => return None,
        };
        if self.rls_bypasses(table, truthy(&rs.rows[0][1])) {
            return None;
        }
        let (perm, restr) = self.gather_policies(table, PolicyCommand::Insert, true);
        Some(combine_rls(&perm, &restr))
    }

    fn rls_bypasses(&mut self, table: &str, forced: bool) -> bool {
        let cur = self.current_role.clone();
        if self.role_bypasses(&cur) || self.has_bypass_rls(&cur) {
            return true;
        }
        if !forced {
            let roles = self.effective_roles();
            if self.owns_table(table, &roles) {
                return true;
            }
        }
        false
    }

    fn has_bypass_rls(&mut self, role: &str) -> bool {
        let q = format!(
            "SELECT 1 FROM _overlite_roles WHERE lower(rolname) = lower({}) AND rolbypassrls <> 0 LIMIT 1",
            quote_literal(role)
        );
        matches!(self.exec(&q, &[]), Ok(rs) if !rs.rows.is_empty())
    }

    /// The permissive and restrictive expressions that apply to command for the
    /// current role, using WITH CHECK when for_check (falling back to USING).
    fn gather_policies(&mut self, table: &str, command: PolicyCommand, for_check: bool) -> (Vec<String>, Vec<String>) {
        let mut perm = Vec::new();
        let mut restr = Vec::new();
        let q = format!(
            "SELECT roles, permissive, using_expr, check_expr FROM _overlite_policies WHERE lower(tablename) = lower({}) AND upper(command) IN ('ALL', upper({}))",
            quote_literal(table),
            quote_literal(command.as_str())
        );
        let Ok(rs) = self.exec(&q, &[]) else {
            return (perm, restr);
        };
        let mut role_set: HashSet<String> = HashSet::new();
        role_set.insert("public".to_string());
        for r in self.effective_roles() {
            role_set.insert(r.to_lowercase());
        }
        for row in &rs.rows {
            if !policy_role_match(&text_or_empty(&row[0]), &role_set) {
                continue;
            }
            let mut expr = text_or_empty(&row[2]);
            if for_check {
                let c = text_or_empty(&row[3]);
                let c = c.trim();
                if !c.is_empty() {
                    expr = c.to_string();
                }
            }
            if expr.trim().is_empty() {
                expr = "1".to_string(); // a policy with no relevant expression permits the row
            }
            if truthy(&row[1]) {
                perm.push(expr);
            } else {
                restr.push(expr);
            }
        }
        (perm, restr)
    }

    /// Replaces every base-table reference in a FROM/JOIN clause that is under
    /// RLS with a filtering subquery.
    fn wrap_rls_select(&mut self, sql: &str) -> String {
        self.wrap_rls_tables(sql, false)
    }

    /// wrap_rls_select, leaving out a DELETE's target (the table after its first
    /// FROM), whose USING list it reads as a FROM list.
    fn wrap_rls_tables(&mut self, sql: &str, delete: bool) -> String {
        #[derive(Debug)]
        struct Repl {
            start: usize,
            end: usize,
            text: String,
        }
        let toks = rls_tokens(sql);
        let mut reps: Vec<Repl> = Vec::new();
        let (mut in_from_list, mut expect_table) = (false, false);
        // A DELETE's target: not yet passed, then passed and its USING still to come.
        let (mut target_pending, mut after_target) = (delete, false);
        for i in 0..toks.len() {
            let t = &toks[i];
            match t.kind {
                b'w' => {
                    let w = lowercase(t.text);
                    if after_target && w == "using" {
                        after_target = false;
                        in_from_list = true;
                        expect_table = true;
                        continue;
                    }
                    match w.as_ref() {
                        "from" => {
                            in_from_list = true;
                            expect_table = true;
                            continue;
                        }
                        "join" => {
                            expect_table = true;
                            continue;
                        }
                        "only" | "lateral" => continue, // modifiers before the table name
                        "where" | "group" | "having" | "order" | "limit" | "window" | "union" | "except"
                        | "intersect" | "on" | "using" | "returning" | "set" | "values" => {
                            in_from_list = false;
                            expect_table = false;
                            continue;
                        }
                        _ => {}
                    }
                    if expect_table && target_pending {
                        target_pending = false;
                        after_target = true;
                        in_from_list = false;
                        expect_table = false;
                        continue;
                    }
                    if expect_table {
                        if !bare_table_name(t.text).is_empty() {
                            let stored = self.stored_name(t.text);
                            if let Some(expr) = self.rls_filter(&stored, PolicyCommand::Select) {
                                let mut r = format!("(SELECT * FROM {} WHERE {})", t.text, expr);
                                if !alias_follows(&toks, i) {
                                    r.push_str(" AS ");
                                    r.push_str(&bare_alias(t.text));
                                }
                                reps.push(Repl {
                                    start: t.start,
                                    end: t.end,
                                    text: r,
                                });
                            }
                        }
                        expect_table = false;
                    }
                }
                b'(' => expect_table = false, // derived table; its inner FROM is scanned too
                b',' if in_from_list => {
                    expect_table = true;
                }
                _ => {}
            }
        }
        if reps.is_empty() {
            return sql.to_string();
        }
        let mut b = String::with_capacity(sql.len());
        let mut prev = 0;
        for r in &reps {
            b.push_str(&sql[prev..r.start]);
            b.push_str(&r.text);
            prev = r.end;
        }
        b.push_str(&sql[prev..]);
        b
    }

    /// ANDs the USING filter for the target table into an UPDATE or DELETE,
    /// before any RETURNING clause.
    fn inject_rls_where(&mut self, sql: &str, command: PolicyCommand) -> String {
        let kw = if command == PolicyCommand::Delete {
            "from"
        } else {
            "update"
        };
        let privilege = if command == PolicyCommand::Delete {
            Privilege::Delete
        } else {
            Privilege::Update
        };
        let tps = tables_after(sql, &[kw], privilege);
        if tps.is_empty() {
            return sql.to_string();
        }
        let table = self.stored_name(tps[0].table);
        let Some(expr) = self.rls_filter(&table, command) else {
            return sql.to_string();
        };
        let trimmed = trim_end_chars(sql, "; \t\n");
        let semi = &sql[trimmed.len()..];
        let mut ret = "";
        let mut body = trimmed;
        if let Some(ri) = top_level_keyword(body, "returning") {
            ret = &body[ri..];
            body = &body[..ri];
        }
        let mut out;
        if let Some(w) = top_level_keyword(body, "where") {
            let after = w + "where".len();
            let cond = body[after..].trim();
            out = format!("{} (({}) AND ({}))", &body[..after], expr, cond);
        } else {
            out = format!("{} WHERE ({})", trim_end_chars(body, " \t\n"), expr);
        }
        if !ret.is_empty() {
            out.push(' ');
            out.push_str(ret.trim());
        }
        out.push_str(semi);
        out
    }

    /// Enforces WITH CHECK on INSERT (both VALUES and SELECT sources) for a
    /// subject role. Counts how many of the new rows satisfy the policy versus
    /// how many the source produces (binding the same params, so parameterized
    /// inserts are covered) and, if any row fails, rejects the statement before
    /// the real INSERT runs.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_rls_insert(&mut self, sql: &str, params: &[Value]) -> Option<Result<ResultSet>> {
        if first_word_upper(sql) != "INSERT" {
            return None;
        }
        let (target, mut cols, src_start) = parse_insert(sql);
        if target.is_empty() || src_start < 0 {
            return None;
        }
        let src_start = src_start as usize;
        if bare_table_name(&target).is_empty() {
            return None;
        }
        let bare = self.stored_name(&target);
        let expr = self.rls_check_expr(&bare)?;
        // An upsert's DO UPDATE writes the final row, which must satisfy the UPDATE
        // policy: validated post-hoc on the affected rows (not the insert source).
        if has_do_update(sql) {
            return self.rls_check_upsert(sql, params, &bare);
        }
        // The row source is everything after the target/column-list, VALUES (...)
        // or SELECT ..., both valid as a CTE body, minus any trailing ON CONFLICT /
        // RETURNING. The check applies to the source (insert-path) rows.
        let src = trim_end_chars(&sql[src_start..], "; \t\n");
        let mut cut = src.len();
        if let Some(ri) = top_level_keyword(src, "returning")
            && ri < cut
        {
            cut = ri;
        }
        if let Some(ci) = top_level_on_conflict(src)
            && ci < cut
        {
            cut = ci;
        }
        let mut source = trim_end_chars(&src[..cut], "; \t\n").to_string();
        let src_low = source.trim().to_lowercase();
        if src_low.starts_with("default") {
            // INSERT ... DEFAULT VALUES: the one new row is every column's default.
            let (dcols, defs) = self.table_defaults(&bare);
            if dcols.is_empty() {
                if expr == "(0)" {
                    return Some(Err(rls_violation(&bare)));
                }
                return None;
            }
            cols = dcols;
            source = format!("SELECT {}", defs.join(", "));
        } else if src_low.starts_with("overriding") {
            // A form we can't turn into a row-count check: default-deny, else allow.
            if expr == "(0)" {
                return Some(Err(rls_violation(&bare)));
            }
            return None;
        }
        if cols.is_empty() {
            cols = self.table_columns(&bare);
        }
        if cols.is_empty() {
            return None;
        }
        let collist = cols.join(", ");
        // Read-filter the source too, so the count validates exactly the rows the
        // real INSERT (which is read-filtered by apply_rls) will insert.
        let cte = format!("WITH _rls_src ({}) AS ({}) ", collist, self.wrap_rls_select(&source));

        // The VALUES may carry the same $N placeholders as the original INSERT, so
        // bind params to both counts.
        // The calls of the source are inlined, each body read through its own role's
        // policies, as the INSERT will read them.
        let counted = match self.inline_functions(&format!("{cte}SELECT count(*) FROM _rls_src"), Inline::Exec) {
            Ok(q) => q,
            Err(e) => return Some(Err(e)),
        };
        let prov = match self.exec(&rewrite(&counted), params) {
            Ok(rs) if !rs.rows.is_empty() => rs,
            // A source the check cannot count is not let through unchecked.
            Ok(_) => return Some(Err(rls_violation(&bare))),
            Err(e) => return Some(Err(e)),
        };
        let q = self.expand_session_user(&format!("{cte}SELECT count(*) FROM _rls_src WHERE {expr}"));
        let q = match self.inline_functions(&q, Inline::Exec) {
            Ok(q) => q,
            Err(e) => return Some(Err(e)),
        };
        let pass = match self.exec(&rewrite(&q), params) {
            Ok(rs) if !rs.rows.is_empty() => rs,
            Ok(_) => return Some(Err(rls_violation(&bare))),
            Err(e) => return Some(Err(e)),
        };
        if to_int(&pass.rows[0][0]) < to_int(&prov.rows[0][0]) {
            return Some(Err(rls_violation(&bare)));
        }
        // All rows pass; let the normal INSERT path execute the statement as-is.
        None
    }

    /// Runs an ON CONFLICT DO UPDATE upsert inside a savepoint, tacking the UPDATE
    /// policy expression onto RETURNING so every affected row (the final,
    /// post-update version) is validated. If any fails, it rolls back and rejects.
    /// A client RETURNING is preserved and projected back.
    fn rls_check_upsert(&mut self, sql: &str, params: &[Value], bare: &str) -> Option<Result<ResultSet>> {
        let (perm, restr) = self.gather_policies(bare, PolicyCommand::Update, true);
        let upd_expr = combine_rls(&perm, &restr);

        let mut body = trim_end_chars(sql, "; \t\n");
        let mut client_ret = "";
        if let Some(ri) = top_level_keyword(body, "returning") {
            client_ret = body[ri + "returning".len()..].trim();
            body = &body[..ri];
        }
        let mut ret = format!("({upd_expr}) AS _rls_ok");
        if !client_ret.is_empty() {
            ret = format!("{client_ret}, {ret}");
        }
        let exec_sql = match self.rewrite_for_exec(&format!("{body} RETURNING {ret}"), &[]) {
            Ok(q) => q,
            Err(e) => return Some(Err(e)),
        };

        if self.exec("SAVEPOINT _rls_up", &[]).is_err() {
            return None; // can't guard it; let the normal path run
        }
        let rs = match self.exec(&exec_sql, params) {
            Ok(rs) => rs,
            Err(e) => {
                let _ = self.exec("ROLLBACK TO _rls_up", &[]);
                let _ = self.exec("RELEASE _rls_up", &[]);
                return Some(Err(e));
            }
        };
        let ok_col = rs.columns.len() as i64 - 1;
        for row in &rs.rows {
            if ok_col < 0 || !truthy(&row[ok_col as usize]) {
                let _ = self.exec("ROLLBACK TO _rls_up", &[]);
                let _ = self.exec("RELEASE _rls_up", &[]);
                return Some(Err(rls_violation(bare)));
            }
        }
        let _ = self.exec("RELEASE _rls_up", &[]);

        let mut out = ResultSet {
            command: Command::Insert,
            rows_affected: rs.rows.len() as i64,
            ..Default::default()
        };
        if !client_ret.is_empty() {
            // project the client's RETURNING columns back (drop _rls_ok)
            let ok_col = ok_col.max(0) as usize;
            out.is_query = true;
            let mut rs = rs;
            rs.columns.truncate(ok_col);
            out.columns = rs.columns;
            for mut row in rs.rows {
                row.truncate(ok_col);
                out.rows.push(row);
            }
        }
        Some(Ok(out))
    }

    fn table_columns(&mut self, table: &str) -> Vec<String> {
        let q = format!("SELECT name FROM pragma_table_info({})", quote_literal(table));
        let Ok(rs) = self.exec(&q, &[]) else { return Vec::new() };
        let mut cols = Vec::new();
        for row in &rs.rows {
            let n = text_or_empty(&row[0]);
            if !n.is_empty() {
                cols.push(n);
            }
        }
        cols
    }

    /// Each column and the SQL text of its DEFAULT (or "NULL"), so DEFAULT VALUES
    /// can be modeled as a one-row SELECT of those expressions.
    fn table_defaults(&mut self, table: &str) -> (Vec<String>, Vec<String>) {
        let q = format!(
            "SELECT name, dflt_value FROM pragma_table_info({})",
            quote_literal(table)
        );
        let Ok(rs) = self.exec(&q, &[]) else {
            return (Vec::new(), Vec::new());
        };
        let mut cols = Vec::new();
        let mut defs = Vec::new();
        for row in &rs.rows {
            let n = text_or_empty(&row[0]);
            if n.is_empty() {
                continue;
            }
            let mut d = "NULL".to_string();
            match &row[1] {
                Value::Text(v) => {
                    if !v.is_empty() {
                        d = v.clone();
                    }
                }
                Value::Blob(v) if !v.is_empty() => {
                    d = text_from_bytes(v).into_owned();
                }
                v @ (Value::Int(_) | Value::Float(_)) => d = v.to_string(),
                _ => {}
            }
            cols.push(n);
            defs.push(d);
        }
        (cols, defs)
    }
}

fn rls_violation(bare: &str) -> Error {
    errf!(
        SqlState::INSUFFICIENT_PRIVILEGE,
        "new row violates row-level security policy for table {:?}",
        quoted(bare)
    )
}

fn policy_role_match(pol_roles: &str, role_set: &HashSet<String>) -> bool {
    let pol_roles = pol_roles.trim();
    if pol_roles.is_empty() {
        return true; // TO PUBLIC / no TO clause
    }
    for r in pol_roles.split(',') {
        if role_set.contains(&r.trim().to_lowercase()) {
            return true;
        }
    }
    false
}

/// ORs the permissive expressions and ANDs the restrictive ones; no permissive
/// policy means default-deny.
fn combine_rls(perm: &[String], restr: &[String]) -> String {
    let p = if perm.is_empty() {
        "(0)".to_string()
    } else {
        let parts: Vec<String> = perm.iter().map(|e| format!("({e})")).collect();
        format!("({})", parts.join(" OR "))
    };
    if restr.is_empty() {
        return p;
    }
    let parts: Vec<String> = restr.iter().map(|e| format!("({e})")).collect();
    format!("{} AND ({})", p, parts.join(" AND "))
}

/// Whether the statement is an upsert with a DO UPDATE action (as opposed to DO
/// NOTHING).
fn has_do_update(sql: &str) -> bool {
    tree::read(sql, |n| match n {
        NodeEnum::InsertStmt(i) => {
            Some(i.on_conflict_clause.as_ref()?.action == pg_query::protobuf::OnConflictAction::OnconflictUpdate as i32)
        }
        _ => None,
    })
    .unwrap_or(false)
}

/// Extracts the target table token, an explicit column list (if any), and the
/// byte index where the row source (VALUES/SELECT/...) begins (-1 if none).
pub(crate) fn parse_insert(sql: &str) -> (String, Vec<String>, i64) {
    let mut target = String::new();
    let mut cols = Vec::new();
    let mut src_start = -1;
    let toks = rls_tokens(sql);
    for i in 0..toks.len() {
        if toks[i].kind != b'w' || !eq_fold(toks[i].text, "into") || i + 1 >= toks.len() {
            continue;
        }
        target = toks[i + 1].text.to_string();
        let mut j = i + 2;
        // Optional column list in parentheses before the source query.
        if j < toks.len() && toks[j].kind == b'(' {
            let mut depth = 0;
            while j < toks.len() {
                match toks[j].kind {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    b'w' if depth == 1 => {
                        cols.push(toks[j].text.to_string());
                    }
                    _ => {}
                }
                if depth == 0 {
                    j += 1;
                    break;
                }
                j += 1;
            }
        }
        if j < toks.len() {
            src_start = toks[j].start as i64;
        }
        break;
    }
    (target, cols, src_start)
}

// --- small helpers ----------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct SqlTok<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    /// 'w' word/ident, '(' ')' ',' punctuation, 'o' other
    kind: u8,
}

fn rls_tokens(sql: &str) -> Vec<SqlTok<'_>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            i = end_of_string_literal(sql, i);
        } else if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
            i += 1;
        } else if c == b'(' || c == b')' || c == b',' {
            out.push(SqlTok {
                text: &sql[i..=i],
                start: i,
                end: i + 1,
                kind: c,
            });
            i += 1;
        } else if c == b'"' || is_ident_start(c) {
            let mut j = i;
            while j < b.len() {
                let d = b[j];
                if d == b'"' {
                    j += 1;
                    while j < b.len() && b[j] != b'"' {
                        j += 1;
                    }
                    if j < b.len() {
                        j += 1;
                    }
                    continue;
                }
                if is_ident_part(d) || d == b'.' {
                    j += 1;
                    continue;
                }
                break;
            }
            out.push(SqlTok {
                text: &sql[i..j],
                start: i,
                end: j,
                kind: b'w',
            });
            i = j;
        } else {
            out.push(SqlTok {
                text: slice(sql, i, i + 1),
                start: i,
                end: i + 1,
                kind: b'o',
            });
            i += 1;
        }
    }
    out
}

/// Keywords that cannot be a table alias (so a bare alias is injected instead).
static RLS_ALIAS_STOP: LazyLock<HashMap<&'static str, bool>> = LazyLock::new(|| {
    [
        ("on", true),
        ("using", true),
        ("where", true),
        ("group", true),
        ("order", true),
        ("having", true),
        ("limit", true),
        ("join", true),
        ("inner", true),
        ("left", true),
        ("right", true),
        ("full", true),
        ("cross", true),
        ("natural", true),
        ("union", true),
        ("except", true),
        ("intersect", true),
        ("window", true),
        ("returning", true),
        ("set", true),
        ("as", false), // handled explicitly
    ]
    .into_iter()
    .collect()
});

fn alias_follows(toks: &[SqlTok<'_>], i: usize) -> bool {
    if i + 1 >= toks.len() || toks[i + 1].kind != b'w' {
        return false;
    }
    let w = lowercase(toks[i + 1].text);
    if w == "as" {
        return true;
    }
    !RLS_ALIAS_STOP.get(w.as_ref()).copied().unwrap_or(false)
}

pub(crate) fn bare_table_name(text: &str) -> String {
    let t = bare_alias(text);
    let low = lowercase(&t);
    if low.starts_with("pg_")
        || low.starts_with("sqlite_")
        || low.starts_with("_overlite")
        || low == "information_schema"
    {
        return String::new();
    }
    t
}

fn bare_alias(text: &str) -> String {
    let mut text = text;
    if let Some(i) = text.rfind('.') {
        text = &text[i + 1..];
    }
    trim_chars(text, "\"").to_string()
}

/// The byte index of a whole-word keyword at paren depth zero, outside
/// string/quoted-identifier literals.
pub(crate) fn top_level_keyword(s: &str, kw: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            i = end_of_string_literal(s, i);
        } else if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            if i < b.len() {
                i += 1;
            }
        } else if c == b'(' {
            depth += 1;
            i += 1;
        } else if c == b')' {
            if depth > 0 {
                depth -= 1;
            }
            i += 1;
        } else if depth == 0 && is_ident_start(c) {
            let mut j = i;
            while j < b.len() && is_ident_part(b[j]) {
                j += 1;
            }
            if eq_fold(&s[i..j], kw) {
                return Some(i);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// The byte index of a top-level "ON CONFLICT" (an "on" whose next word is
/// "conflict"), distinguishing it from a JOIN's ON.
fn top_level_on_conflict(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            i = end_of_string_literal(s, i);
        } else if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            if i < b.len() {
                i += 1;
            }
        } else if c == b'(' {
            depth += 1;
            i += 1;
        } else if c == b')' {
            if depth > 0 {
                depth -= 1;
            }
            i += 1;
        } else if depth == 0 && is_ident_start(c) {
            let mut j = i;
            while j < b.len() && is_ident_part(b[j]) {
                j += 1;
            }
            if eq_fold(&s[i..j], "on") {
                let mut k = j;
                while k < b.len() && (b[k] == b' ' || b[k] == b'\t' || b[k] == b'\n' || b[k] == b'\r') {
                    k += 1;
                }
                let mut m = k;
                while m < b.len() && is_ident_part(b[m]) {
                    m += 1;
                }
                if eq_fold(&s[k..m], "conflict") {
                    return Some(i);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

pub(crate) fn balanced_paren(s: &str, open: usize) -> (String, usize) {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\'' => i = end_of_string_literal(s, i) - 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return (s[open + 1..i].to_string(), i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    (s[open + 1..].to_string(), s.len())
}

pub(crate) fn text_or_empty(v: &Value) -> String {
    match v {
        Value::Text(x) => x.clone(),
        Value::Blob(x) => text_from_bytes(x).into_owned(),
        _ => String::new(),
    }
}

fn to_int(v: &Value) -> i64 {
    match v {
        Value::Int(x) => *x,
        Value::Float(x) => *x as i64,
        _ => 0,
    }
}
