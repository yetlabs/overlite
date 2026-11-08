//! Objects SQLite has no kind for: materialized views, extensions, and the
//! foreign-data statements, which are refused rather than taken without effect.
//!
//! CREATE EXTENSION takes the extensions whose functions and types this server
//! provides, and records them for pg_extension; any other is "not available", as on
//! a Postgres without its files, rather than taken and then missing.
//!
//! A materialized view is a table holding the rows of its query, beside a SQLite view
//! `_overlite_mv_<name>` holding the query itself. REFRESH empties the table and
//! fills it from the view; the catalog gives the table relkind 'm' when its view is
//! there, and the `_overlite_` prefix keeps the view out of every catalog.

use crate::error::{Result, errf};
use crate::rewrite::tokens::code_tokens;
use crate::session::Session;
use crate::session::utility::unquote_ident;
use overlite_core::text::{eq_fold, quote_ident, quote_literal, quoted};
use overlite_core::{Command, SqlState, Value};

/// The view that keeps a materialized view's query.
pub(crate) const MATVIEW_PREFIX: &str = "_overlite_mv_";

/// The leading words of sql, lower case, up to n of them, from its code.
fn lead_words(sql: &str, n: usize) -> Vec<String> {
    let toks = code_tokens(sql);
    toks.iter()
        .take(n)
        .map(|t| sql[t.span.clone()].to_ascii_lowercase())
        .collect()
}

/// Whether try_object_ddl takes sql. Every statement comes through here, so the
/// first word is read before anything is tokenized.
pub(crate) fn is_object_ddl(sql: &str) -> bool {
    let head = sql.trim_start().as_bytes();
    let starts = |w: &str| head.len() >= w.len() && head[..w.len()].eq_ignore_ascii_case(w.as_bytes());
    if !["create", "drop", "alter", "refresh", "import"]
        .iter()
        .any(|w| starts(w))
    {
        return false;
    }
    let w = lead_words(sql, 4);
    if w.get(1).is_some_and(|t| t == "table") {
        return partitioned(sql);
    }
    let w: Vec<&str> = w.iter().map(String::as_str).collect();
    let ddl = matches!(w.first(), Some(&("create" | "drop" | "alter")));
    match w.as_slice() {
        ["import", "foreign", ..] => true,
        [_, "extension" | "collation" | "server" | "foreign", ..] | [_, "user", "mapping", ..] => ddl,
        ["create" | "refresh" | "drop" | "alter", "materialized", "view", ..] => true,
        _ => false,
    }
}

