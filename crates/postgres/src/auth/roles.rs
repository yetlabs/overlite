//! CREATE/ALTER/DROP ROLE|USER|GROUP, translated to the engine's internal
//! _overlite_roles table (so they succeed and show up in \du / pg_roles).

use crate::auth::scram::build_scram_verifier;
use crate::ddl::words::{Word, ddl_words, syntax_error_at};
use crate::error::{Result, errf};
use crate::session::Session;
use overlite_core::Value;
use overlite_core::text::{eq_fold, quote_literal, split_words, trim_end_chars};
use overlite_core::{Command, SqlState};
use std::collections::HashMap;

/// Whether try_role_ddl takes this statement.
pub(crate) fn is_role_ddl(sql: &str) -> bool {
    let f = split_words(sql);
    f.len() >= 2
        && ["CREATE", "ALTER", "DROP"].iter().any(|v| eq_fold(f[0], v))
        && ["ROLE", "USER", "GROUP"]
            .iter()
            .any(|o| eq_fold(trim_end_chars(f[1], ";"), o))
        && !f.get(2).is_some_and(|w| eq_fold(w, "mapping"))
}

impl Session {
    /// Handles CREATE/ALTER/DROP ROLE|USER|GROUP. GRANT/REVOKE are no-ops handled
    /// in intercept_utility, since SQLite has no per-object privileges.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_role_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        let fields = split_words(sql);
        if fields.get(2).is_some_and(|w| eq_fold(w, "mapping")) {
            return None; // CREATE USER MAPPING: a foreign-data statement
        }
        if fields.len() < 2 {
            return None;
        }
        let verb = fields[0].to_uppercase();
        let obj = trim_end_chars(fields[1], ";").to_uppercase();
        if verb == "ALTER" && obj == "DEFAULT" {
            // ALTER DEFAULT PRIVILEGES: accept, no-op
            return Some(Ok(Command::from("ALTER DEFAULT PRIVILEGES")));
        }
        if !is_role_ddl(sql) {
            return None;
        }
        Some(self.run_role_ddl(&verb, &obj, sql))
    }

    fn run_role_ddl(&mut self, verb: &str, obj: &str, sql: &str) -> Result<Command> {
        let mut words = ddl_words(sql)?;
        if words.last() == Some(&Word::Punct(';')) {
            words.pop();
        }
        // After the verb and the object.
        let words = words.get(2..).unwrap_or(&[]);
        match verb {
            "CREATE" => {
                let name = self.role_name_at(words, 0)?;
                self.require_role_admin("CREATE", &name)?;
                self.create_role(&name, &words[1..], obj)?;
                self.grant_creator_admin(&name);
                Ok(Command::from("CREATE ROLE"))
            }
            "ALTER" => {
                let name = self.role_name_at(words, 0)?;
                self.require_role_admin("ALTER", &name)?;
                self.alter_role(&name, &words[1..])?;
                Ok(Command::from("ALTER ROLE"))
            }
            _ => {
                let if_exists = words.len() >= 2 && words[0].is("if") && words[1].is("exists");
                let names = &words[if if_exists { 2 } else { 0 }..];
                let mut roles = Vec::new();
                for at in (0..names.len()).step_by(2) {
                    roles.push(self.role_name_at(names, at)?);
                    if names.get(at + 1).is_some_and(|w| *w != Word::Punct(',')) {
                        return Err(syntax_error_at(names.get(at + 1)));
                    }
                }
                if roles.is_empty() {
                    return Err(syntax_error_at(None));
                }
                self.require_role_admin("DROP", roles.first().map_or("", String::as_str))?;
                for role in &roles {
                    self.drop_role(role, if_exists)?;
                }
                Ok(Command::from("DROP ROLE"))
            }
        }
    }

    /// The role a word names: CURRENT_USER and the like stand for one.
    fn role_name_at(&self, words: &[Word], at: usize) -> Result<String> {
        match words.get(at) {
            Some(w) if w.is("current_user") || w.is("current_role") => Ok(self.current_role.clone()),
            Some(w) if w.is("session_user") => Ok(self.session_user.clone()),
            Some(w) => w.name().map(str::to_string).ok_or_else(|| syntax_error_at(Some(w))),
            None => Err(syntax_error_at(None)),
        }
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

    fn create_role(&mut self, name: &str, options: &[Word], obj: &str) -> Result<()> {
        if name == "public" || name == "none" || name.starts_with("pg_") {
            return Err(errf!(SqlState::RESERVED_NAME, "role name \"{name}\" is reserved"));
        }
        let mut attrs = parse_role_attrs(options);
        if !attrs.contains_key("rolcanlogin") && obj == "USER" {
            attrs.insert("rolcanlogin", 1); // CREATE USER defaults to LOGIN
        }

        let mut cols: Vec<String> = vec!["rolname".to_string()];
        let mut ph: Vec<String> = vec!["?".to_string()];
        let mut vals: Vec<Value> = vec![Value::Text(name.to_string())];
        for (col, v) in &attrs {
            cols.push(col.to_string());
            ph.push("?".to_string());
            vals.push(Value::Int(*v));
        }
        if let Some(pw) = extract_password(options) {
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

    fn alter_role(&mut self, name: &str, options: &[Word]) -> Result<()> {
        if !self.role_exists(name) {
            // The role logged in may be one the catalog never held.
            if name == self.current_role {
                return Ok(());
            }
            return Err(errf!(SqlState::UNDEFINED_OBJECT, "role \"{name}\" does not exist"));
        }

        // ALTER ROLE x RENAME TO y
        if options.first().is_some_and(|w| w.is("rename")) {
            if !options.get(1).is_some_and(|w| w.is("to")) {
                return Err(syntax_error_at(options.get(1)));
            }
            let Some(to) = options.get(2).and_then(Word::name) else {
                return Err(syntax_error_at(options.get(2)));
            };
            return self.rename_role(name, to);
        }

        let mut set: Vec<String> = Vec::with_capacity(2);
        let mut vals: Vec<Value> = Vec::with_capacity(3);
        if let Some(pw) = extract_password(options) {
            let verifier = build_scram_verifier(&pw)?;
            set.push("rolpassword = ?".to_string());
            vals.push(Value::Text(verifier));
        }
        for (col, v) in &parse_role_attrs(options) {
            set.push(format!("{col} = ?"));
            vals.push(Value::Int(*v));
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

/// The string after PASSWORD in a role's options. PASSWORD NULL sets none.
fn extract_password(options: &[Word]) -> Option<String> {
    let at = options.iter().position(|w| w.is("password"))?;
    match options.get(at + 1)? {
        Word::Str(password) => Some(password.clone()),
        _ => None,
    }
}

/// Maps the role option keywords that were explicitly given to their column and
/// value, so ALTER only touches what was named.
fn parse_role_attrs(options: &[Word]) -> HashMap<&'static str, i64> {
    let mut m = HashMap::new();
    for t in options {
        let Word::Bare(word) = t else {
            continue; // quoted, it is a name
        };
        match word.to_uppercase().as_str() {
            "SUPERUSER" => {
                m.insert("rolsuper", 1);
            }
            "NOSUPERUSER" => {
                m.insert("rolsuper", 0);
            }
            "CREATEDB" => {
                m.insert("rolcreatedb", 1);
            }
            "NOCREATEDB" => {
                m.insert("rolcreatedb", 0);
            }
            "CREATEROLE" => {
                m.insert("rolcreaterole", 1);
            }
            "NOCREATEROLE" => {
                m.insert("rolcreaterole", 0);
            }
            "LOGIN" => {
                m.insert("rolcanlogin", 1);
            }
            "NOLOGIN" => {
                m.insert("rolcanlogin", 0);
            }
            "INHERIT" => {
                m.insert("rolinherit", 1);
            }
            "NOINHERIT" => {
                m.insert("rolinherit", 0);
            }
            "REPLICATION" => {
                m.insert("rolreplication", 1);
            }
            "NOREPLICATION" => {
                m.insert("rolreplication", 0);
            }
            "BYPASSRLS" => {
                m.insert("rolbypassrls", 1);
            }
            "NOBYPASSRLS" => {
                m.insert("rolbypassrls", 0);
            }
            _ => {}
        }
    }
    m
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn extract_password_cases() {
        let password = |sql: &str| extract_password(&ddl_words(sql).unwrap());
        assert_eq!(
            password("CREATE ROLE alice LOGIN PASSWORD 'alicepw'").as_deref(),
            Some("alicepw")
        );
        assert_eq!(
            password("ALTER ROLE bob ENCRYPTED PASSWORD 'it''s me'").as_deref(),
            Some("it's me")
        );
        assert_eq!(password("ALTER ROLE bob CREATEDB"), None);
        assert_eq!(password("ALTER ROLE bob PASSWORD NULL"), None);
        assert_eq!(password(r#"CREATE ROLE "password""#), None);
    }

    #[test]
    fn options_in_quotes_are_names() {
        let attrs = parse_role_attrs(&ddl_words(r#""login" LOGIN "NOLOGIN""#).unwrap());
        assert_eq!(attrs.get("rolcanlogin"), Some(&1));
        assert!(parse_role_attrs(&ddl_words(r#""superuser""#).unwrap()).is_empty());
    }
}
