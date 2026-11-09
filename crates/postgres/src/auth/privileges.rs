//! Privilege enforcement. SQLite has no per-object privileges, so overlite keeps
//! grants in _overlite_grants and table ownership in _overlite_owners, and this
//! layer checks a statement against the connected role before it runs.
//!
//! Enforcement is deliberately narrow to stay safe and backward compatible:
//!   - A superuser, or a role not managed by overlite (never CREATE ROLE'd), is
//!     never checked.
//!   - A table's owner holds every privilege on it implicitly, and alone may
//!     DROP/ALTER it.
//!   - Otherwise the role needs a matching GRANT (to itself or to PUBLIC).
//!
//! What a SELECT, INSERT, UPDATE or DELETE touches is read from its parse tree,
//! column by column (columns.rs), so column privileges hold. Any other statement,
//! and one that reader does not follow, is read from its text (best effort: the
//! tables of a DML statement after INTO, UPDATE, FROM, JOIN and USING) and held to
//! table-level grants alone.
//!
//! What a statement reaches beyond its own text is checked too, as Postgres checks
//! it: the body of each LANGUAGE sql function it calls (functions.rs), a view's query
//! as the view's owner, and the body of each SQLite trigger its writes fire, as the
//! role writing. EXPLAIN is held to the statement it explains, CREATE TABLE AS to its
//! query. The server's own tables, PRAGMA, ATTACH and VACUUM INTO are a superuser's.

use crate::auth::columns::{Access, Cols, Relations, accesses};
use crate::auth::functions::{Inline, MAX_DEPTH};
use crate::ddl::sequences::{is_ident_part, is_ident_start};
use crate::error::{Result, errf};
use crate::session::Session;
use crate::session::utility::{first_word_upper, second_word_upper};
use overlite_core::SqlState;
use overlite_core::Value;
use overlite_core::text::end_of_string_literal;
use overlite_core::text::{eq_fold, quote_literal, split_words, trim_chars};
use overlite_engine::blank_literals;
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::LazyLock;

#[derive(Clone, Debug)]
pub(crate) struct TablePriv<'a> {
    /// As the statement writes it; stored_name says which table that is.
    pub(crate) table: &'a str,
    privilege: Privilege,
}

/// What a statement needs on a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    /// DROP and ALTER: only the owner qualifies, whatever is granted.
    Owner,
}

impl Privilege {
    /// The word `_overlite_grants` holds.
    fn as_str(self) -> &'static str {
        match self {
            Privilege::Select => "SELECT",
            Privilege::Insert => "INSERT",
            Privilege::Update => "UPDATE",
            Privilege::Delete => "DELETE",
            Privilege::Truncate => "TRUNCATE",
            Privilege::Owner => "OWNER",
        }
    }
}

/// Lowers s, returning it untouched when there is nothing to lower.
pub(crate) fn lowercase(s: &str) -> Cow<'_, str> {
    if s.bytes().all(|c| c < 0x80 && !c.is_ascii_uppercase()) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.to_lowercase())
    }
}

impl Session {
    /// Rejects the statement if the session's current role lacks a required
    /// privilege. Ok for bypassing roles and table-less statements.
    ///
    /// The columns a statement reads and writes come from its parse tree
    /// (columns.rs). A statement that reader does not follow is held to the tables
    /// the text names and to table-level grants, so column privileges alone never
    /// let it run. An EXPLAIN is held to what it explains, ANALYZE or not.
    pub(crate) fn check_privileges(&mut self, sql: &str) -> Result<()> {
        let sql = crate::session::explain::explained(sql).unwrap_or(sql);
        if self.priv_bypass() {
            // A SECURITY DEFINER body owned by a role that is checked is checked as
            // that role, whoever calls it.
            if self.calls_functions(sql) {
                self.inline_functions(sql, Inline::Check)?;
            }
            return Ok(());
        }
        self.check_function_ddl(sql)?;
        let expanded = self.inline_functions(sql, Inline::Check)?;
        self.check_statement(&expanded, 0)
    }

    /// Checks sql, its calls inlined, as the current role, with what it reaches: the
    /// queries of the views it reads and the triggers its writes fire. depth counts
    /// the views, triggers and bodies it was reached through.
    pub(crate) fn check_statement(&mut self, sql: &str, depth: usize) -> Result<()> {
        self.check_statement_except(sql, depth, None).map(|_| ())
    }

    /// check_statement, leaving out the table exempt (the table of an exclusion
    /// constraint's trigger, which reads it for the constraint). Returns what was
    /// checked.
    fn check_statement_except(&mut self, sql: &str, depth: usize, exempt: Option<&str>) -> Result<Vec<Access>> {
        if self.priv_bypass() {
            return Ok(Vec::new());
        }
        if depth > MAX_DEPTH {
            return Err(errf!(
                SqlState::STATEMENT_TOO_COMPLEX,
                "views, triggers and functions nest deeper than {MAX_DEPTH}"
            ));
        }
        check_internal(sql)?;
        // A statement Postgres' grammar refuses is refused for that, before any grant
        // is read, as Postgres parses first.
        if depth == 0
            && let Some(e) = crate::ast::grammar_refusal_parsing(sql)
        {
            return Err(e);
        }
        if let Some(query) = create_table_as_query(sql) {
            return self.check_statement_except(query, depth, exempt);
        }
        self.check_trigger_ddl(sql)?;
        let checked = self.statement_accesses(sql, exempt)?;
        self.follow_relations(&checked, depth)?;
        Ok(checked)
    }