impl Session {
    /// Handles the statements of this module; None for any other.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_object_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_object_ddl(sql) {
            return None;
        }
        let w = lead_words(sql, 2);
        Some(match (w[0].as_str(), w[1].as_str()) {
            ("create", "extension") => self.create_extension(sql),
            ("drop", "extension") => self.drop_extension(sql),
            ("alter", "extension") => Ok(Command::from("ALTER EXTENSION")),
            ("create", "collation") => self.create_collation(sql),
            ("drop", "collation") => self.drop_collation(sql),
            ("alter", "collation") => Ok(Command::from("ALTER COLLATION")),
            (_, "table") => Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "partitioned tables are not supported"
            )),
            ("create", "materialized") => self.create_matview(sql),
            ("refresh", _) => self.refresh_matview(sql),
            ("drop", "materialized") => self.drop_matview(sql),
            ("alter", "materialized") => Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER MATERIALIZED VIEW is not supported"
            )),
            _ => Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "foreign-data wrappers are not supported"
            )),
        })
    }

    /// CREATE MATERIALIZED VIEW [IF NOT EXISTS] name [(cols)] AS query [WITH [NO] DATA].
    fn create_matview(&mut self, sql: &str) -> Result<Command> {
        let toks = code_tokens(sql);
        let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
        let mut i = 3;
        let if_not_exists = is(i, "if") && is(i + 1, "not") && is(i + 2, "exists");
        if if_not_exists {
            i += 3;
        }
        let Some(name_tok) = toks.get(i) else {
            return Err(errf!(
                SqlState::SYNTAX_ERROR,
                "syntax error in CREATE MATERIALIZED VIEW"
            ));
        };
        let mut name_end = name_tok.span.end;
        let mut j = i + 1;
        while toks.get(j).is_some_and(|t| t.is_punct(sql, ".")) && toks.get(j + 1).is_some() {
            name_end = toks[j + 1].span.end;
            j += 2;
        }
        let reference = &sql[name_tok.span.start..name_end];
        let Some(as_at) = (j..toks.len()).find(|&k| toks[k].depth == 0 && toks[k].is_word(sql, "as")) else {
            return Err(errf!(
                SqlState::SYNTAX_ERROR,
                "syntax error in CREATE MATERIALIZED VIEW"
            ));
        };
        let column_list = (j < as_at).then(|| &sql[toks[j].span.start..toks[as_at - 1].span.end]);
        let mut query_end = sql.trim_end().trim_end_matches(';').trim_end().len();
        let n = toks.len();
        let mut with_data = true;
        if n >= 3 && is(n - 2, "with") && is(n - 1, "data") {
            query_end = toks[n - 2].span.start;
        } else if n >= 4 && is(n - 3, "with") && is(n - 2, "no") && is(n - 1, "data") {
            query_end = toks[n - 3].span.start;
            with_data = false;
        }
        let query = sql[toks[as_at].span.end..query_end].trim();
        let stored = self.stored_name(&unquote_reference(reference));
        if self.relation_exists(&stored)? {
            if if_not_exists {
                self.c.send_notice(&format!(
                    "relation {:?} already exists, skipping",
                    quoted(&unquote_reference(reference))
                ))?;
                return Ok(Command::from("CREATE MATERIALIZED VIEW"));
            }
            return Err(errf!(
                SqlState::DUPLICATE_TABLE,
                "relation {:?} already exists",
                quoted(&unquote_reference(reference))
            ));
        }
        let view = quote_ident(&(MATVIEW_PREFIX.to_string() + &stored));
        let view_sql = ["CREATE VIEW ", &view, column_list.map_or("", |c| c), " AS ", query].concat();
        let view_sql = self.rewrite_for_exec(&view_sql, &[])?;
        self.exec("SAVEPOINT _mv", &[])?;
        let made = (|| {
            self.exec(&view_sql, &[])?;
            // The table takes the view's columns with the types they are declared with.
            let cols = self.exec(
                &[
                    "SELECT name, type FROM pragma_table_info(",
                    &quote_literal(&(MATVIEW_PREFIX.to_string() + &stored)),
                    ")",
                ]
                .concat(),
                &[],
            )?;
            let defs: Vec<String> = cols
                .rows
                .iter()
                .map(|r| {
                    let name = r[0].to_text_lossy();
                    let typ = match &r[1] {
                        Value::Text(t) if !t.is_empty() => t.clone(),
                        _ => String::new(),
                    };
                    [quote_ident(&name), " ".into(), typ].concat().trim_end().to_string()
                })
                .collect();
            self.exec(
                &["CREATE TABLE ", &quote_ident(&stored), " (", &defs.join(", "), ")"].concat(),
                &[],
            )?;
            if !with_data {
                return Ok(0);
            }
            let rs = self.exec(
                &["INSERT INTO ", &quote_ident(&stored), " SELECT * FROM ", &view].concat(),
                &[],
            )?;
            Ok::<_, crate::error::Error>(rs.rows_affected)
        })();
        let rows = match made {
            Ok(n) => n,
            Err(e) => {
                let _ = self.exec("ROLLBACK TO _mv", &[]);
                let _ = self.exec("RELEASE _mv", &[]);
                return Err(e);
            }
        };
        self.exec("RELEASE _mv", &[])?;
        if with_data {
            return Ok(Command::from(format!("SELECT {rows}").as_str()));
        }
        Ok(Command::from("CREATE MATERIALIZED VIEW"))
    }

    /// REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA].
    fn refresh_matview(&mut self, sql: &str) -> Result<Command> {
        let toks = code_tokens(sql);
        let mut i = 3;
        if toks.get(i).is_some_and(|t| t.is_word(sql, "concurrently")) {
            i += 1;
        }
        let Some(t) = toks.get(i) else {
            return Err(errf!(
                SqlState::SYNTAX_ERROR,
                "syntax error in REFRESH MATERIALIZED VIEW"
            ));
        };
        let mut end = t.span.end;
        let mut j = i + 1;
        while toks.get(j).is_some_and(|t| t.is_punct(sql, ".")) && toks.get(j + 1).is_some() {
            end = toks[j + 1].span.end;
            j += 2;
        }
        let reference = unquote_reference(&sql[t.span.start..end]);
        let no_data = toks.get(j + 1).is_some_and(|t| t.is_word(sql, "no"));
        let stored = self.stored_name(&reference);
        if !self.is_matview(&stored)? {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation {:?} does not exist",
                quoted(&reference)
            ));
        }
        let view = quote_ident(&(MATVIEW_PREFIX.to_string() + &stored));
        self.exec("SAVEPOINT _mv", &[])?;
        let mut r = self.exec(&["DELETE FROM ", &quote_ident(&stored)].concat(), &[]);
        if r.is_ok() && !no_data {
            r = self.exec(
                &["INSERT INTO ", &quote_ident(&stored), " SELECT * FROM ", &view].concat(),
                &[],
            );
        }
        if let Err(e) = r {
            let _ = self.exec("ROLLBACK TO _mv", &[]);
            let _ = self.exec("RELEASE _mv", &[]);
            return Err(e);
        }
        self.exec("RELEASE _mv", &[])?;
        Ok(Command::from("REFRESH MATERIALIZED VIEW"))
    }

    /// DROP MATERIALIZED VIEW [IF EXISTS] name [, ...] [CASCADE | RESTRICT].
    fn drop_matview(&mut self, sql: &str) -> Result<Command> {
        let toks = code_tokens(sql);
        let mut i = 3;
        let if_exists = toks.get(i).is_some_and(|t| t.is_word(sql, "if"))
            && toks.get(i + 1).is_some_and(|t| t.is_word(sql, "exists"));
        if if_exists {
            i += 2;
        }
        let names_end = toks
            .iter()
            .rposition(|t| !(t.is_word(sql, "cascade") || t.is_word(sql, "restrict") || t.is_punct(sql, ";")))
            .map_or(sql.len(), |k| toks[k].span.end);
        let start = toks.get(i).map_or(sql.len(), |t| t.span.start);
        let names = crate::ddl::sequences::split_top_level(sql.get(start..names_end).unwrap_or(""));
        // All or none, as one statement.
        self.exec("SAVEPOINT _mv", &[])?;
        let r = self.drop_matviews(&names, if_exists);
        if r.is_err() {
            let _ = self.exec("ROLLBACK TO _mv", &[]);
        }
        let _ = self.exec("RELEASE _mv", &[]);
        r.map(|()| Command::from("DROP MATERIALIZED VIEW"))
    }

    fn drop_matviews(&mut self, names: &[String], if_exists: bool) -> Result<()> {
        for reference in names {
            let reference = unquote_reference(reference.trim());
            let stored = self.stored_name(&reference);
            if !self.is_matview(&stored)? {
                if self.relation_exists(&stored)? {
                    return Err(errf!(
                        SqlState::WRONG_OBJECT_TYPE,
                        "{:?} is not a materialized view",
                        quoted(&reference)
                    ));
                }
                if if_exists {
                    self.c.send_notice(&format!(
                        "materialized view {:?} does not exist, skipping",
                        quoted(&reference)
                    ))?;
                    continue;
                }
                return Err(errf!(
                    SqlState::UNDEFINED_TABLE,
                    "materialized view {:?} does not exist",
                    quoted(&reference)
                ));
            }
            self.exec(&["DROP TABLE ", &quote_ident(&stored)].concat(), &[])?;
            self.exec(
                &[
                    "DROP VIEW IF EXISTS ",
                    &quote_ident(&(MATVIEW_PREFIX.to_string() + &stored)),
                ]
                .concat(),
                &[],
            )?;
        }
        Ok(())
    }

    /// CREATE EXTENSION [IF NOT EXISTS] name [WITH] [SCHEMA s] [VERSION v] [CASCADE].
    fn create_extension(&mut self, sql: &str) -> Result<Command> {
        let toks = code_tokens(sql);
        let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
        let mut i = 2;
        let if_not_exists = is(i, "if") && is(i + 1, "not") && is(i + 2, "exists");
        if if_not_exists {
            i += 3;
        }
        let Some(t) = toks.get(i) else {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error at end of input"));
        };
        let name = extension_name(&sql[t.span.clone()]);
        let Some(&(_, version)) = EXTENSIONS.iter().find(|(n, _)| *n == name) else {
            return Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "extension {:?} is not available",
                quoted(&name)
            ));
        };
        if name == "plpgsql" || self.extension_exists(&name)? {
            if if_not_exists {
                self.c
                    .send_notice(&format!("extension {:?} already exists, skipping", quoted(&name)))?;
                return Ok(Command::from("CREATE EXTENSION"));
            }
            return Err(errf!(
                SqlState::DUPLICATE_OBJECT,
                "extension {:?} already exists",
                quoted(&name)
            ));
        }
        self.exec(
            &[
                "INSERT INTO _overlite_extensions (extname, extversion) VALUES (",
                &quote_literal(&name),
                ", ",
                &quote_literal(version),
                ")",
            ]
            .concat(),
            &[],
        )?;
        Ok(Command::from("CREATE EXTENSION"))
    }

    /// DROP EXTENSION [IF EXISTS] name [, ...] [CASCADE | RESTRICT].
    fn drop_extension(&mut self, sql: &str) -> Result<Command> {
        let toks = code_tokens(sql);
        let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
        let mut i = 2;
        let if_exists = is(i, "if") && is(i + 1, "exists");
        if if_exists {
            i += 2;
        }
        let names: Vec<String> = toks[i.min(toks.len())..]
            .iter()
            .filter(|t| !t.is_punct(sql, ",") && !t.is_punct(sql, ";"))
            .filter(|t| !t.is_word(sql, "cascade") && !t.is_word(sql, "restrict"))
            .map(|t| extension_name(&sql[t.span.clone()]))
            .collect();
        for name in &names {
            if !self.extension_exists(name)? && name != "plpgsql" {
                if if_exists {
                    self.c
                        .send_notice(&format!("extension {:?} does not exist, skipping", quoted(name)))?;
                    continue;
                }
                return Err(errf!(
                    SqlState::UNDEFINED_OBJECT,
                    "extension {:?} does not exist",
                    quoted(name)
                ));
            }
        }
        for name in &names {
            self.exec(
                &[
                    "DELETE FROM _overlite_extensions WHERE extname = ",
                    &quote_literal(name),
                ]
                .concat(),
                &[],
            )?;
        }
        Ok(Command::from("DROP EXTENSION"))
    }

    /// CREATE COLLATION [IF NOT EXISTS] name (provider = p, locale = l, lc_collate = l,
    /// deterministic = b) | FROM existing.
    fn create_collation(&mut self, sql: &str) -> Result<Command> {
        let toks = code_tokens(sql);
        let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
        let mut i = 2;
        let if_not_exists = is(i, "if") && is(i + 1, "not") && is(i + 2, "exists");
        if if_not_exists {
            i += 3;
        }
        let Some(t) = toks.get(i) else {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error at end of input"));
        };
        let name = extension_name(&sql[t.span.clone()]);
        let (mut provider, mut locale, mut deterministic) = ("libc".to_string(), String::new(), true);
        if is(i + 1, "from") {
            let from = toks
                .get(i + 2)
                .map(|t| extension_name(&sql[t.span.clone()]))
                .unwrap_or_default();
            if !overlite_engine::collation_known(&from)
                && !["c", "posix", "default", "ucs_basic"].contains(&from.to_ascii_lowercase().as_str())
            {
                return Err(errf!(
                    SqlState::UNDEFINED_OBJECT,
                    "collation {:?} for encoding \"UTF8\" does not exist",
                    quoted(&from)
                ));
            }
            locale = from;
            provider = "icu".into();
        } else {
            let open = sql
                .find('(')
                .ok_or_else(|| errf!(SqlState::SYNTAX_ERROR, "syntax error in CREATE COLLATION"))?;
            let close = sql
                .rfind(')')
                .filter(|&c| c > open)
                .ok_or_else(|| errf!(SqlState::SYNTAX_ERROR, "syntax error in CREATE COLLATION"))?;
            for opt in sql[open + 1..close].split(',') {
                let Some((k, v)) = opt.split_once('=') else { continue };
                let v = v.trim().trim_matches('\'').trim_matches('"').to_string();
                match k.trim().to_ascii_lowercase().as_str() {
                    "provider" => provider = v.to_ascii_lowercase(),
                    "locale" | "lc_collate" => locale = v,
                    "deterministic" => {
                        deterministic = !matches!(v.to_ascii_lowercase().as_str(), "false" | "off" | "0" | "no");
                    }
                    _ => {}
                }
            }
        }
        if !matches!(provider.as_str(), "icu" | "libc" | "builtin") {
            return Err(errf!(
                SqlState::INVALID_OBJECT_DEFINITION,
                "unrecognized collation provider: {provider}"
            ));
        }
        if !deterministic && provider != "icu" {
            return Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "nondeterministic collations not supported with this provider"
            ));
        }
        let exists = !self
            .exec(
                &["SELECT 1 FROM pg_collation WHERE collname = ", &quote_literal(&name)].concat(),
                &[],
            )?
            .rows
            .is_empty();
        if exists {
            if if_not_exists {
                self.c
                    .send_notice(&format!("collation {:?} already exists, skipping", quoted(&name)))?;
                return Ok(Command::from("CREATE COLLATION"));
            }
            return Err(errf!(
                SqlState::DUPLICATE_OBJECT,
                "collation {:?} already exists",
                quoted(&name)
            ));
        }
        let lower = locale.to_ascii_lowercase();
        let spec = if lower == "c" || lower == "posix" || lower.is_empty() {
            None
        } else {
            Some(overlite_engine::spec_of_locale(&locale, deterministic))
        };
        self.exec(
            &[
                "INSERT INTO _overlite_collations (collname, provider, locale, deterministic) VALUES (",
                &quote_literal(&name),
                ", ",
                &quote_literal(&provider),
                ", ",
                &quote_literal(&locale),
                ", ",
                if deterministic { "1" } else { "0" },
                ")",
            ]
            .concat(),
            &[],
        )?;
        // C and POSIX are byte order, SQLite's own: COLLATE drops a name it does not know.
        if let Some(spec) = spec {
            overlite_engine::register_collation(&name, spec);
        }
        Ok(Command::from("CREATE COLLATION"))
    }

    /// DROP COLLATION [IF EXISTS] name [CASCADE | RESTRICT].
    fn drop_collation(&mut self, sql: &str) -> Result<Command> {
        let toks = code_tokens(sql);
        let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
        let mut i = 2;
        let if_exists = is(i, "if") && is(i + 1, "exists");
        if if_exists {
            i += 2;
        }
        let Some(t) = toks.get(i) else {
            return Err(errf!(SqlState::SYNTAX_ERROR, "syntax error at end of input"));
        };
        let name = extension_name(&sql[t.span.clone()]);
        let rs = self.exec(
            &[
                "DELETE FROM _overlite_collations WHERE collname = ",
                &quote_literal(&name),
            ]
            .concat(),
            &[],
        )?;
        if rs.rows_affected == 0 {
            if if_exists {
                self.c
                    .send_notice(&format!("collation {:?} does not exist, skipping", quoted(&name)))?;
                return Ok(Command::from("DROP COLLATION"));
            }
            return Err(errf!(
                SqlState::UNDEFINED_OBJECT,
                "collation {:?} for encoding \"UTF8\" does not exist",
                quoted(&name)
            ));
        }
        overlite_engine::forget_collation(&name);
        Ok(Command::from("DROP COLLATION"))
    }

    fn extension_exists(&mut self, name: &str) -> Result<bool> {
        let rs = self.exec(
            &[
                "SELECT 1 FROM _overlite_extensions WHERE extname = ",
                &quote_literal(name),
            ]
            .concat(),
            &[],
        )?;
        Ok(!rs.rows.is_empty())
    }

    /// Whether a table, view or index of that stored name exists.
    fn relation_exists(&mut self, stored: &str) -> Result<bool> {
        let rs = self.exec(
            &[
                "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view', 'index') AND lower(name) = lower(",
                &quote_literal(stored),
                ")",
            ]
            .concat(),
            &[],
        )?;
        Ok(!rs.rows.is_empty())
    }

    /// Whether stored names a materialized view.
    fn is_matview(&mut self, stored: &str) -> Result<bool> {
        let rs = self.exec(
            &[
                "SELECT 1 FROM sqlite_master WHERE type = 'view' AND lower(name) = lower(",
                &quote_literal(&(MATVIEW_PREFIX.to_string() + stored)),
                ")",
            ]
            .concat(),
            &[],
        )?;
        Ok(!rs.rows.is_empty())
    }
}

