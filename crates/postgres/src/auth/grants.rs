//! GRANT/REVOKE handling and table-ownership tracking. These write the internal
//! _overlite_grants / _overlite_owners tables that privileges.rs reads.

use crate::auth::privileges::{Privilege, Word, owned_tables, table_name};
use crate::ddl::alter::split_qualified_ref;
use crate::ddl::sequences::is_ident_part;
use crate::error::{Result, errf};
use crate::session::Session;
use crate::session::utility::{first_word_upper, second_word_upper};
use overlite_core::text::{eq_fold, quote_ident, quote_literal, quoted, split_words, trim_chars, trim_end_chars};
use overlite_core::{Command, SqlState, Value};
use overlite_engine::{code_only, forget_table_rows_sql, split_stored_name, stored_table_name};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// Whether sql is a GRANT or REVOKE statement, or an ALTER FUNCTION (ROUTINE,
/// PROCEDURE), which changes who owns a function and as whom its body runs.
pub(crate) fn is_grant(sql: &str) -> bool {
    let w = first_word_upper(sql);
    w == "GRANT"
        || w == "REVOKE"
        || (w == "ALTER" && matches!(second_word_upper(sql).as_str(), "FUNCTION" | "ROUTINE" | "PROCEDURE"))
}

impl Session {
    /// Parses a table GRANT/REVOKE and updates _overlite_grants. Role (GRANT role
    /// TO role) and non-table object grants are accepted as no-ops. Returns the
    /// command tag ("GRANT"/"REVOKE").
    pub(crate) fn apply_grant(&mut self, sql: &str) -> Result<Command> {
        if let Some(r) = self.try_function_acl(sql) {
            return r;
        }
        if eq_fold(&first_word_upper(sql), "ALTER") {
            return Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER FUNCTION supports OWNER TO and SECURITY DEFINER | INVOKER only"
            ));
        }
        let revoke = eq_fold(&first_word_upper(sql), "REVOKE");
        let tag = Command::from(if revoke { "REVOKE" } else { "GRANT" });

        // No ON clause => role membership: GRANT <role> TO <member>.
        if index_word(&code_only(sql).to_ascii_lowercase(), "on").is_none() {
            self.apply_role_grant(sql, revoke)?;
            return Ok(tag);
        }

        let Some((privs, tables, grantees)) = parse_grant(sql, revoke) else {
            return Ok(tag); // shape we don't model (roles, schemas, ...): no-op
        };
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
    fn apply_role_grant(&mut self, sql: &str, revoke: bool) -> Result<()> {
        let Some(rg) = parse_role_grant(sql, revoke) else {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in role grant"));
        };
        for role in &rg.roles {
            if !self.can_grant_membership(role) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "must have admin option on role {:?}",
                    quoted(role)
                ));
            }
            for mem in &rg.members {
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
        let f = split_words(sql);
        // A view is its creator's, as a table is: it reads with its owner's privileges.
        if f.len() >= 3 && eq_fold(f[0], "create") {
            let or_replace = eq_fold(f[1], "or") && f.len() > 4;
            let kind = if or_replace { 3 } else { 1 };
            let kind = kind
                + f[kind..]
                    .iter()
                    .take_while(|w| eq_fold(w, "temp") || eq_fold(w, "temporary"))
                    .count();
            if f.get(kind).is_some_and(|w| eq_fold(w, "view")) {
                let mut at = kind + 1;
                if f.get(at).is_some_and(|w| eq_fold(w, "if")) {
                    at += 3;
                }
                let name = f.get(at).map(|n| n.split('(').next().unwrap_or(n)).unwrap_or_default();
                if !name.is_empty() {
                    let conflict = if or_replace { "IGNORE" } else { "REPLACE" };
                    let q = format!(
                        "INSERT OR {conflict} INTO _overlite_owners (tablename, owner) VALUES ({}, {})",
                        quote_literal(&self.stored_name(name)),
                        quote_literal(&self.current_role)
                    );
                    let _ = self.exec(&q, &[]);
                }
                return;
            }
        }
        if f.len() >= 3 && eq_fold(f[0], "drop") && eq_fold(f[1], "view") {
            let mut at = 2;
            if f.get(2).is_some_and(|w| eq_fold(w, "if")) {
                at = 4;
            }
            for name in f[at.min(f.len())..].join(" ").split(',') {
                let name = name.split_whitespace().next().unwrap_or_default().trim_end_matches(';');
                if name.is_empty() || eq_fold(name, "cascade") || eq_fold(name, "restrict") {
                    continue;
                }
                let stored = self.stored_name(name);
                self.forget_table(&stored);
            }
            return;
        }
        if f.len() < 3 || !eq_fold(f[1], "table") {
            return;
        }
        if eq_fold(f[0], "create") {
            let name = created_table_name(&f[2..]);
            if !name.is_empty() {
                // IF NOT EXISTS may have created nothing, and then takes nothing.
                let conflict = if eq_fold(f[2], "if") { "IGNORE" } else { "REPLACE" };
                let q = format!(
                    "INSERT OR {conflict} INTO _overlite_owners (tablename, owner) VALUES ({}, {})",
                    quote_literal(&self.stored_name(&name)),
                    quote_literal(&self.current_role)
                );
                let _ = self.exec(&q, &[]);
            }
        } else if eq_fold(f[0], "drop") {
            for r in owned_tables(sql) {
                for name in self.names_of(r.table) {
                    self.forget_table(&name);
                }
            }
            self.rls_changed();
        } else if eq_fold(f[0], "alter") {
            self.follow_columns(sql);
        }
    }

    /// Keeps column grants on their columns after an ALTER TABLE run by the engine:
    /// a renamed column takes them along, and a dropped one takes them away, so that
    /// a column added later by that name starts with none.
    fn follow_columns(&mut self, sql: &str) {
        let Ok(parsed) = crate::ast::parse::parse(sql) else {
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

/// The parsed form of a role-membership GRANT/REVOKE.
#[derive(Clone, Debug, Default)]
struct RoleGrant {
    roles: Vec<String>,
    members: Vec<String>,
    /// GRANT ... WITH ADMIN OPTION
    admin: bool,
    /// REVOKE ADMIN OPTION FOR ...
    admin_only: bool,
}

/// Splits "GRANT r1, r2 TO m1, m2 [WITH ADMIN OPTION]" or "REVOKE [ADMIN OPTION
/// FOR] r1 FROM m1" into its parts.
fn parse_role_grant(sql: &str, revoke: bool) -> Option<RoleGrant> {
    let sep = if revoke { "from" } else { "to" };
    // Keywords are read in the code, as a quoted name can spell one.
    let low = code_only(sql).to_ascii_lowercase();
    let sep_idx = index_word(&low, sep)?;
    let roles_at = first_word_end(sql);
    let mut roles_spec = &sql[roles_at..sep_idx];
    let mem_at = sep_idx + sep.len();
    let mem_low = &low[mem_at..];
    let mut mem_end = mem_low.len();

    let mut rg = RoleGrant::default();
    // GRANT ... WITH ADMIN OPTION
    if let Some(k) = mem_low.find("with admin") {
        rg.admin = true;
        mem_end = k;
    }
    // REVOKE ADMIN OPTION FOR <role> FROM ...
    if let Some(rest) = low[roles_at..sep_idx].trim_start().strip_prefix("admin option for") {
        rg.admin_only = true;
        roles_spec = &roles_spec[roles_spec.len() - rest.len()..];
    }
    for tail in [" granted by", " cascade", " restrict"] {
        if let Some(k) = mem_low[..mem_end].find(tail) {
            mem_end = k;
        }
    }
    let mem_spec = trim_end_chars(&sql[mem_at..mem_at + mem_end], "; \t\n");
    rg.roles = split_grantees(roles_spec);
    rg.members = split_grantees(mem_spec);
    if rg.roles.is_empty() || rg.members.is_empty() {
        return None;
    }
    Some(rg)
}

/// A column a GRANT names: folded to lower case unless quoted.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GrantColumn {
    name: String,
    quoted: bool,
}

/// One privilege of a GRANT: SELECT, ALL, ..., with the columns it is limited to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PrivSpec {
    name: String,
    columns: Option<Vec<GrantColumn>>,
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

/// Extracts the privileges, tables, and grantees from a table GRANT/REVOKE. None
/// for forms it doesn't model (e.g. GRANT <role> TO, GRANT ... ON
/// SCHEMA/SEQUENCE/DATABASE, GRANT ... ON ALL TABLES IN SCHEMA).
fn parse_grant(sql: &str, revoke: bool) -> Option<(Vec<PrivSpec>, Vec<String>, Vec<String>)> {
    let low = code_only(sql).to_ascii_lowercase();
    let on = index_word(&low, "on")?;
    let sep = if revoke { "from" } else { "to" };
    let from_kw = index_word(&low, sep)?;
    if from_kw < on {
        return None;
    }

    let priv_spec = sql[first_word_end(sql)..on].trim();
    let mut obj_spec = sql[on + 2..from_kw].trim();
    let mut grantee_spec = sql[from_kw + sep.len()..].trim();
    grantee_spec = trim_end_chars(grantee_spec, "; \t\n");
    // Strip GRANT-option / cascade tails we don't model.
    for tail in [" with grant option", " cascade", " restrict"] {
        if let Some(k) = grantee_spec.to_ascii_lowercase().find(tail) {
            grantee_spec = &grantee_spec[..k];
        }
    }

    // Object must be a plain (optionally TABLE-qualified) table list.
    let o = obj_spec.trim();
    obj_spec = o.strip_prefix("TABLE").unwrap_or(o).trim();
    obj_spec = obj_spec.strip_prefix("table").unwrap_or(obj_spec).trim();
    let low_obj = obj_spec.to_lowercase();
    for kind in ["schema", "sequence", "database", "function", "all tables", "all "] {
        if low_obj.starts_with(kind) {
            return None;
        }
    }

    let privs = split_privs(priv_spec);
    let tables = split_tables(obj_spec);
    let grantees = split_grantees(grantee_spec);
    if privs.is_empty() || tables.is_empty() || grantees.is_empty() {
        return None;
    }
    Some((privs, tables, grantees))
}

/// Turns "SELECT, INSERT" / "ALL" / "ALL PRIVILEGES" / "SELECT (a, \"B\"), UPDATE (c)"
/// into a list. A comma inside a column list does not end a privilege.
fn split_privs(spec: &str) -> Vec<PrivSpec> {
    let mut out = Vec::new();
    let b = spec.as_bytes();
    let (mut depth, mut quoted, mut start) = (0_i32, false, 0);
    let mut items = Vec::new();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'"' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            b',' if !quoted && depth == 0 => {
                items.push(&spec[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&spec[start..]);
    for item in items {
        let item = item.trim();
        let (word, columns) = match item.find('(') {
            Some(i) => {
                let inner = item[i + 1..].trim_end();
                let inner = inner.strip_suffix(')').unwrap_or(inner);
                (item[..i].trim(), Some(split_grant_columns(inner)))
            }
            None => (item, None),
        };
        let mut name = word.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
        if name == "ALL PRIVILEGES" || (name.is_empty() && columns.is_none()) {
            name = "ALL".to_string();
        }
        if !name.is_empty() {
            out.push(PrivSpec { name, columns });
        }
    }
    out
}

/// The names of a column list: "a, \"B\"".
fn split_grant_columns(spec: &str) -> Vec<GrantColumn> {
    let mut out = Vec::new();
    let b = spec.as_bytes();
    let (mut quoted, mut start) = (false, 0);
    let mut push = |s: &str| {
        let s = s.trim();
        if let Some(inner) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
            out.push(GrantColumn {
                name: inner.replace("\"\"", "\""),
                quoted: true,
            });
        } else if !s.is_empty() {
            out.push(GrantColumn {
                name: s.to_lowercase(),
                quoted: false,
            });
        }
    };
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                push(&spec[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    push(&spec[start..]);
    out
}

fn split_tables(spec: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in spec.split(',') {
        if let Some(name) = table_name(&Word {
            text: t.trim(),
            quoted: false,
        }) {
            out.push(name.to_string());
        }
    }
    out
}

/// Returns role names, mapping GROUP/PUBLIC and quotes away.
fn split_grantees(spec: &str) -> Vec<String> {
    let mut out = Vec::new();
    for g in spec.split(',') {
        let g = g.trim();
        let g = g.strip_prefix("GROUP ").unwrap_or(g);
        let g = g.strip_prefix("group ").unwrap_or(g);
        let g = g.trim();
        if eq_fold(g, "public") {
            out.push("public".to_string());
            continue;
        }
        let g = trim_chars(g, "\"");
        if !g.is_empty() {
            out.push(g.to_string());
        }
    }
    out
}

/// Pulls the table reference from the tokens after CREATE/DROP TABLE, skipping "IF
/// NOT EXISTS" / "IF EXISTS".
fn created_table_name(toks: &[&str]) -> String {
    let mut toks = toks;
    while !toks.is_empty() {
        match toks[0].to_lowercase().as_str() {
            "if" | "not" | "exists" => toks = &toks[1..],
            _ => {
                let mut raw = toks[0];
                if let Some(i) = raw.find(['(', ';']) {
                    raw = &raw[..i];
                }
                if let Some(name) = table_name(&Word {
                    text: raw.trim(),
                    quoted: false,
                }) {
                    return name.to_string();
                }
                return String::new();
            }
        }
    }
    String::new()
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

/// The byte offset just past the first word of sql.
fn first_word_end(sql: &str) -> usize {
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n') {
        i += 1;
    }
    while i < b.len() && !(b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n') {
        i += 1;
    }
    i
}
