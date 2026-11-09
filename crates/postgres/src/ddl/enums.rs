//! Enum types: CREATE/ALTER/DROP TYPE ... AS ENUM are intercepted here and
//! recorded in the engine's internal enum tables; a column whose type is a known
//! enum is rewritten to TEXT plus a CHECK constraint (native SQLite, so the file
//! stays usable without overlite and membership is enforced). pg_type/pg_enum are
//! populated from the same tables for introspection. Ordering/comparison uses the
//! stored text, not the declared enum order.

use crate::ddl::sequences::{int_text, is_ident_part, read_paren_args, split_top_level};
use crate::error::{Result, errf};
use crate::rewrite::strings::end_of_string_literal;
use crate::session::Session;
use crate::session::utility::{first_word_upper, unquote_ident};
use overlite_core::lazy_regex;
use overlite_core::text::{
    eq_fold, quote_literal, quoted, slice, split_words, trim_chars, trim_end_chars, trim_start_chars,
};
use overlite_core::{Command, SqlState};
use std::collections::HashMap;

lazy_regex!(RE_AS_ENUM, r"(?i)\bas\s+enum\b");

// Matches "AS (" -- a composite type definition.
lazy_regex!(RE_AS_COMPOSITE, r"(?i)\bas\s*\(");

/// Reports whether sql is a CREATE/ALTER/DROP TYPE statement (used by the
/// extended-query path to route it like other utility DDL).
pub(crate) fn is_type_ddl(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::type_ddl).unwrap_or_else(|| is_type_ddl_text(sql))
}

/// `is_type_ddl` by the text, for what Postgres' grammar does not read.
pub(crate) fn is_type_ddl_text(sql: &str) -> bool {
    if crate::ddl::objects::is_object_ddl(sql) {
        return true;
    }
    let f = split_words(sql);
    if f.len() < 2 || !eq_fold(f[1], "type") {
        return false;
    }
    matches!(f[0].to_uppercase().as_str(), "CREATE" | "ALTER" | "DROP")
}

