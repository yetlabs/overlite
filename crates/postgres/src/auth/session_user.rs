//! Session identity: current_user / session_user / current_role reflect the
//! connecting role (and SET ROLE / SET SESSION AUTHORIZATION). Because the engine
//! scalar functions are process-global, these niladic functions are substituted
//! with the session's role literal before the statement runs.

use crate::auth::privileges::lowercase;
use crate::ddl::sequences::{is_ident_part, is_ident_start};
use crate::ddl::words::{Word, ddl_words};
use crate::error::{Result, errf};
use crate::rewrite::functions::skip_spaces;
use crate::rewrite::strings::end_of_string_literal;
use crate::session::Session;
use overlite_core::SqlState;
use overlite_core::text::{eq_fold, quote_literal, quoted, split_words, string_from_cut_utf8};

impl Session {
    /// Replaces current_user / current_role / session_user (bare or called) with
    /// the session's role as a string literal, outside string literals and
    /// skipping quoted/qualified identifiers.
    pub(crate) fn expand_session_user(&self, sql: &str) -> String {
        let low = lowercase(sql);
        if !low.contains("_user") && !low.contains("current_role") {
            return sql.to_string();
        }
        let s = sql.as_bytes();
        // Every piece copied is a slice of valid UTF-8 cut at ASCII bytes.
        let mut b: Vec<u8> = Vec::with_capacity(sql.len());
        let mut i = 0;
        while i < s.len() {
            let c = s[i];
            if c == b'\'' {
                let j = end_of_string_literal(sql, i);
                b.extend_from_slice(&s[i..j]);
                i = j;
                continue;
            }
            if !is_ident_start(c) {
                b.push(c);
                i += 1;
                continue;
            }
            // A qualified ("n.current_user") or quoted identifier is not the function.
            let qualified = i > 0 && (s[i - 1] == b'.' || s[i - 1] == b'"' || is_ident_part(s[i - 1]));
            let mut j = i;
            while j < s.len() && is_ident_part(s[j]) {
                j += 1;
            }
            let lit = self.user_literal(lowercase(&sql[i..j]).as_ref());
            let lit = match lit {
                Some(l) if !qualified => l,
                _ => {
                    b.extend_from_slice(&s[i..j]);
                    i = j;
                    continue;
                }
            };
            // Consume an optional "()" call suffix.
            let k = skip_spaces(sql, j);
            if k < s.len() && s[k] == b'(' {
                let m = skip_spaces(sql, k + 1);
                if m < s.len() && s[m] == b')' {
                    j = m + 1;
                }
            }
            b.extend_from_slice(lit.as_bytes());
            i = j;
        }
        string_from_cut_utf8(b)
    }

    fn user_literal(&self, word: &str) -> Option<String> {
        match word {
            "current_user" | "current_role" => Some(quote_literal(&self.current_role)),
            "session_user" => Some(quote_literal(&self.session_user)),
            _ => None,
        }
    }

    /// Updates the session identity for a SET/RESET ROLE / SESSION AUTHORIZATION
    /// statement.
    pub(crate) fn apply_set_role(&mut self, sql: &str) -> Result<()> {
        let change = match role_change(sql) {
            Some(change) => change?,
            None => return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in SET ROLE")),
        };
        if change.local && !self.in_tx() {
            // As Postgres: a warning, and nothing changes.
            return self.c.send_warning(
                SqlState::NO_ACTIVE_SQL_TRANSACTION,
                "SET LOCAL can only be used in transaction blocks",
            );
        }
        let before = (self.current_role.clone(), self.session_user.clone());
        let done = self.switch_role(&change);
        if done.is_ok() && self.in_tx() {
            // The end of the transaction undoes SET LOCAL, and a rollback any SET.
            if change.local {
                self.role_local_restore.get_or_insert(before);
            } else {
                self.role_tx_restore.get_or_insert(before);
            }
        }
        overlite_engine::set_session_role(&self.current_role);
        done
    }

    /// At the end of a transaction: SET LOCAL ROLE lapses, and a rollback undoes a SET
    /// ROLE or SESSION AUTHORIZATION, as each is a setting of the transaction in
    /// Postgres.
    pub(crate) fn end_tx_role(&mut self, committed: bool) {
        let local = self.role_local_restore.take();
        let tx = self.role_tx_restore.take();
        let back = if committed { local } else { tx.or(local) };
        if let Some((role, session_user)) = back {
            self.current_role = role;
            self.session_user = session_user;
            overlite_engine::set_session_role(&self.current_role);
        }
    }

