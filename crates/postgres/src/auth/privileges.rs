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
//! primary target of a DML statement and its FROM/JOIN tables) and held to
//! table-level grants alone.

use crate::auth::columns::{Access, Cols, Relations, accesses};
use crate::ddl::sequences::{is_ident_part, is_ident_start};
use crate::error::{Result, errf};
use crate::rewrite::strings::end_of_string_literal;
use crate::session::Session;
use crate::session::utility::{first_word_upper, second_word_upper};
use overlite_core::SqlState;
use overlite_core::Value;
use overlite_core::text::{eq_fold, quote_literal, split_words, trim_chars};
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
    /// let it run.
    pub(crate) fn check_privileges(&mut self, sql: &str) -> Result<()> {
        if self.priv_bypass() {
            return Ok(());
        }
        if let Some(accesses) = accesses(sql, &mut StoredRelations(self)) {
            return self.check_accesses(&accesses);
        }
        let reqs = required_privileges(sql);
        for r in &reqs {
            let table = self.stored_name(r.table);
            if !self.has_privilege(&table, r.privilege) {
                return Err(errf!(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    "permission denied for table {table}"
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
            "SELECT 1 FROM _overlite_grants WHERE lower(tablename) = lower({}) AND upper(privilege) IN (upper({}), 'ALL') AND lower(grantee) IN ({}) LIMIT 1",
            quote_literal(table),
            quote_literal(privilege.as_str()),
            in_list(grantees)
        );
        matches!(self.exec(&q, &[]), Ok(rs) if !rs.rows.is_empty())
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
            "SELECT 1 FROM _overlite_owners WHERE lower(tablename) = lower({}) AND lower(owner) IN ({}) LIMIT 1",
            quote_literal(table),
            in_list(roles)
        );
        matches!(self.exec(&q, &[]), Ok(rs) if !rs.rows.is_empty())
    }

    /// The current role plus every role it inherits from, transitively,
    /// following a membership edge only when the member has INHERIT. All names
    /// are lower-cased.
    pub(crate) fn effective_roles(&mut self) -> Vec<String> {
        let cur = self.current_role.to_lowercase();
        let mut roles = vec![cur.clone()];
        let q = format!(
            r#"WITH RECURSIVE eff(role) AS (
  SELECT lower({})
  UNION
  SELECT lower(m.roleof) FROM _overlite_memberships m
    JOIN eff e ON lower(m.member) = e.role
    JOIN _overlite_roles r ON lower(r.rolname) = e.role AND r.rolinherit <> 0
) SELECT role FROM eff"#,
            quote_literal(&self.current_role)
        );
        let Ok(rs) = self.exec(&q, &[]) else { return roles };
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(cur);
        for row in &rs.rows {
            if let Value::Text(s) = &row[0]
                && !seen.contains(s)
            {
                seen.insert(s.clone());
                roles.push(s.clone());
            }
        }
        roles
    }
}

/// The tables of a statement as this session finds them: by the search_path, under
/// their stored names.
struct StoredRelations<'a>(&'a mut Session);

impl Relations for StoredRelations<'_> {
    fn relation(&mut self, schema: &str, name: &str) -> (String, Option<Vec<String>>) {
        let quote = |s: &str| ["\"", &s.replace('"', "\"\""), "\""].concat();
        let reference = if schema.is_empty() {
            quote(name)
        } else {
            [quote(schema), ".".to_string(), quote(name)].concat()
        };
        let table = self.0.stored_name(&reference);
        let q = format!(
            "SELECT name FROM pragma_table_xinfo({}) WHERE hidden <> 1 ORDER BY cid",
            quote_literal(&table)
        );
        let columns = match self.0.exec(&q, &[]) {
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

/// The (table, privilege) pairs a statement needs.
fn required_privileges(sql: &str) -> Vec<TablePriv<'_>> {
    match first_word_upper(sql).as_str() {
        "SELECT" | "WITH" | "TABLE" => return tables_after(sql, &["from", "join"], Privilege::Select),
        "INSERT" => return tables_after(sql, &["into"], Privilege::Insert),
        "UPDATE" => return tables_after(sql, &["update"], Privilege::Update),
        "DELETE" => return tables_after(sql, &["from"], Privilege::Delete),
        "TRUNCATE" => return truncate_tables(sql),
        "DROP" | "ALTER" if eq_fold(&second_word(sql), "table") => return owned_tables(sql),
        _ => {}
    }
    Vec::new()
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
fn scan_words(sql: &str) -> Vec<Word<'_>> {
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