impl Session {
    /// Handles CREATE/ALTER/DROP TYPE; None when it is not one.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_type_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        if let Some(r) = self.try_object_ddl(sql) {
            return Some(r);
        }
        let f = split_words(sql);
        if f.len() < 2 || !eq_fold(f[1], "type") {
            return None;
        }
        match f[0].to_uppercase().as_str() {
            "CREATE" => Some(self.create_enum_type(sql, &f)),
            "ALTER" => Some(self.alter_enum_type(sql, &f)),
            "DROP" => Some(self.drop_type(&f[2..]).map(|()| Command::from("DROP TYPE"))),
            _ => None,
        }
    }

    fn create_enum_type(&mut self, sql: &str, f: &[&str]) -> Result<Command> {
        if f.len() < 3 {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in CREATE TYPE"));
        }
        let name = enum_ident(f[2]);
        let loc = if let Some(m) = RE_AS_ENUM.find(sql) {
            m.end()
        } else {
            // CREATE TYPE ... AS (...) -- composite: record its name so it shows
            // in pg_type / \dT (its fields aren't modeled).
            if RE_AS_COMPOSITE.is_match(sql) {
                self.exec(
                    &("INSERT INTO _overlite_composite_types (typname) VALUES (".to_string()
                        + &quote_literal(&name)
                        + ")"),
                    &[],
                )?;
                return Ok(Command::from("CREATE TYPE"));
            }
            // Range / base types: accept as a no-op so dumps and tools don't fail.
            return Ok(Command::from("CREATE TYPE"));
        };
        let Some(open) = sql[loc..].find('(') else {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in CREATE TYPE ... AS ENUM"));
        };
        let Some((args, _)) = read_paren_args(sql, loc + open) else {
            return Err(errf!(SqlState::SYNTAX_ERROR, "unterminated ENUM label list"));
        };
        self.exec(
            &("INSERT INTO _overlite_enum_types (typname) VALUES (".to_string() + &quote_literal(&name) + ")"),
            &[],
        )
        .map_err(|e| match e.sql_state() {
            SqlState::UNIQUE_VIOLATION => errf!(SqlState::DUPLICATE_OBJECT, "type \"{name}\" already exists"),
            _ => e,
        })?;
        for (i, label) in parse_enum_labels(&args).iter().enumerate() {
            self.exec(
                &("INSERT INTO _overlite_enums (typname, label, sortorder) VALUES (".to_string()
                    + &quote_literal(&name)
                    + ", "
                    + &quote_literal(label)
                    + ", "
                    + &int_text((i + 1) as i64)
                    + ")"),
                &[],
            )?;
        }
        Ok(Command::from("CREATE TYPE"))
    }

    fn alter_enum_type(&mut self, sql: &str, f: &[&str]) -> Result<Command> {
        if f.len() < 4 {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error in ALTER TYPE"));
        }
        let name = enum_ident(f[2]);
        let rest = &f[3..];

        // ALTER TYPE x RENAME TO y
        if rest.len() >= 3 && eq_fold(rest[0], "rename") && eq_fold(rest[1], "to") {
            let new_name = enum_ident(rest[2]);
            self.exec(
                &("UPDATE _overlite_enum_types SET typname = ".to_string()
                    + &quote_literal(&new_name)
                    + " WHERE typname = "
                    + &quote_literal(&name)),
                &[],
            )?;
            self.exec(
                &("UPDATE _overlite_enums SET typname = ".to_string()
                    + &quote_literal(&new_name)
                    + " WHERE typname = "
                    + &quote_literal(&name)),
                &[],
            )?;
            return Ok(Command::from("ALTER TYPE"));
        }

        // ALTER TYPE x RENAME VALUE 'a' TO 'b'
        if rest.len() >= 2 && eq_fold(rest[0], "rename") && eq_fold(rest[1], "value") {
            let qs = quoted_strings(&sql[re_after_word(sql, "value")..]);
            if qs.len() < 2 {
                return Err(errf!(
                    SqlState::SYNTAX_ERROR,
                    "syntax error in ALTER TYPE ... RENAME VALUE"
                ));
            }
            self.exec(
                &("UPDATE _overlite_enums SET label = ".to_string()
                    + &quote_literal(&qs[1])
                    + " WHERE typname = "
                    + &quote_literal(&name)
                    + " AND label = "
                    + &quote_literal(&qs[0])),
                &[],
            )?;
            return Ok(Command::from("ALTER TYPE"));
        }

        // ALTER TYPE x ADD VALUE [IF NOT EXISTS] 'v' [BEFORE|AFTER 'other']
        if rest.len() >= 2 && eq_fold(rest[0], "add") && eq_fold(rest[1], "value") {
            self.add_enum_value(sql, &name, &rest[2..])?;
            return Ok(Command::from("ALTER TYPE"));
        }
        Ok(Command::from("ALTER TYPE")) // accept unrecognized ALTER TYPE as a no-op
    }

    fn add_enum_value(&mut self, sql: &str, name: &str, tokens: &[&str]) -> Result<()> {
        if !self.enum_type_exists(name) {
            return Err(errf!(
                SqlState::UNDEFINED_OBJECT,
                "type {:?} does not exist",
                quoted(name)
            ));
        }
        let qs = quoted_strings(&sql[re_after_word(sql, "value")..]);
        if qs.is_empty() {
            return Err(errf!(
                SqlState::SYNTAX_ERROR,
                "syntax error in ALTER TYPE ... ADD VALUE"
            ));
        }
        let label = &qs[0];
        let if_not_exists =
            tokens.len() >= 3 && eq_fold(tokens[0], "if") && eq_fold(tokens[1], "not") && eq_fold(tokens[2], "exists");
        if self.enum_label_exists(name, label) {
            if if_not_exists {
                return Ok(());
            }
            return Err(errf!(
                SqlState::DUPLICATE_OBJECT,
                "label {:?} already exists in type {:?}",
                quoted(label),
                quoted(name)
            ));
        }

        let mut order = "(SELECT COALESCE(MAX(sortorder), 0) + 1 FROM _overlite_enums WHERE typname = ".to_string()
            + &quote_literal(name)
            + ")";
        let (pos, anchor) = enum_position(tokens, &qs);
        if !pos.is_empty() && !anchor.is_empty() {
            let mut delta = "- 0.5";
            if pos == "after" {
                delta = "+ 0.5";
            }
            order = "(SELECT sortorder ".to_string()
                + delta
                + " FROM _overlite_enums WHERE typname = "
                + &quote_literal(name)
                + " AND label = "
                + &quote_literal(&anchor)
                + ")";
        }
        self.exec(
            &("INSERT INTO _overlite_enums (typname, label, sortorder) VALUES (".to_string()
                + &quote_literal(name)
                + ", "
                + &quote_literal(label)
                + ", "
                + &order
                + ")"),
            &[],
        )?;
        Ok(())
    }

    fn drop_type(&mut self, rest: &[&str]) -> Result<()> {
        let mut rest = rest;
        let if_exists = rest.len() >= 2 && eq_fold(rest[0], "if") && eq_fold(rest[1], "exists");
        if if_exists {
            rest = &rest[2..];
        }
        for name in rest.join(" ").split(',') {
            let name = enum_ident(trim_end_chars(name, ";").trim());
            if name.is_empty() || eq_fold(&name, "cascade") || eq_fold(&name, "restrict") {
                continue;
            }
            if !self.enum_type_exists(&name) && !self.composite_type_exists(&name) {
                if if_exists {
                    self.c
                        .send_notice(&format!("type \"{name}\" does not exist, skipping"))?;
                    continue;
                }
                return Err(errf!(SqlState::UNDEFINED_OBJECT, "type \"{name}\" does not exist"));
            }
            self.exec(
                &("DELETE FROM _overlite_enum_types WHERE typname = ".to_string() + &quote_literal(&name)),
                &[],
            )?;
            self.exec(
                &("DELETE FROM _overlite_enums WHERE typname = ".to_string() + &quote_literal(&name)),
                &[],
            )?;
            self.exec(
                &("DELETE FROM _overlite_composite_types WHERE typname = ".to_string() + &quote_literal(&name)),
                &[],
            )?;
        }
        Ok(())
    }

    fn composite_type_exists(&mut self, name: &str) -> bool {
        let q = "SELECT 1 FROM _overlite_composite_types WHERE typname = ".to_string() + &quote_literal(name);
        self.exec(&q, &[]).is_ok_and(|rs| !rs.rows.is_empty())
    }

    fn enum_type_exists(&mut self, name: &str) -> bool {
        match self.exec(
            &("SELECT 1 FROM _overlite_enum_types WHERE typname = ".to_string() + &quote_literal(name)),
            &[],
        ) {
            Ok(rs) => !rs.rows.is_empty(),
            Err(_) => false,
        }
    }

    fn enum_label_exists(&mut self, name: &str, label: &str) -> bool {
        match self.exec(
            &("SELECT 1 FROM _overlite_enums WHERE typname = ".to_string()
                + &quote_literal(name)
                + " AND label = "
                + &quote_literal(label)),
            &[],
        ) {
            Ok(rs) => !rs.rows.is_empty(),
            Err(_) => false,
        }
    }

    // --- enum column rewriting ------------------------------------------------

    /// Rewrites enum columns in a CREATE TABLE to TEXT + CHECK. Runs before the
    /// dialect rewrite (like expand_sequences), so the labels it emits are
    /// protected by the string-literal-aware rewrite.
    pub(crate) fn expand_enums(&mut self, sql: &str) -> Result<String> {
        if first_word_upper(sql) != "CREATE" || !contains_word_fold(sql, "table") {
            return Ok(sql.to_string());
        }
        let enums = self.load_enums()?;
        if enums.is_empty() {
            return Ok(sql.to_string());
        }
        Ok(rewrite_enum_columns(sql, &enums))
    }

    fn load_enums(&mut self) -> Result<HashMap<String, Vec<String>>> {
        let rs = self.exec(
            "SELECT typname, label FROM _overlite_enums ORDER BY typname, sortorder",
            &[],
        )?;
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rs.rows {
            if row.len() < 2 {
                continue;
            }
            let key = row[0].to_string().to_lowercase();
            m.entry(key).or_default().push(row[1].to_string());
        }
        Ok(m)
    }
}

