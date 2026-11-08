//! CREATE / DROP / ALTER DATABASE act on the server's set of databases, not on the
//! one the connection is using, so they are handled here rather than reaching the
//! engine. A server holding a single file has no set to act on and says so.

use crate::ddl::words::{Word, ddl_words, syntax_error_at};
use crate::error::{Error, Result};
use crate::session::Session;
use crate::session::utility::{first_word_upper, second_word_upper};
use overlite_core::text::quote_literal;
use overlite_core::{Command, SqlState};
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether sql is a CREATE/DROP/ALTER DATABASE.
pub(crate) fn is_database_ddl(sql: &str) -> bool {
    let w = first_word_upper(sql);
    (w == "CREATE" || w == "DROP" || w == "ALTER") && second_word_upper(sql) == "DATABASE"
}

/// What a database statement asks for. Options after the name (ENCODING, OWNER, WITH
/// (FORCE) ...) are accepted and ignored: there is nothing here to vary.
#[derive(Debug, PartialEq, Eq)]
enum DatabaseDdl {
    Create(String),
    Drop {
        name: String,
        if_exists: bool,
    },
    Rename {
        name: String,
        to: String,
    },
    /// OWNER TO, SET, RESET, a connection limit: nothing to keep.
    Alter(String),
}

fn name_at(words: &[Word], at: usize) -> Result<String> {
    match words.get(at).and_then(Word::name) {
        Some(name) => Ok(name.to_string()),
        None => Err(syntax_error_at(words.get(at))),
    }
}

/// Reads a statement is_database_ddl took, whose name Postgres' grammar puts in one
/// place, so an option or a quoted name is never taken for it.
fn parse_database_ddl(sql: &str) -> Result<DatabaseDdl> {
    let mut words = ddl_words(sql)?;
    if words.last() == Some(&Word::Punct(';')) {
        words.pop();
    }
    match words.first() {
        Some(w) if w.is("create") => {
            // The grammar has no IF NOT EXISTS here, and `if` is a good name.
            if words.get(3).is_some_and(|w| w.is("not")) {
                return Err(syntax_error_at(words.get(3)));
            }
            Ok(DatabaseDdl::Create(name_at(&words, 2)?))
        }
        Some(w) if w.is("drop") => {
            let if_exists = words.get(2).is_some_and(|w| w.is("if")) && words.get(3).is_some_and(|w| w.is("exists"));
            let at = if if_exists { 4 } else { 2 };
            Ok(DatabaseDdl::Drop {
                name: name_at(&words, at)?,
                if_exists,
            })
        }
        _ => {
            let name = name_at(&words, 2)?;
            if words.get(3).is_some_and(|w| w.is("rename")) {
                if !words.get(4).is_some_and(|w| w.is("to")) {
                    return Err(syntax_error_at(words.get(4)));
                }
                let to = name_at(&words, 5)?;
                return Ok(DatabaseDdl::Rename { name, to });
            }
            if words.len() < 4 {
                return Err(syntax_error_at(None));
            }
            Ok(DatabaseDdl::Alter(name))
        }
    }
}

/// Bumped when the set of databases changes, for the sessions already open.
static DATABASE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Which set of databases the server is at.
pub(crate) fn database_generation() -> u64 {
    DATABASE_GENERATION.load(Ordering::Acquire)
}