    /// What sql needs of each table, checked against the current role's grants.
    fn statement_accesses(&mut self, sql: &str, exempt: Option<&str>) -> Result<Vec<Access>> {
        let keep = |a: &Access| exempt.is_none_or(|t| !eq_fold(&a.table, t));
        // First without the tables' columns: a reference then reads as any column, and
        // a bare name as one of every table it may belong to, which asks for more than
        // the statement needs, never less. Table grants that cover that cover the
        // statement, which is the common case and costs no column lookup. Else the
        // columns decide.
        let mut rels = StoredRelations::new(self, false);
        if let Some(mut rough) = accesses(sql, &mut rels) {
            rough.retain(keep);
            if self.check_accesses(&rough).is_ok() {
                return Ok(rough);
            }
            let mut rels = StoredRelations::new(self, true);
            let mut exact = accesses(sql, &mut rels).unwrap_or(rough);
            exact.retain(keep);
            self.check_accesses(&exact)?;
            return Ok(exact);
        }
        let reqs = required_privileges(sql);
        let mut out: Vec<Access> = Vec::with_capacity(reqs.len());
        for r in &reqs {
            let table = self.stored_name(r.table);
            if exempt.is_some_and(|t| eq_fold(&table, t)) {
                continue;
            }
            if !self.has_privilege(&table, r.privilege) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "permission denied for table {table}"
                ));
            }
            let any = || Some(Cols::default());
            out.push(Access {
                table,
                columns: None,
                select: (r.privilege == Privilege::Select).then(any).flatten(),
                insert: (r.privilege == Privilege::Insert).then(any).flatten(),
                update: (r.privilege == Privilege::Update).then(any).flatten(),
                delete: matches!(r.privilege, Privilege::Delete | Privilege::Truncate),
            });
        }
        Ok(out)
    }

    /// The views read and the triggers fired by what was checked: a view's query is
    /// checked as its owner, a trigger's body as the role writing.
    fn follow_relations(&mut self, checked: &[Access], depth: usize) -> Result<()> {
        for a in checked {
            let writes = a.insert.is_some() || a.update.is_some() || a.delete;
            if a.select.is_none() && !writes {
                continue;
            }
            let reached = self.db.views_and_triggers();
            let key = a.table.to_lowercase();
            if a.select.is_some()
                && let Some(ddl) = reached.views.get(&key)
            {
                self.check_view(&a.table, ddl, depth)?;
            }
            if writes && let Some(triggers) = reached.triggers.get(&key) {
                self.check_triggers(a, triggers, depth)?;
            }
        }
        Ok(())
    }

    /// A view reads its tables with its owner's privileges and through its owner's
    /// policies, as in Postgres. A view no role owns is an administrator's, as a table
    /// with no owner row is.
    fn check_view(&mut self, view: &str, ddl: &str, depth: usize) -> Result<()> {
        let Some(owner) = self.table_owner(view) else {
            return Ok(());
        };
        if self.role_bypasses(&owner) {
            return Ok(());
        }
        let Some(query) = view_query(ddl) else {
            return Err(errf!(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied for view {view}"
            ));
        };
        let query = query.to_string();
        self.as_role(&owner, |s| s.check_owned_query(&query, depth + 1, "view", view))
    }

    /// Checks the query of a view or a materialized view as its owner, the current
    /// role. A table under row-level security whose policies hold the owner is
    /// refused: its policies would have to apply inside the view, which SQLite
    /// expands where they cannot be added.
    pub(crate) fn check_owned_query(&mut self, query: &str, depth: usize, kind: &str, name: &str) -> Result<()> {
        let checked = self.check_statement_except(query, depth, None)?;
        for a in &checked {
            if a.select.is_none() || self.db.views_and_triggers().views.contains_key(&a.table.to_lowercase()) {
                continue;
            }
            if self
                .rls_filter(&a.table, crate::auth::rls::PolicyCommand::Select)
                .is_some()
            {
                return Err(errf!(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "row-level security of table {} cannot be applied through {kind} {name}",
                    a.table
                ));
            }
        }
        Ok(())
    }

    /// The owner of a table or view, from _overlite_owners.
    pub(crate) fn table_owner(&mut self, table: &str) -> Option<String> {
        self.db
            .lookup(
                "SELECT owner FROM _overlite_owners WHERE lower(tablename) = lower(?1)",
                &[table],
            )?
            .into_iter()
            .next()
    }

    /// The SQLite triggers a write fires run their bodies with the writer's
    /// privileges, as a Postgres trigger function (SECURITY INVOKER) does. A trigger
    /// already being checked further up is not checked again: SQLite does not fire a
    /// trigger from itself.
    fn check_triggers(&mut self, a: &Access, triggers: &[(String, String)], depth: usize) -> Result<()> {
        for (name, ddl) in triggers {
            let fires = match trigger_event(ddl) {
                Some(Privilege::Insert) => a.insert.is_some(),
                Some(Privilege::Update) => a.update.is_some(),
                Some(Privilege::Delete) => a.delete,
                _ => true,
            };
            if !fires || self.checking_triggers.iter().any(|t| eq_fold(t, name)) {
                continue;
            }
            let exempt = name
                .to_ascii_lowercase()
                .starts_with("_overlite_excl_")
                .then(|| a.table.clone());
            self.checking_triggers.push(name.clone());
            let r = (|| -> Result<()> {
                for stmt in trigger_statements(ddl) {
                    self.check_statement_except(&stmt, depth + 1, exempt.as_deref())?;
                }
                Ok(())
            })();
            self.checking_triggers.pop();
            r?;
        }
        Ok(())
    }

    /// CREATE TRIGGER needs the table's ownership or the TRIGGER privilege on it, and
    /// DROP TRIGGER its ownership, as in Postgres: a trigger of one role's on another's
    /// table runs whenever the owner writes it.
    fn check_trigger_ddl(&mut self, sql: &str) -> Result<()> {
        let head = sql.trim_start().as_bytes();
        let starts = |w: &[u8]| head.len() >= w.len() && head[..w.len()].eq_ignore_ascii_case(w);
        if !(starts(b"create") || starts(b"drop")) {
            return Ok(());
        }
        let words = split_words(sql);
        let lower: Vec<String> = words.iter().take(4).map(|w| w.to_ascii_lowercase()).collect();
        let creates = lower.first().is_some_and(|w| w == "create")
            && lower[1..].iter().take(2).any(|w| w == "trigger")
            && lower[1..]
                .iter()
                .take_while(|w| *w != "trigger")
                .all(|w| w == "temp" || w == "temporary");
        let drops = lower.first().is_some_and(|w| w == "drop") && lower.get(1).is_some_and(|w| w == "trigger");
        if creates {
            let Some(table) = trigger_table(sql) else {
                return Ok(());
            };
            let table = self.stored_name(table);
            let mut roles = self.effective_roles();
            if self.owns_table(&table, &roles) {
                return Ok(());
            }
            roles.push("public".to_string());
            let q = format!(
                "SELECT 1 FROM _overlite_grants WHERE lower(tablename) = lower({}) AND upper(privilege) IN ('TRIGGER', 'ALL') AND lower(grantee) IN ({}) LIMIT 1",
                quote_literal(&table),
                in_list(&roles)
            );
            if matches!(self.exec(&q, &[]), Ok(rs) if !rs.rows.is_empty()) {
                return Ok(());
            }
            return Err(errf!(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied for table {table}"
            ));
        }
        if drops {
            let mut i = 2;
            if lower.get(2).is_some_and(|w| w == "if") {
                i = 4;
            }
            let Some(name) = words.get(i) else { return Ok(()) };
            let name = trim_chars(name.trim_end_matches(';'), "\"");
            let name = name.rsplit('.').next().unwrap_or(name);
            let Some((kind, ddl)) = self.db.schema_object(name) else {
                return Ok(());
            };
            if kind != "trigger" {
                return Ok(());
            }
            let Some(table) = trigger_table(&ddl) else {
                return Ok(());
            };
            let table = self.stored_name(table);
            if !self.has_privilege(&table, Privilege::Owner) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must be owner of relation {table}"
                ));
            }
        }
        Ok(())
    }

    /// Holds each table to what the statement needs of it, as Postgres' executor does
    /// (ExecCheckOneRelPerms): the owner may do anything; else a privilege granted on
    /// the table covers all its columns, and without one each column needed must have
    /// it granted, or, when none is named, any column. DELETE has no column form.
    fn check_accesses(&mut self, accesses: &[Access]) -> Result<()> {
        if accesses.is_empty() {
            return Ok(());
        }
        let mut roles = self.effective_roles();
        for a in accesses {
            if self.owns_table(&a.table, &roles) {
                continue;
            }
            roles.push("public".to_string());
            let granted = [
                (Privilege::Select, &a.select),
                (Privilege::Insert, &a.insert),
                (Privilege::Update, &a.update),
            ]
            .into_iter()
            .all(|(p, cols)| cols.as_ref().is_none_or(|c| self.columns_granted(a, p, c, &roles)))
                && (!a.delete || self.table_granted(&a.table, Privilege::Delete, &roles));
            roles.pop();
            if !granted {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "permission denied for table {}",
                    a.table
                ));
            }
        }
        Ok(())
    }

    /// Whether grantees hold privilege on the columns of a that cols asks for.
    fn columns_granted(&mut self, a: &Access, privilege: Privilege, cols: &Cols, grantees: &[String]) -> bool {
        if self.table_granted(&a.table, privilege, grantees) {
            return true;
        }
        // Columns that cannot be read cannot be matched to column grants.
        let Some(all) = &a.columns else { return false };
        let held = self.granted_columns(&a.table, privilege, grantees);
        if cols.all {
            all.iter().all(|c| held.contains(&c.to_lowercase()))
        } else if cols.names.is_empty() {
            !held.is_empty()
        } else {
            cols.names.iter().all(|c| held.contains(c))
        }
    }

    /// Whether grantees hold privilege on the table itself.
    fn table_granted(&mut self, table: &str, privilege: Privilege, grantees: &[String]) -> bool {
        let q = format!(
            "SELECT 1 FROM _overlite_grants WHERE lower(tablename) = lower(?1) AND upper(privilege) IN (upper(?2), 'ALL') AND lower(grantee) IN ({}) LIMIT 1",
            in_list(grantees)
        );
        self.db
            .lookup(&q, &[table, privilege.as_str()])
            .is_some_and(|rows| !rows.is_empty())
    }

    /// The columns of table on which grantees hold privilege, lower-cased.
    fn granted_columns(&mut self, table: &str, privilege: Privilege, grantees: &[String]) -> HashSet<String> {
        let q = format!(
            "SELECT DISTINCT lower(columnname) FROM _overlite_column_grants WHERE tablename = {} AND privilege = {} AND lower(grantee) IN ({})",
            quote_literal(table),
            quote_literal(privilege.as_str()),
            in_list(grantees)
        );
        let Ok(rs) = self.exec(&q, &[]) else {
            return HashSet::new();
        };
        rs.rows
            .iter()
            .filter_map(|r| match &r[0] {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// The owner check of an ALTER TABLE, which has to come before any of its routes:
    /// most of them never reach the check on the way to the engine. A table that is
    /// not there is left to the statement, which says so or, IF EXISTS, does nothing.
    pub(crate) fn check_alter_table(&mut self, sql: &str) -> Result<()> {
        if !eq_fold(&first_word_upper(sql), "ALTER") || second_word_upper(sql) != "TABLE" {
            return Ok(());
        }
        let Err(denied) = self.check_privileges(sql) else {
            return Ok(());
        };
        for r in required_privileges(sql) {
            let table = self.stored_name(r.table);
            if self.table_ddl(&table).is_empty() {
                return Ok(());
            }
        }
        Err(denied)
    }

    /// Whether the current role skips privilege checks: unmanaged roles and
    /// superusers. Cached per role; cleared on role DDL (see clear_priv_cache).
    pub(crate) fn priv_bypass(&mut self) -> bool {
        if let Some(b) = self.bypass_cache.get(&self.current_role) {
            return *b;
        }
        let b = self.compute_bypass();
        self.bypass_cache.insert(self.current_role.clone(), b);
        b
    }

    fn compute_bypass(&mut self) -> bool {
        let role = self.current_role.clone();
        self.role_bypasses(&role)
    }

    /// Whether a role skips privilege checks: an unmanaged role (never CREATE
    /// ROLE'd -> trusted/legacy) or a superuser.
    pub(crate) fn role_bypasses(&mut self, role: &str) -> bool {
        let q = format!(
            "SELECT rolsuper FROM _overlite_roles WHERE lower(rolname) = lower({})",
            quote_literal(role)
        );
        match self.exec(&q, &[]) {
            Ok(rs) if !rs.rows.is_empty() => truthy(&rs.rows[0][0]),
            _ => true, // unmanaged role -> trusted/legacy
        }
    }

    pub(crate) fn clear_priv_cache(&mut self) {
        self.bypass_cache.clear();
    }

    /// Whether the role has the CREATEROLE attribute.
    pub(crate) fn has_create_role(&mut self, role: &str) -> bool {
        let q = format!(
            "SELECT 1 FROM _overlite_roles WHERE lower(rolname) = lower({}) AND rolcreaterole <> 0 LIMIT 1",
            quote_literal(role)
        );
        matches!(self.exec(&q, &[]), Ok(rs) if !rs.rows.is_empty())
    }

    /// Whether the current role (directly or via an inherited role) holds ADMIN
    /// OPTION on target, i.e. may grant/revoke membership in it.
    pub(crate) fn has_admin_option(&mut self, target: &str) -> bool {
        let roles = self.effective_roles();
        let q = format!(
            "SELECT 1 FROM _overlite_memberships WHERE lower(roleof) = lower({}) AND admin_option <> 0 AND lower(member) IN ({}) LIMIT 1",
            quote_literal(target),
            in_list(&roles)
        );
        matches!(self.exec(&q, &[]), Ok(rs) if !rs.rows.is_empty())
    }

    /// Checks ownership then (for grantable privileges) the grant table, against
    /// the role's effective set (itself plus inherited roles). Rows hold the stored
    /// name, so a table is only ever matched by its own: one with no owner row
    /// is nobody's, and only a role that bypasses these checks can touch it. A row
    /// from before names were stored holds the bare name, which reads as the table
    /// of that name in public, and so stops counting for a table anywhere else.
    pub(crate) fn has_privilege(&mut self, table: &str, privilege: Privilege) -> bool {
        let mut roles = self.effective_roles();
        if self.owns_table(table, &roles) {
            return true;
        }
        if privilege == Privilege::Owner {
            return false; // only the owner (or a role that inherits it) qualifies
        }
        roles.push("public".to_string());
        self.table_granted(table, privilege, &roles)
    }

    pub(crate) fn owns_table(&mut self, table: &str, roles: &[String]) -> bool {
        let q = format!(
            "SELECT 1 FROM _overlite_owners WHERE lower(tablename) = lower(?1) AND lower(owner) IN ({}) LIMIT 1",
            in_list(roles)
        );
        self.db.lookup(&q, &[table]).is_some_and(|rows| !rows.is_empty())
    }

    /// The current role plus every role it inherits from, transitively,
    /// following a membership edge only when the member has INHERIT. All names
    /// are lower-cased.
    pub(crate) fn effective_roles(&mut self) -> Vec<String> {
        let cur = self.current_role.to_lowercase();
        let mut roles = vec![cur.clone()];
        let q = r#"WITH RECURSIVE eff(role) AS (
  SELECT lower(?1)
  UNION
  SELECT lower(m.roleof) FROM _overlite_memberships m
    JOIN eff e ON lower(m.member) = e.role
    JOIN _overlite_roles r ON lower(r.rolname) = e.role AND r.rolinherit <> 0
) SELECT role FROM eff"#;
        let Some(rows) = self.db.lookup(q, &[self.current_role.as_str()]) else {
            return roles;
        };
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(cur);
        for s in rows {
            if !s.is_empty() && !seen.contains(&s) {
                seen.insert(s.clone());
                roles.push(s);
            }
        }
        roles
    }
}

/// The tables of a statement as this session finds them: by the search_path, under
/// their stored names.
pub(crate) struct StoredRelations<'a> {
    session: &'a mut Session,
    /// Whether to read the tables' columns, or to leave them unknown.
    columns: bool,
}