/// Replaces each column whose declared type is a known enum with
/// "col text CHECK (col IN ('a','b',...))".
fn rewrite_enum_columns(sql: &str, enums: &HashMap<String, Vec<String>>) -> String {
    let Some(open) = index_top_level_byte(sql, b'(') else {
        return sql.to_string();
    };
    let Some((inner, end)) = read_paren_args(sql, open) else {
        return sql.to_string();
    };
    let mut parts = split_top_level(&inner);
    let mut changed = false;
    for p in &mut parts {
        let np = rewrite_enum_column_def(p, enums);
        if np != *p {
            *p = np;
            changed = true;
        }
    }
    if !changed {
        return sql.to_string();
    }
    sql[..open].to_string() + "(" + &parts.join(",") + ")" + &sql[end..]
}

fn rewrite_enum_column_def(def: &str, enums: &HashMap<String, Vec<String>>) -> String {
    let trimmed = trim_start_chars(def, " \t\r\n");
    let lead = &def[..def.len() - trimmed.len()];
    let (name, after_name) = first_token(trimmed);
    if name.is_empty() {
        return def.to_string();
    }
    let typ_trimmed = trim_start_chars(after_name, " \t\r\n");
    let (typ, after_type) = first_token(typ_trimmed);
    let Some(labels) = enums.get(&trim_chars(typ, "\"").to_lowercase()) else {
        return def.to_string();
    };
    let quoted: Vec<String> = labels.iter().map(|l| quote_literal(l)).collect();
    lead.to_string() + name + " text CHECK (" + name + " IN (" + &quoted.join(", ") + "))" + after_type
}

