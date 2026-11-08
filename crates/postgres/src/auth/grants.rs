//! GRANT/REVOKE handling and table-ownership tracking. These write the internal
//! _overlite_grants / _overlite_owners tables that privileges.rs reads.

use crate::auth::privileges::{Privilege, Word, owned_tables, table_name};
use crate::ddl::alter::split_qualified_ref;
use crate::ddl::sequences::is_ident_part;
use crate::error::{Result, errf};
use crate::session::Session;
use crate::session::utility::first_word_upper;
use overlite_core::text::{eq_fold, quote_literal, quoted, split_words, trim_chars, trim_end_chars};
use overlite_core::{Command, SqlState};
use overlite_engine::{code_only, forget_table_rows_sql, stored_table_name};

/// Whether sql is a GRANT or REVOKE statement.
pub(crate) fn is_grant(sql: &str) -> bool {
    let w = first_word_upper(sql);
    w == "GRANT" || w == "REVOKE"
}

impl Session {
    /// Parses a table GRANT/REVOKE and updates _overlite_grants. Role (GRANT role
    /// TO role) and non-table object grants are accepted as no-ops. Returns the
    /// command tag ("GRANT"/"REVOKE").
    pub(crate) fn apply_grant(&mut self, sql: &str) -> Result<Command> {
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

        for tbl in &tables {
            for who in &grantees {
                for priv_ in &privs {
                    if revoke {
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
        let f = split_words(sql);
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
        }
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

/// Extracts the privileges, tables, and grantees from a table GRANT/REVOKE. None
/// for forms it doesn't model (e.g. GRANT <role> TO, GRANT ... ON
/// SCHEMA/SEQUENCE/DATABASE, GRANT ... ON ALL TABLES IN SCHEMA).
fn parse_grant(sql: &str, revoke: bool) -> Option<(Vec<String>, Vec<String>, Vec<String>)> {
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

/// Turns "SELECT, INSERT" / "ALL" / "ALL PRIVILEGES" into a list.
fn split_privs(spec: &str) -> Vec<String> {
    let l = spec.trim().to_lowercase();
    if l == "all" || l == "all privileges" || l.is_empty() {
        return vec!["ALL".to_string()];
    }
    let mut out = Vec::new();
    for p in spec.split(',') {
        let mut p = p.trim().to_uppercase();
        if let Some(i) = p.find('(') {
            // column-list priv -> drop columns
            p = p[..i].trim().to_string();
        }
        if !p.is_empty() {
            out.push(p);
        }
    }
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