impl<'a> StoredRelations<'a> {
    pub(crate) fn new(session: &'a mut Session, columns: bool) -> Self {
        StoredRelations { session, columns }
    }
}

impl Relations for StoredRelations<'_> {
    fn relation(&mut self, schema: &str, name: &str) -> (String, Option<Vec<String>>) {
        let quote = |s: &str| ["\"", &s.replace('"', "\"\""), "\""].concat();
        let reference = if schema.is_empty() {
            quote(name)
        } else {
            [quote(schema), ".".to_string(), quote(name)].concat()
        };
        let table = self.session.stored_name(&reference);
        if !self.columns {
            return (table, None);
        }
        let q = format!(
            "SELECT name FROM pragma_table_xinfo({}) WHERE hidden <> 1 ORDER BY cid",
            quote_literal(&table)
        );
        let columns = match self.session.exec(&q, &[]) {
            Ok(rs) if !rs.rows.is_empty() => Some(
                rs.rows
                    .iter()
                    .map(|r| match &r[0] {
                        Value::Text(s) => s.clone(),
                        _ => String::new(),
                    })
                    .collect(),
            ),
            _ => None,
        };
        (table, columns)
    }
}

/// Renders role names as a lower-cased SQL IN-list body.
fn in_list(roles: &[String]) -> String {
    let parts: Vec<String> = roles.iter().map(|r| quote_literal(&r.to_lowercase())).collect();
    parts.join(", ")
}