// --- parsing helpers ----------------------------------------------------------

/// Extracts a bare type name from a token: unquote and drop any schema qualifier
/// (public.mood -> mood).
fn enum_ident(tok: &str) -> String {
    let u = unquote_ident(tok.trim());
    let mut t = u.as_str();
    if let Some(i) = t.rfind('.') {
        t = &t[i + 1..];
    }
    trim_chars(t, "\"").to_string()
}

/// Splits a quoted, comma-separated label list into its values.
fn parse_enum_labels(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    for p in split_top_level(args) {
        let p = p.trim();
        let p = p.strip_prefix('\'').unwrap_or(p);
        let p = p.strip_suffix('\'').unwrap_or(p);
        let p = p.replace("''", "'");
        if !p.is_empty() {
            out.push(p);
        }
    }
    out
}

/// Every single-quoted literal's content in s (in order), with doubled quotes
/// unescaped.
fn quoted_strings(s: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\'' {
            i += 1;
            continue;
        }
        let j = end_of_string_literal(s, i);
        let body = slice(s, i + 1, j - 1);
        out.push(body.replace("''", "'"));
        i = j;
    }
    out
}

/// Reads a BEFORE/AFTER anchor from an ADD VALUE clause. qs are the quoted strings
/// in order (qs[0] is the new label); the anchor is qs[1].
fn enum_position(tokens: &[&str], qs: &[String]) -> (String, String) {
    for t in tokens {
        let l = t.to_lowercase();
        if (l == "before" || l == "after") && qs.len() >= 2 {
            return (l, qs[1].clone());
        }
    }
    (String::new(), String::new())
}

/// The first identifier token (bare or "quoted") of s and the remainder; "" when
/// s does not start with an identifier.
fn first_token(s: &str) -> (&str, &str) {
    if s.is_empty() {
        return ("", "");
    }
    let b = s.as_bytes();
    if b[0] == b'"' {
        let mut j = 1;
        while j < b.len() {
            if b[j] == b'"' {
                j += 1;
                break;
            }
            j += 1;
        }
        return (&s[..j], &s[j..]);
    }
    let mut j = 0;
    while j < b.len() && is_ident_part(b[j]) {
        j += 1;
    }
    if j == 0 {
        return ("", s);
    }
    (&s[..j], &s[j..])
}

/// Finds the first byte c that is outside any string literal.
fn index_top_level_byte(s: &str, c: u8) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_of_string_literal(s, i);
            continue;
        }
        if b[i] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn contains_word_fold(sql: &str, word: &str) -> bool {
    let low = sql.to_lowercase();
    let w = word.to_lowercase();
    let lb = low.as_bytes();
    let mut from = 0;
    loop {
        let i = match low[from..].find(&w) {
            Some(i) => i + from,
            None => return false,
        };
        let before = i == 0 || !is_ident_part(lb[i - 1]);
        let after = i + w.len() >= lb.len() || !is_ident_part(lb[i + w.len()]);
        if before && after {
            return true;
        }
        from = i + w.len();
    }
}

/// The index just past the first whole-word occurrence of word (case-insensitive)
/// in sql, or 0 if absent.
fn re_after_word(sql: &str, word: &str) -> usize {
    let low = sql.to_ascii_lowercase();
    let w = word.to_ascii_lowercase();
    let lb = low.as_bytes();
    let mut from = 0;
    loop {
        let i = match low[from..].find(&w) {
            Some(i) => i + from,
            None => return 0,
        };
        let before = i == 0 || !is_ident_part(lb[i - 1]);
        let after = i + w.len() >= lb.len() || !is_ident_part(lb[i + w.len()]);
        if before && after {
            return i + w.len();
        }
        from = i + w.len();
    }
}
