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

use crate::ddl::tree;
use crate::error::{Result, errf};
use crate::session::Session;
use overlite_core::text::{eq_fold, quote_ident, quote_literal, quoted};
use overlite_core::{Command, SqlState, Value};
use pg_query::NodeEnum;

/// The view that keeps a materialized view's query.
pub(crate) const MATVIEW_PREFIX: &str = "_overlite_mv_";

/// A statement of this module, read from its node.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ObjectPlan {
    CreateExtension {
        name: String,
        if_not_exists: bool,
    },
    DropExtension {
        names: Vec<String>,
        if_exists: bool,
    },
    AlterExtension,
    CreateCollation {
        name: String,
        if_not_exists: bool,
        /// FROM an existing collation, or the options' (key, value).
        from: Option<String>,
        options: Vec<(String, String)>,
    },
    DropCollation {
        name: String,
        if_exists: bool,
    },
    AlterCollation,
    Partitioned,
    CreateMatview {
        /// The relation as a reference, and as a message names it.
        reference: String,
        shown: String,
        columns: Vec<String>,
        if_not_exists: bool,
        with_data: bool,
    },
    RefreshMatview {
        reference: String,
        shown: String,
        with_data: bool,
    },
    DropMatview {
        names: Vec<(String, String)>,
        if_exists: bool,
    },
    AlterMatview,
    ForeignData,
}

/// A relation as a reference and as a message names it: `s.v`, or `v` in public.
fn relation(r: &pg_query::protobuf::RangeVar) -> (String, String) {
    let shown = if r.schemaname.is_empty() || eq_fold(&r.schemaname, "public") {
        r.relname.clone()
    } else {
        [r.schemaname.as_str(), ".", &r.relname].concat()
    };
    (tree::reference(r), shown)
}

/// The same of a name's parts (a DROP's objects).
fn relation_of(parts: &[String]) -> (String, String) {
    let shown = match parts {
        [s, n] if eq_fold(s, "public") => n.clone(),
        _ => parts.join("."),
    };
    (tree::reference_of(parts), shown)
}