    fn switch_role(&mut self, change: &crate::ast::classify::RoleChange) -> Result<()> {
        if !change.session_authorization {
            if change.reset || change.target.is_empty() || eq_fold(&change.target, "default") {
                self.current_role = self.session_user.clone();
                return Ok(());
            }
            let target = change.target.clone();
            if !self.role_exists(&target) {
                return Err(errf!(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "role {:?} does not exist",
                    quoted(target)
                ));
            }
            if !self.can_set_role(&target) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "permission denied to set role {:?}",
                    quoted(target)
                ));
            }
            self.current_role = target;
            return Ok(());
        }

        // SET/RESET SESSION AUTHORIZATION: only a superuser session may change it.
        if change.reset || change.target.is_empty() || eq_fold(&change.target, "default") {
            self.session_user = self.auth_user.clone();
            self.current_role = self.auth_user.clone();
            return Ok(());
        }
        let target = change.target.clone();
        let auth = self.auth_user.clone();
        if !self.role_bypasses(&auth) {
            return Err(errf!(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied to set session authorization"
            ));
        }
        if !self.role_exists(&target) {
            return Err(errf!(
                SqlState::INVALID_PARAMETER_VALUE,
                "role {:?} does not exist",
                quoted(target)
            ));
        }
        self.session_user = target.clone();
        self.current_role = target;
        Ok(())
    }

    /// Whether the session may SET ROLE to target: always to its own
    /// login/session role, unrestricted for a superuser or unmanaged session
    /// user, otherwise only to a role the session user is a member of (directly
    /// or indirectly). Unlike privilege inheritance, membership here is not gated
    /// by INHERIT: SET ROLE is exactly how a NOINHERIT member uses a role.
    fn can_set_role(&mut self, target: &str) -> bool {
        if eq_fold(target, &self.session_user) || eq_fold(target, &self.auth_user) {
            return true;
        }
        let su = self.session_user.clone();
        if self.role_bypasses(&su) {
            return true;
        }
        self.is_member_of(&su, target)
    }

    /// Whether member belongs to target transitively via the membership graph
    /// (ignoring INHERIT).
    fn is_member_of(&mut self, member: &str, target: &str) -> bool {
        let q = format!(
            r#"WITH RECURSIVE m(role) AS (
  SELECT lower({})
  UNION
  SELECT lower(x.roleof) FROM _overlite_memberships x JOIN m ON lower(x.member) = m.role
) SELECT 1 FROM m WHERE role = lower({}) LIMIT 1"#,
            quote_literal(member),
            quote_literal(target)
        );
        matches!(self.exec(&q, &[]), Ok(rs) if !rs.rows.is_empty())
    }

    pub(crate) fn role_exists(&mut self, name: &str) -> bool {
        let q = format!(
            "SELECT 1 FROM _overlite_roles WHERE rolname = {} COLLATE NOCASE",
            quote_literal(name)
        );
        matches!(self.exec(&q, &[]), Ok(rs) if !rs.rows.is_empty())
    }
}

/// Whether sql is a SET/RESET ROLE or SET/RESET SESSION AUTHORIZATION statement
/// (handled by the session, not passed to SQLite), of any scope: SET LOCAL ROLE and
/// SET SESSION ROLE included.
pub(crate) fn is_set_role(sql: &str) -> bool {
    role_change(sql).is_some()
}

/// What a SET or RESET of the role or the session's authorization asks for, by its
/// tree (`classify::set_role`), or by its text where Postgres' grammar does not read
/// it; None for any other statement.
pub(crate) fn role_change(sql: &str) -> Option<Result<crate::ast::classify::RoleChange>> {
    if let Some(change) = crate::ast::classify::read(sql, crate::ast::classify::set_role) {
        return change.map(Ok);
    }
    is_set_role_text(sql).then(|| role_change_text(sql))
}

/// `is_set_role` by the text: SET or RESET, then ROLE or SESSION AUTHORIZATION.
pub(crate) fn is_set_role_text(sql: &str) -> bool {
    let f = split_words(sql);
    if f.len() < 2 {
        return false;
    }
    match f[0].to_uppercase().as_str() {
        "SET" | "RESET" => {
            eq_fold(f[1], "role") || (f.len() >= 3 && eq_fold(f[1], "session") && eq_fold(f[2], "authorization"))
        }
        _ => false,
    }
}

fn role_change_text(sql: &str) -> Result<crate::ast::classify::RoleChange> {
    let f = split_words(sql);
    let session_authorization = !f.get(1).is_some_and(|w| eq_fold(w, "role"));
    let reset = f.first().is_some_and(|w| eq_fold(w, "reset"));
    let target = if reset { String::new() } else { role_target(sql)? };
    let reset = reset || (!session_authorization && eq_fold(&target, "none"));
    Ok(crate::ast::classify::RoleChange {
        session_authorization,
        reset,
        local: false,
        target,
    })
}

/// Reads the role name after an optional TO / = from the tokens.
fn role_target(sql: &str) -> Result<String> {
    let words = ddl_words(sql)?;
    let value = words
        .iter()
        .skip(2)
        .find(|w| !w.is("to") && **w != Word::Punct('=') && !w.is("authorization"));
    Ok(match value {
        Some(Word::Str(s)) => s.clone(),
        Some(word) => word.name().unwrap_or_default().to_string(),
        None => String::new(),
    })
}