/// The (table, privilege) pairs a statement needs. A write also reads what it names
/// after FROM, JOIN and USING, besides its target.
fn required_privileges(sql: &str) -> Vec<TablePriv<'_>> {
    let reads = |skip: usize| {
        let mut v = tables_after(sql, &["from", "join", "using"], Privilege::Select);
        v.drain(..skip.min(v.len()));
        v
    };
    match first_word_upper(sql).as_str() {
        "SELECT" | "WITH" | "TABLE" | "VALUES" => return tables_after(sql, &["from", "join"], Privilege::Select),
        "INSERT" => {
            let mut v = tables_after(sql, &["into"], Privilege::Insert);
            v.extend(reads(0));
            return v;
        }
        "REPLACE" => {
            let mut v = tables_after(sql, &["into"], Privilege::Insert);
            v.extend(tables_after(sql, &["into"], Privilege::Delete));
            v.extend(reads(0));
            return v;
        }
        "UPDATE" => {
            let mut v = tables_after(sql, &["update"], Privilege::Update);
            v.extend(reads(0));
            return v;
        }
        "DELETE" => {
            let mut v = tables_after(sql, &["from"], Privilege::Delete);
            v.truncate(1);
            v.extend(reads(1));
            return v;
        }
        "TRUNCATE" => return truncate_tables(sql),
        "DROP" | "ALTER" if eq_fold(&second_word(sql), "table") => return owned_tables(sql),
        "DROP" if eq_fold(&second_word(sql), "view") => return owned_tables(sql),
        _ => {}
    }
    Vec::new()
}