fn object_plan(n: &NodeEnum) -> Option<ObjectPlan> {
    use pg_query::protobuf::ObjectType as O;
    let lists = |objects: &[pg_query::protobuf::Node]| -> Vec<Vec<String>> {
        objects
            .iter()
            .map(|o| match &o.node {
                Some(NodeEnum::List(l)) => tree::strings(&l.items),
                Some(NodeEnum::String(s)) => vec![s.sval.clone()],
                _ => Vec::new(),
            })
            .collect()
    };
    let foreign = |t: i32| {
        matches!(
            tree::object(t),
            O::ObjectForeignServer | O::ObjectForeignTable | O::ObjectFdw
        )
    };
    Some(match n {
        NodeEnum::CreateExtensionStmt(c) => ObjectPlan::CreateExtension {
            name: c.extname.clone(),
            if_not_exists: c.if_not_exists,
        },
        NodeEnum::DropStmt(d) if tree::object(d.remove_type) == O::ObjectExtension => ObjectPlan::DropExtension {
            names: lists(&d.objects).into_iter().filter_map(|mut l| l.pop()).collect(),
            if_exists: d.missing_ok,
        },
        NodeEnum::AlterExtensionStmt(_) | NodeEnum::AlterExtensionContentsStmt(_) => ObjectPlan::AlterExtension,
        NodeEnum::DefineStmt(d) if tree::object(d.kind) == O::ObjectCollation => {
            let mut from = None;
            let mut options = Vec::new();
            for e in tree::defs(&d.definition) {
                let v = tree::def_text(e).unwrap_or_default();
                if e.defname == "from" {
                    from = Some(v);
                } else {
                    options.push((e.defname.to_ascii_lowercase(), v));
                }
            }
            ObjectPlan::CreateCollation {
                name: tree::strings(&d.defnames).pop()?,
                if_not_exists: d.if_not_exists,
                from,
                options,
            }
        }
        NodeEnum::DropStmt(d) if tree::object(d.remove_type) == O::ObjectCollation => ObjectPlan::DropCollation {
            name: lists(&d.objects).first()?.last()?.clone(),
            if_exists: d.missing_ok,
        },
        NodeEnum::AlterCollationStmt(_) => ObjectPlan::AlterCollation,
        NodeEnum::RenameStmt(r) if tree::object(r.rename_type) == O::ObjectCollation => ObjectPlan::AlterCollation,
        NodeEnum::AlterOwnerStmt(a) if tree::object(a.object_type) == O::ObjectCollation => ObjectPlan::AlterCollation,
        NodeEnum::AlterObjectSchemaStmt(a) if tree::object(a.object_type) == O::ObjectCollation => {
            ObjectPlan::AlterCollation
        }
        NodeEnum::AlterOwnerStmt(a) if tree::object(a.object_type) == O::ObjectExtension => ObjectPlan::AlterExtension,
        NodeEnum::CreateStmt(c) if c.partspec.is_some() || c.partbound.is_some() => ObjectPlan::Partitioned,
        NodeEnum::CreateTableAsStmt(c) if tree::object(c.objtype) == O::ObjectMatview => {
            let into = c.into.as_ref()?;
            let (reference, shown) = relation(into.rel.as_ref()?);
            ObjectPlan::CreateMatview {
                reference,
                shown,
                columns: tree::strings(&into.col_names),
                if_not_exists: c.if_not_exists,
                with_data: !into.skip_data,
            }
        }
        NodeEnum::RefreshMatViewStmt(r) => {
            let (reference, shown) = relation(r.relation.as_ref()?);
            ObjectPlan::RefreshMatview {
                reference,
                shown,
                with_data: !r.skip_data,
            }
        }
        NodeEnum::DropStmt(d) if tree::object(d.remove_type) == O::ObjectMatview => ObjectPlan::DropMatview {
            names: lists(&d.objects).iter().map(|l| relation_of(l)).collect(),
            if_exists: d.missing_ok,
        },
        NodeEnum::AlterTableStmt(a) if tree::object(a.objtype) == O::ObjectMatview => ObjectPlan::AlterMatview,
        NodeEnum::RenameStmt(r) if tree::object(r.rename_type) == O::ObjectMatview => ObjectPlan::AlterMatview,
        NodeEnum::AlterObjectSchemaStmt(a) if tree::object(a.object_type) == O::ObjectMatview => {
            ObjectPlan::AlterMatview
        }
        NodeEnum::AlterOwnerStmt(a) if tree::object(a.object_type) == O::ObjectMatview => ObjectPlan::AlterMatview,
        NodeEnum::CreateFdwStmt(_)
        | NodeEnum::AlterFdwStmt(_)
        | NodeEnum::CreateForeignServerStmt(_)
        | NodeEnum::AlterForeignServerStmt(_)
        | NodeEnum::CreateForeignTableStmt(_)
        | NodeEnum::CreateUserMappingStmt(_)
        | NodeEnum::AlterUserMappingStmt(_)
        | NodeEnum::DropUserMappingStmt(_)
        | NodeEnum::ImportForeignSchemaStmt(_) => ObjectPlan::ForeignData,
        NodeEnum::DropStmt(d) if foreign(d.remove_type) => ObjectPlan::ForeignData,
        NodeEnum::AlterTableStmt(a) if foreign(a.objtype) => ObjectPlan::ForeignData,
        NodeEnum::RenameStmt(r) if foreign(r.rename_type) => ObjectPlan::ForeignData,
        NodeEnum::AlterOwnerStmt(a) if foreign(a.object_type) => ObjectPlan::ForeignData,
        _ => return None,
    })
}