/// The extensions this server provides, with the version Postgres 17 ships: what
/// each adds is built in (pgcrypto's digest and gen_random_uuid, uuid-ossp's
/// uuid_generate_v4, hstore, citext, unaccent, pg_trgm), or is an index operator class
/// or method the btree under every index stands in for (btree_gin, btree_gist, bloom).
const EXTENSIONS: &[(&str, &str)] = &[
    ("plpgsql", "1.0"),
    ("pgcrypto", "1.3"),
    ("uuid-ossp", "1.1"),
    ("hstore", "1.8"),
    ("citext", "1.6"),
    ("unaccent", "1.1"),
    ("pg_trgm", "1.6"),
    ("btree_gin", "1.3"),
    ("btree_gist", "1.7"),
    ("bloom", "1.0"),
];

/// An extension's name as written: quoted whole, else in lower case.
fn extension_name(word: &str) -> String {
    if word.starts_with('"') {
        unquote_ident(word)
    } else {
        word.to_ascii_lowercase()
    }
}

/// Whether a CREATE TABLE is a partitioned table or a partition: PARTITION BY after
/// its column list, or PARTITION OF. SQLite has no partitions, and a table that
/// took the clause without routing rows would answer queries wrongly.
fn partitioned(sql: &str) -> bool {
    let toks = code_tokens(sql);
    toks.windows(2).any(|w| {
        w[0].depth == 0 && w[0].is_word(sql, "partition") && (w[1].is_word(sql, "by") || w[1].is_word(sql, "of"))
    })
}