impl Session {
    /// Runs a CREATE/DROP/ALTER DATABASE, returning the command tag.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_database_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_database_ddl(sql) {
            return None;
        }
        let r = parse_database_ddl(sql).and_then(|ddl| self.run_database_ddl(ddl));
        if r.is_ok() {
            DATABASE_GENERATION.fetch_add(1, Ordering::AcqRel);
        }
        Some(r)
    }

    fn run_database_ddl(&mut self, ddl: DatabaseDdl) -> Result<Command> {
        let refused_in_tx = |verb: &str| {
            Error::with_state(
                SqlState::ACTIVE_SQL_TRANSACTION,
                format!("{verb} DATABASE cannot run inside a transaction block"),
            )
        };
        match ddl {
            DatabaseDdl::Create(_) if self.in_tx() => Err(refused_in_tx("CREATE")),
            DatabaseDdl::Drop { .. } if self.in_tx() => Err(refused_in_tx("DROP")),
            DatabaseDdl::Rename { .. } if self.in_tx() => Err(Error::with_state(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER DATABASE RENAME moves a file and cannot be rolled back, so it cannot run inside a transaction block",
            )),
            DatabaseDdl::Create(name) => {
                self.cluster.create_database(&name)?;
                Ok(Command::from("CREATE DATABASE"))
            }
            DatabaseDdl::Drop { name, if_exists } => {
                if name == self.database {
                    return Err(Error::with_state(
                        SqlState::OBJECT_IN_USE,
                        "cannot drop the currently open database",
                    ));
                }
                match self.cluster.drop_database(&name) {
                    Err(e) if if_exists && e.sql_state() == SqlState::INVALID_CATALOG_NAME => {
                        self.c
                            .send_notice(&format!("database \"{name}\" does not exist, skipping"))?;
                    }
                    r => r?,
                }
                Ok(Command::from("DROP DATABASE"))
            }
            DatabaseDdl::Rename { name, to } => {
                if name == self.database {
                    return Err(Error::with_state(
                        SqlState::OBJECT_IN_USE,
                        "current database cannot be renamed",
                    ));
                }
                self.cluster.rename_database(&name, &to)?;
                Ok(Command::from("ALTER DATABASE"))
            }
            DatabaseDdl::Alter(name) => {
                if !self.database_list()?.contains(&name) {
                    return Err(Error::with_state(
                        SqlState::INVALID_CATALOG_NAME,
                        format!("database \"{name}\" does not exist"),
                    ));
                }
                Ok(Command::from("ALTER DATABASE"))
            }
        }
    }

    /// Answers a query against pg_database from the cluster, so a client sees the
    /// databases this server actually holds.
    fn database_list(&self) -> Result<Vec<String>> {
        Ok(self.cluster.databases()?)
    }

    /// Rebuilds this connection's pg_database so it lists the databases the
    /// server actually holds.
    ///
    /// The engine builds the view knowing only its own database, because a SQLite
    /// file has no idea it sits beside others. The cluster is what knows, and it
    /// lives out here, so the view is replaced per connection, and again whenever
    /// a CREATE or DROP changes the set.
    pub(crate) fn refresh_database_list(&mut self) -> Result<()> {
        let names = match self.database_list() {
            Ok(names) if !names.is_empty() => names,
            _ => return Ok(()), // leave the engine's own single-row view in place
        };

        let rows: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                format!(
                    concat!(
                        "SELECT {} AS oid, {} AS datname, 10 AS datdba, 6 AS encoding,",
                        " 'c' AS datlocprovider, 'C' AS datcollate, 'C' AS datctype,",
                        " NULL AS daticulocale, NULL AS daticurules, NULL AS datcollversion,",
                        " 0 AS datistemplate, 1 AS datallowconn, -1 AS datconnlimit,",
                        " 0 AS dattablespace, 0 AS datfrozenxid, 0 AS datminmxid,",
                        " NULL AS datacl, 1262 AS tableoid"
                    ),
                    i + 1,
                    quote_literal(name)
                )
            })
            .collect();
        if self.exec("DROP VIEW IF EXISTS pg_database", &[]).is_err() {
            return Ok(());
        }
        let q = format!("CREATE TEMP VIEW pg_database AS {}", rows.join(" UNION ALL "));
        if self.exec(&q, &[]).is_err() {
            return Ok(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_is_read_where_the_grammar_puts_it() {
        let drop = |name: &str, if_exists| {
            Ok::<_, ()>(DatabaseDdl::Drop {
                name: name.to_string(),
                if_exists,
            })
        };
        for (sql, want) in [
            (r#"DROP DATABASE "a b""#, drop("a b", false)),
            (r#"DROP DATABASE IF EXISTS "a b";"#, drop("a b", true)),
            (r#"DROP DATABASE "IF EXISTS""#, drop("IF EXISTS", false)),
            (r#"DROP DATABASE "say ""hi""""#, drop(r#"say "hi""#, false)),
            ("DROP DATABASE Shop WITH (FORCE)", drop("shop", false)),
            (
                r#"CREATE DATABASE "already exists" WITH OWNER x"#,
                Ok(DatabaseDdl::Create("already exists".to_string())),
            ),
            (
                r#"ALTER DATABASE "a b" RENAME TO "c d""#,
                Ok(DatabaseDdl::Rename {
                    name: "a b".to_string(),
                    to: "c d".to_string(),
                }),
            ),
            (
                r#"ALTER DATABASE "a b" OWNER TO x"#,
                Ok(DatabaseDdl::Alter("a b".to_string())),
            ),
        ] {
            assert_eq!(parse_database_ddl(sql).map_err(|_| ()), want, "{sql}");
        }
    }
}