/// Whether try_object_ddl takes sql. Every statement comes through here, so the
/// first word is read before anything is parsed.
pub(crate) fn is_object_ddl(sql: &str) -> bool {
    let head = sql.trim_start().as_bytes();
    let starts = |w: &str| head.len() >= w.len() && head[..w.len()].eq_ignore_ascii_case(w.as_bytes());
    if !["create", "drop", "alter", "refresh", "import"]
        .iter()
        .any(|w| starts(w))
    {
        return false;
    }
    tree::read(sql, object_plan).is_some()
}

impl Session {
    /// Handles the statements of this module; None for any other.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_object_ddl(&mut self, sql: &str) -> Option<Result<Command>> {
        if !is_object_ddl(sql) {
            return None;
        }
        Some(match tree::read(sql, object_plan)? {
            ObjectPlan::CreateExtension { name, if_not_exists } => self.create_extension(&name, if_not_exists),
            ObjectPlan::DropExtension { names, if_exists } => self.drop_extension(&names, if_exists),
            ObjectPlan::AlterExtension => Ok(Command::from("ALTER EXTENSION")),
            ObjectPlan::CreateCollation {
                name,
                if_not_exists,
                from,
                options,
            } => self.create_collation(&name, if_not_exists, from, &options),
            ObjectPlan::DropCollation { name, if_exists } => self.drop_collation(&name, if_exists),
            ObjectPlan::AlterCollation => Ok(Command::from("ALTER COLLATION")),
            ObjectPlan::Partitioned => Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "partitioned tables are not supported"
            )),
            ObjectPlan::CreateMatview {
                reference,
                shown,
                columns,
                if_not_exists,
                with_data,
            } => self.create_matview(sql, &reference, &shown, &columns, if_not_exists, with_data),
            ObjectPlan::RefreshMatview {
                reference,
                shown,
                with_data,
            } => self.refresh_matview(&reference, &shown, with_data),
            ObjectPlan::DropMatview { names, if_exists } => self.drop_matview(&names, if_exists),
            ObjectPlan::AlterMatview => Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER MATERIALIZED VIEW is not supported"
            )),
            ObjectPlan::ForeignData => Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "foreign-data wrappers are not supported"
            )),
        })
    }

    /// CREATE MATERIALIZED VIEW [IF NOT EXISTS] name [(cols)] AS query [WITH [NO] DATA].
    /// The query is the client's text after the top-level AS, up to WITH [NO] DATA.
    fn create_matview(
        &mut self,
        sql: &str,
        reference: &str,
        shown: &str,
        columns: &[String],
        if_not_exists: bool,
        with_data: bool,
    ) -> Result<Command> {
        let toks = tree::tokens(sql);
        let is = |i: usize, w: &str| {
            toks.get(i)
                .is_some_and(|t| t.keyword && sql[t.start..t.end].eq_ignore_ascii_case(w))
        };
        let Some(as_at) = (0..toks.len()).find(|&k| toks[k].depth == 0 && is(k, "as")) else {
            return Err(errf!(
                SqlState::SYNTAX_ERROR,
                "syntax error in CREATE MATERIALIZED VIEW"
            ));
        };
        let mut n = toks.len();
        if n > 0 && &sql[toks[n - 1].start..toks[n - 1].end] == ";" {
            n -= 1;
        }
        let mut query_end = toks.get(n.wrapping_sub(1)).map_or(sql.len(), |t| t.end);
        if n >= 3 && is(n - 2, "with") && is(n - 1, "data") {
            query_end = toks[n - 2].start;
        } else if n >= 4 && is(n - 3, "with") && is(n - 2, "no") && is(n - 1, "data") {
            query_end = toks[n - 3].start;
        }
        let query = sql[toks[as_at].end..query_end].trim();
        let column_list = (!columns.is_empty()).then(|| {
            [
                " (",
                &columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", "),
                ")",
            ]
            .concat()
        });
        let column_list = column_list.as_deref();
        let stored = self.stored_name(reference);
        if self.relation_exists(&stored)? {
            if if_not_exists {
                self.c
                    .send_notice(&format!("relation {:?} already exists, skipping", quoted(shown)))?;
                return Ok(Command::from("CREATE MATERIALIZED VIEW"));
            }
            return Err(errf!(
                SqlState::DUPLICATE_TABLE,
                "relation {:?} already exists",
                quoted(shown)
            ));
        }
        // Its query is read now, as a query is, with the creator's privileges and
        // through the creator's policies; WITH NO DATA reads nothing.
        if with_data {
            self.check_privileges(query)?;
        }
        let fill = self.rewrite_for_exec(&["SELECT * FROM (", query, ")"].concat(), &[])?;
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
            let rs = self.exec(&["INSERT INTO ", &quote_ident(&stored), " ", &fill].concat(), &[])?;
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
        // Its creator's, as a table is.
        let owner = self.current_role.clone();
        self.exec(
            "INSERT OR REPLACE INTO _overlite_owners (tablename, owner) VALUES (?1, ?2)",
            &[Value::from(stored.as_str()), Value::from(owner.as_str())],
        )?;
        if with_data {
            return Ok(Command::from(format!("SELECT {rows}").as_str()));
        }
        Ok(Command::from("CREATE MATERIALIZED VIEW"))
    }

    /// REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA].
    fn refresh_matview(&mut self, reference: &str, shown: &str, with_data: bool) -> Result<Command> {
        let no_data = !with_data;
        let stored = self.stored_name(reference);
        let reference = shown;
        if !self.is_matview(&stored)? {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation {:?} does not exist",
                quoted(&reference)
            ));
        }
        // Its owner alone refreshes it, and its query reads with the owner's
        // privileges, whoever asks, as in Postgres.
        if !self.priv_bypass() && !self.has_privilege(&stored, crate::auth::privileges::Privilege::Owner) {
            return Err(errf!(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be owner of materialized view {reference}"
            ));
        }
        if !no_data
            && let Some(owner) = self.table_owner(&stored)
            && !self.role_bypasses(&owner)
        {
            let ddl = self
                .db
                .schema_object(&(MATVIEW_PREFIX.to_string() + &stored))
                .map(|(_, ddl)| ddl)
                .unwrap_or_default();
            let query = crate::auth::privileges::view_query(&ddl).unwrap_or("").to_string();
            self.as_role(&owner, |s| s.check_owned_query(&query, 1, "materialized view", &stored))?;
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
    fn drop_matview(&mut self, names: &[(String, String)], if_exists: bool) -> Result<Command> {
        // All or none, as one statement.
        self.exec("SAVEPOINT _mv", &[])?;
        let r = self.drop_matviews(names, if_exists);
        if r.is_err() {
            let _ = self.exec("ROLLBACK TO _mv", &[]);
        }
        let _ = self.exec("RELEASE _mv", &[]);
        r.map(|()| Command::from("DROP MATERIALIZED VIEW"))
    }

    fn drop_matviews(&mut self, names: &[(String, String)], if_exists: bool) -> Result<()> {
        for (reference, shown) in names {
            let stored = self.stored_name(reference);
            let reference = shown;
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
    fn create_extension(&mut self, name: &str, if_not_exists: bool) -> Result<Command> {
        let name = name.to_string();
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
    fn drop_extension(&mut self, names: &[String], if_exists: bool) -> Result<Command> {
        let names = names.to_vec();
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
    fn create_collation(
        &mut self,
        name: &str,
        if_not_exists: bool,
        from: Option<String>,
        options: &[(String, String)],
    ) -> Result<Command> {
        let name = name.to_string();
        let (mut provider, mut locale, mut deterministic) = ("libc".to_string(), String::new(), true);
        if let Some(from) = from {
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
            for (k, v) in options {
                let v = v.clone();
                match k.as_str() {
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
    fn drop_collation(&mut self, name: &str, if_exists: bool) -> Result<Command> {
        let name = name.to_string();
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
    }
}