/// A reference with each part unquoted: `"s"."v"` is s.v, `v` is v.
fn unquote_reference(reference: &str) -> String {
    let toks = code_tokens(reference);
    let parts: Vec<String> = toks
        .iter()
        .filter(|t| !t.is_punct(reference, "."))
        .map(|t| {
            let p = &reference[t.span.clone()];
            if p.starts_with('"') {
                unquote_ident(p)
            } else {
                p.to_ascii_lowercase()
            }
        })
        .collect();
    if parts.len() == 2 && eq_fold(&parts[0], "public") {
        return parts[1].clone();
    }
    parts.join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_statements() {
        assert!(is_object_ddl("CREATE MATERIALIZED VIEW v AS SELECT 1"));
        assert!(is_object_ddl("refresh materialized view concurrently v"));
        assert!(is_object_ddl("CREATE SERVER s FOREIGN DATA WRAPPER file_fdw"));
        assert!(is_object_ddl("CREATE FOREIGN TABLE f (a int) SERVER s"));
        assert!(!is_object_ddl("CREATE VIEW v AS SELECT 1"));
        assert!(!is_object_ddl("CREATE TABLE server (a int)"));
        assert!(is_object_ddl("CREATE TABLE p (a text) PARTITION BY LIST (left(a, 1))"));
        assert!(is_object_ddl("CREATE TABLE c PARTITION OF p FOR VALUES IN ('A')"));
        assert!(!is_object_ddl("CREATE TABLE t (partition int)"));
        assert_eq!(unquote_reference(r#""public"."My""#), "My");
        assert_eq!(unquote_reference("S.v"), "s.v");
    }
}
