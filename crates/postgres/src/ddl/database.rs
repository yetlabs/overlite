//! CREATE / DROP / ALTER DATABASE act on the server's set of databases, not on the
//! one the connection is using, so they are handled here rather than reaching the
//! engine. A server holding a single file has no set to act on and says so.

use crate::ddl::tree;
use crate::error::{Error, Result};
use crate::session::Session;
use crate::session::utility::{first_word_upper, second_word_upper};
use overlite_core::text::quote_literal;
use overlite_core::{Command, SqlState};
use pg_query::NodeEnum;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether sql is a CREATE/DROP/ALTER DATABASE.
pub(crate) fn is_database_ddl(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::database_ddl).unwrap_or_else(|| is_database_ddl_text(sql))
}

/// `is_database_ddl` by the text, for what Postgres' grammar does not read.
pub(crate) fn is_database_ddl_text(sql: &str) -> bool {
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

/// What a database statement's node asks for.
fn database_plan(n: &NodeEnum) -> Option<DatabaseDdl> {
    use pg_query::protobuf::ObjectType;
    Some(match n {
        NodeEnum::CreatedbStmt(c) => DatabaseDdl::Create(c.dbname.clone()),
        NodeEnum::DropdbStmt(d) => DatabaseDdl::Drop {
            name: d.dbname.clone(),
            if_exists: d.missing_ok,
        },
        NodeEnum::RenameStmt(r) if tree::object(r.rename_type) == ObjectType::ObjectDatabase => DatabaseDdl::Rename {
            name: r.subname.clone(),
            to: r.newname.clone(),
        },
        NodeEnum::AlterDatabaseStmt(a) => DatabaseDdl::Alter(a.dbname.clone()),
        NodeEnum::AlterDatabaseSetStmt(a) => DatabaseDdl::Alter(a.dbname.clone()),
        NodeEnum::AlterDatabaseRefreshCollStmt(a) => DatabaseDdl::Alter(a.dbname.clone()),
        NodeEnum::AlterOwnerStmt(a) if tree::object(a.object_type) == ObjectType::ObjectDatabase => {
            DatabaseDdl::Alter(tree::string(a.object.as_deref()?)?.to_string())
        }
        _ => return None,
    })
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
        let r = tree::statement(sql, |n| database_plan(n).ok_or_else(|| tree::parse_failure(sql)))
            .and_then(|ddl| self.run_database_ddl(ddl));
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
            assert_eq!(tree::read(sql, database_plan).ok_or(()), want, "{sql}");
        }
    }
}