/// What only a superuser may reach: the server's own tables (`_overlite_*`, which
/// hold the roles, the grants and the policies), writing SQLite's (`sqlite_*`),
/// sqlite_dbpage (the file's raw pages), and PRAGMA, ATTACH, DETACH and VACUUM INTO,
/// which change the connection or reach files.
fn check_internal(sql: &str) -> Result<()> {
    let head = sql.trim_start().as_bytes();
    let starts = |w: &str| head.len() >= w.len() && head[..w.len()].eq_ignore_ascii_case(w.as_bytes());
    let suspect = ["pragma", "attach", "detach", "vacuum"].iter().any(|w| starts(w))
        || contains_fold(sql, "_overlite")
        || contains_fold(sql, "sqlite_");
    if !suspect {
        return Ok(());
    }
    let first = first_word_upper(sql);
    if matches!(first.as_str(), "PRAGMA" | "ATTACH" | "DETACH") {
        return Err(errf!(
            SqlState::INSUFFICIENT_PRIVILEGE,
            "must be superuser to run {first}"
        ));
    }
    let code = blank_literals(sql);
    let words = scan_words(&code);
    if first == "VACUUM" && words.iter().any(|w| !w.quoted && eq_fold(w.text, "into")) {
        return Err(errf!(
            SqlState::INSUFFICIENT_PRIVILEGE,
            "must be superuser to run VACUUM INTO"
        ));
    }
    for w in &words {
        for part in w.text.split('.') {
            let part = trim_chars(part, "\"");
            // sqlite_dbpage reads and writes the file's pages, whatever they hold.
            if (part.len() >= 9 && part[..9].eq_ignore_ascii_case("_overlite"))
                || part.eq_ignore_ascii_case("sqlite_dbpage")
            {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "permission denied for table {part}"
                ));
            }
        }
    }
    let target = match first.as_str() {
        "INSERT" | "REPLACE" => after_word(&words, "into"),
        "UPDATE" => words.get(1).map(|w| w.text),
        "DELETE" => after_word(&words, "from"),
        _ => None,
    };
    if let Some(t) = target {
        let name = trim_chars(t.rsplit('.').next().unwrap_or(t), "\"");
        if name.len() >= 7 && name[..7].eq_ignore_ascii_case("sqlite_") {
            return Err(errf!(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied for table {name}"
            ));
        }
    }
    Ok(())
}

/// Whether hay holds needle (ASCII, lower case), case ignored.
fn contains_fold(hay: &str, needle: &str) -> bool {
    hay.as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// The word after the first unquoted occurrence of keyword.
fn after_word<'a>(words: &[Word<'a>], keyword: &str) -> Option<&'a str> {
    let i = words.iter().position(|w| !w.quoted && eq_fold(w.text, keyword))?;
    let mut j = i + 1;
    while words
        .get(j)
        .is_some_and(|w| !w.quoted && (eq_fold(w.text, "only") || eq_fold(w.text, "or")))
    {
        j += 1;
    }
    words.get(j).map(|w| w.text)
}

/// The query of a CREATE [TEMP | UNLOGGED] TABLE ... AS query [WITH [NO] DATA], or of
/// a CREATE MATERIALIZED VIEW: what the statement reads.
pub(crate) fn create_table_as_query(sql: &str) -> Option<&str> {
    let head = sql.trim_start().as_bytes();
    if head.len() < 6 || !head[..6].eq_ignore_ascii_case(b"create") {
        return None;
    }
    let words = scan_words(sql);
    let lead: Vec<String> = words.iter().take(3).map(|w| w.text.to_ascii_lowercase()).collect();
    if lead.first().is_none_or(|w| w != "create") {
        return None;
    }
    let kind_at = lead[1..]
        .iter()
        .position(|w| !matches!(w.as_str(), "temp" | "temporary" | "unlogged" | "global" | "local"))?
        + 1;
    if !matches!(lead.get(kind_at).map(String::as_str), Some("table" | "materialized")) {
        return None;
    }
    let at = top_level_as_query(sql)?;
    let mut query = sql[at..].trim_end().trim_end_matches(';').trim_end();
    let tail = query.to_ascii_lowercase();
    for suffix in [" with no data", " with data"] {
        if tail.ends_with(suffix) {
            query = &query[..query.len() - suffix.len()];
            break;
        }
    }
    Some(query)
}

/// Where the query of a CREATE ... AS begins: past the first AS at depth zero that a
/// query follows.
fn top_level_as_query(sql: &str) -> Option<usize> {
    let b = sql.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i = end_of_string_literal(sql, i);
                continue;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            c if depth == 0
                && (c == b'a' || c == b'A')
                && sql.get(i..i + 2).is_some_and(|w| w.eq_ignore_ascii_case("as"))
                && (i == 0 || !is_ident_part(b[i - 1]))
                && b.get(i + 2).is_none_or(|c| !is_ident_part(*c)) =>
            {
                let rest = sql[i + 2..].trim_start();
                let word: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
                if rest.starts_with('(')
                    || ["select", "with", "values", "table"]
                        .iter()
                        .any(|w| word.eq_ignore_ascii_case(w))
                {
                    return Some(sql.len() - rest.len());
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The query of a CREATE VIEW as sqlite_master keeps it.
pub(crate) fn view_query(ddl: &str) -> Option<&str> {
    let at = top_level_as_query(ddl)?;
    Some(ddl[at..].trim_end().trim_end_matches(';'))
}

/// The table a CREATE TRIGGER is on: the name after its first ON.
fn trigger_table(ddl: &str) -> Option<&str> {
    let words = scan_words(ddl);
    let i = words.iter().position(|w| !w.quoted && eq_fold(w.text, "on"))?;
    words.get(i + 1).map(|w| w.text)
}

/// What fires a SQLite trigger: INSERT, UPDATE [OF ...] or DELETE, before its ON.
fn trigger_event(ddl: &str) -> Option<Privilege> {
    for w in scan_words(ddl) {
        if w.quoted {
            continue;
        }
        match w.text.to_ascii_lowercase().as_str() {
            "insert" => return Some(Privilege::Insert),
            "update" => return Some(Privilege::Update),
            "delete" => return Some(Privilege::Delete),
            "on" => return None,
            _ => {}
        }
    }
    None
}

/// The statements of a SQLite trigger's body, between its BEGIN and END, with each
/// NEW.col and OLD.col read as NULL: a value of the row, which no privilege governs,
/// and which Postgres' grammar would take for a table.
fn trigger_statements(ddl: &str) -> Vec<String> {
    let code = blank_literals(ddl);
    let lower = code.to_ascii_lowercase();
    let Some(begin) = find_word(&lower, "begin", 0) else {
        return Vec::new();
    };
    let end = lower.rfind("end").unwrap_or(ddl.len());
    let body = &ddl[begin + 5..end.max(begin + 5)];
    let mut out = Vec::new();
    let b = body.as_bytes();
    let (mut start, mut i, mut depth) = (0, 0, 0i32);
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i = end_of_string_literal(body, i);
                continue;
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            b';' if depth == 0 => {
                out.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&body[start..]);
    out.into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(row_values_as_null)
        .collect()
}

/// stmt with every NEW.x and OLD.x (outside literals) as NULL.
fn row_values_as_null(stmt: &str) -> String {
    let b = stmt.as_bytes();
    let mut out = String::with_capacity(stmt.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            let j = end_of_string_literal(stmt, i);
            out.push_str(&stmt[i..j]);
            i = j;
            continue;
        }
        if is_ident_start(b[i]) && (i == 0 || !is_ident_part(b[i - 1])) {
            let mut j = i;
            while j < b.len() && is_ident_part(b[j]) {
                j += 1;
            }
            let word = &stmt[i..j];
            if (eq_fold(word, "new") || eq_fold(word, "old")) && b.get(j) == Some(&b'.') {
                let mut k = j + 1;
                if b.get(k) == Some(&b'"') {
                    k += 1;
                    while k < b.len() && b[k] != b'"' {
                        k += 1;
                    }
                    k += 1;
                } else {
                    while k < b.len() && is_ident_part(b[k]) {
                        k += 1;
                    }
                }
                out.push_str("NULL");
                i = k.min(b.len());
                continue;
            }
            out.push_str(word);
            i = j;
            continue;
        }
        let c = stmt[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// The offset of a whole word in lower, from from.
fn find_word(lower: &str, word: &str, from: usize) -> Option<usize> {
    let b = lower.as_bytes();
    let mut at = from;
    while let Some(k) = lower[at..].find(word) {
        let i = at + k;
        let before = i == 0 || !is_ident_part(b[i - 1]);
        let after = b.get(i + word.len()).is_none_or(|c| !is_ident_part(*c));
        if before && after {
            return Some(i);
        }
        at = i + word.len();
    }
    None
}

/// The tables an ALTER TABLE or DROP TABLE acts on: ALTER names one, DROP a list.
/// IF EXISTS and ONLY come before the name, and must not be taken for it.
pub(crate) fn owned_tables(sql: &str) -> Vec<TablePriv<'_>> {
    let toks = scan_words(sql);
    let mut i = 2;
    if toks.len() > i + 1 && eq_fold(toks[i].text, "if") && eq_fold(toks[i + 1].text, "exists") {
        i += 2;
    }
    if i < toks.len() && eq_fold(toks[i].text, "only") {
        i += 1;
    }
    let names = toks.iter().skip(i);
    let alter = eq_fold(toks[0].text, "alter");
    names
        .take(if alter { 1 } else { usize::MAX })
        .take_while(|t| t.quoted || !(eq_fold(t.text, "cascade") || eq_fold(t.text, "restrict")))
        .filter_map(table_name)
        .map(|table| TablePriv {
            table,
            privilege: Privilege::Owner,
        })
        .collect()
}

/// Collects the table reference following each whole-word occurrence of a
/// keyword (outside string literals), tagged with priv. System catalog and
/// internal tables are skipped.
pub(crate) fn tables_after<'a>(sql: &'a str, keywords: &[&str], privilege: Privilege) -> Vec<TablePriv<'a>> {
    let mut kw: HashSet<&str> = HashSet::new();
    for k in keywords {
        kw.insert(*k);
    }
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let toks = scan_words(sql);
    let mut i = 0;
    while i + 1 < toks.len() {
        if !kw.contains(lowercase(toks[i].text).as_ref()) || toks[i].quoted {
            i += 1;
            continue;
        }
        let Some(name) = table_name(&toks[i + 1]) else {
            i += 1;
            continue;
        };
        let lname = name.to_lowercase();
        if seen.contains(&lname) {
            i += 1;
            continue;
        }
        seen.insert(lname);
        out.push(TablePriv { table: name, privilege });
        i += 1;
    }
    out
}

/// Parses "TRUNCATE [TABLE] t1, t2, ..." into TRUNCATE privileges.
fn truncate_tables(sql: &str) -> Vec<TablePriv<'_>> {
    let toks = scan_words(sql);
    let mut out = Vec::new();
    for t in toks.iter().skip(1) {
        // skip the leading TRUNCATE
        if eq_fold(t.text, "table") || eq_fold(t.text, "only") {
            continue;
        }
        // What follows the tables: RESTART or CONTINUE IDENTITY, CASCADE, RESTRICT.
        if ["restart", "continue", "cascade", "restrict"]
            .iter()
            .any(|w| eq_fold(t.text, w))
        {
            break;
        }
        if let Some(name) = table_name(t) {
            out.push(TablePriv {
                table: name,
                privilege: Privilege::Truncate,
            });
        }
    }
    out
}

/// A table-reference token as written, when it is an enforceable table: None for
/// subqueries, keywords, and system/internal tables.
pub(crate) fn table_name<'a>(t: &Word<'a>) -> Option<&'a str> {
    if t.text.is_empty() || t.text.as_bytes()[0] == b'(' {
        return None;
    }
    let mut name = t.text;
    let mut schema = "";
    if let Some(dot) = name.find('.') {
        schema = &name[..dot];
        name = &name[dot + 1..];
    }
    let name = trim_chars(name, "\"");
    let schema = trim_chars(schema, "\"");
    if name.is_empty() {
        return None;
    }
    let low = lowercase(name);
    if !t.quoted && SQL_RESERVED.contains(low.as_ref()) {
        return None;
    }
    if eq_fold(schema, "pg_catalog") || eq_fold(schema, "information_schema") {
        return None;
    }
    if low.starts_with("pg_")
        || low.starts_with("sqlite_")
        || low.starts_with("_overlite")
        || low == "information_schema"
    {
        return None;
    }
    Some(t.text)
}

/// The small set of words that can legally follow FROM/JOIN/etc. without being a
/// table.
static SQL_RESERVED: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "select",
        "where",
        "on",
        "using",
        "set",
        "values",
        "lateral",
        "only",
        "as",
        "group",
        "order",
        "limit",
        "returning",
        "natural",
        "left",
        "right",
        "inner",
        "outer",
        "full",
        "cross",
        "join",
    ]
    .into_iter()
    .collect()
});

/// An identifier/punctuation token from scan_words.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Word<'a> {
    pub(crate) text: &'a str,
    /// Came from a "double-quoted" identifier.
    pub(crate) quoted: bool,
}

/// Splits a statement into identifier tokens (including a leading
/// schema-qualifier and double-quoted parts), lone "(" markers, and skips string
/// literals. Purpose-built for table extraction, not a general lexer.
pub(crate) fn scan_words(sql: &str) -> Vec<Word<'_>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            i = end_of_string_literal(sql, i);
        } else if c == b'(' {
            out.push(Word {
                text: "(",
                quoted: false,
            });
            i += 1;
        } else if c == b'"' || is_ident_start(c) || c == b'.' {
            let mut j = i;
            let mut quoted = false;
            while j < b.len() {
                let d = b[j];
                if d == b'"' {
                    quoted = true;
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
            out.push(Word {
                text: &sql[i..j],
                quoted,
            });
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

fn second_word(sql: &str) -> String {
    let f = split_words(sql);
    if f.len() < 2 {
        return String::new();
    }
    f[1].to_string()
}

pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Int(x) => *x != 0,
        Value::Bool(x) => *x,
        Value::Float(x) => *x != 0.0,
        Value::Text(x) => !x.is_empty() && x != "0" && !eq_fold(x, "false"),
        _ => false,
    }
}
