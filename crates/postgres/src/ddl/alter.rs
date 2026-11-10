//! ALTER TABLE forms SQLite can't run natively. UNIQUE constraints become a real
//! unique index; column-definition changes (TYPE / NOT NULL / DEFAULT) are done
//! with SQLite's recommended table rebuild (create-copy-swap) inside a savepoint,
//! preserving data and recreating indexes/triggers. A PRIMARY KEY or FOREIGN KEY
//! the rebuild cannot take stays accepted-but-not-enforced (pg_dump restore relies
//! on that), by the no-op path; a CHECK is enforced or the statement fails.

use crate::auth::grants::index_word;
use crate::auth::rls::{balanced_paren, is_rls_ddl, text_or_empty};
use crate::ddl::alter_actions::split_alter_actions;
use crate::ddl::column_if_exists::has_column_if_exists;
use crate::ddl::drop_constraint::{DropColumn, DropConstraint};
use crate::ddl::exclude::parse_exclusion;
use crate::ddl::identity::{retype_identity, retype_serial_key};
use crate::ddl::sequences::{is_ident_part, is_ident_start, split_top_level};
use crate::ddl::tree::{self, code_tokens};
use crate::error::{Error, Result, errf};
use crate::rewrite::rewrite;
use crate::session::Session;
use crate::session::utility::{alter_is_noop, first_word_upper, log_query, second_word_upper, unquote_ident};
use overlite_core::text::{eq_fold, quote_ident, quote_literal, quoted, slice, trim_start_chars};
use overlite_core::{Command, SqlState, lazy_regex};
use overlite_engine::{NOT_VALID_MARK, assignment_type, move_table_rows_sql, split_stored_name, stored_table_name};
use pg_query::NodeEnum;

impl Session {
    /// Handles the ALTER TABLE forms we can implement; None for the rest (which
    /// fall through to the no-op interceptor).
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_alter_table(&mut self, sql: &str) -> Option<Result<Command>> {
        if !eq_fold(&first_word_upper(sql), "ALTER") || second_word_upper(sql) != "TABLE" {
            return None;
        }
        let target = alter_target(sql)?;
        let if_exists = target.if_exists;
        // Clients qualify with public.; in SQLite that schema is the unqualified
        // name, while any other schema really is part of the table's name.
        let table = self.stored_table(target.unqualified.clone());
        let tag = || Command::from("ALTER TABLE");
        if let Some(r) = self.column_if_exists(sql, &table) {
            return Some(r.map(|()| tag()));
        }
        match &target.kind {
            AlterKind::Add => {
                // A rebuild puts the constraint in the CREATE TABLE, where the catalog
                // reads it back as a constraint rather than as a bare index, which is
                // what lets a dump round-trip. Both fallbacks below keep a restore
                // that used to succeed from starting to fail.
                if let Some((name, clause)) = parse_add_exclusion(sql) {
                    let added = parse_exclusion(name, &clause).and_then(|x| self.add_exclusion(&table, &x));
                    return Some(added.map(|()| tag()));
                }
                if let Some(clause) = parse_add_constraint(sql) {
                    let added = self.alter_add_constraint(&table, &clause);
                    // A CHECK is held or refused: it is never taken without effect, and
                    // neither is a foreign key that rows already break.
                    let broken = matches!(&added, Err(e) if e.sql_state() == SqlState::FOREIGN_KEY_VIOLATION);
                    if added.is_ok() || clause.check.is_some() || broken {
                        return Some(added.map(|()| tag()));
                    }
                }
                if let Some(def) = add_column_computed_default(sql) {
                    // The reference as written, not the unqualified name: the rebuild
                    // has to find the table in its own schema.
                    return Some(
                        self.alter_add_column_default(&unquote_ref(&target.reference), &def)
                            .map(|()| tag()),
                    );
                }
                if adds_unique(sql) {
                    return Some(self.alter_add_unique(sql, &table).map(|()| tag()));
                }
            }
            AlterKind::AlterColumn(column, cmd) => {
                return Some(self.alter_column(sql, &table, column, cmd).map(|()| tag()));
            }
            AlterKind::Validate(name) => match self.validate_check(&table, name) {
                Ok(true) => return Some(Ok(tag())),
                Ok(false) => {}
                Err(e) => return Some(Err(e)),
            },
            AlterKind::DropConstraint(drop) => {
                return Some(self.alter_drop_constraint(&table, drop, if_exists).map(|()| tag()));
            }
            AlterKind::DropColumn(drop) => {
                return Some(self.alter_drop_column(&table, drop).map(|()| tag()));
            }
            AlterKind::Rename(new_name) => return Some(self.alter_rename(&table, new_name).map(|()| tag())),
            // ALTER TABLE t SET SCHEMA y -- move the table between schemas.
            AlterKind::SetSchema(schema) => return Some(self.alter_set_schema(&table, schema).map(|()| tag())),
            AlterKind::Other => {}
        }
        None
    }

    /// Answers ALTER TABLE IF EXISTS on a table that is not there, whatever the
    /// action: a notice for the client, and true. Every route comes through here
    /// first, as most of them would fail or write down a row for the table.
    pub(crate) fn alter_skipped(&mut self, sql: &str) -> Result<bool> {
        let Some(target) = alter_target(sql).filter(|t| t.if_exists) else {
            return Ok(false);
        };
        let reference = target.reference.as_str();
        let stored = self.stored_name(reference);
        let q = "SELECT 1 FROM main.sqlite_master WHERE type IN ('table', 'view') AND lower(name) = lower(".to_string()
            + &quote_literal(&stored)
            + ")";
        if !self.exec(&q, &[])?.rows.is_empty() {
            return Ok(false);
        }
        let (_, table) = split_qualified_ref(reference);
        if !self.later_action {
            self.c
                .send_notice(&format!("relation \"{table}\" does not exist, skipping"))?;
        }
        Ok(true)
    }

    /// The name a table is stored under. A bare name reaches SQLite through the
    /// engine's search_path rewrite, but what reads sqlite_master by name has to find
    /// the table itself, in the same order: `public` and `$user` do not count, and
    /// a schema that does not hold the table leaves the name as it is.
    fn stored_table(&mut self, table: String) -> String {
        if table.contains('.') {
            return table;
        }
        for schema in self.search_path.clone().unwrap_or_default() {
            if eq_fold(&schema, "public") || schema == "$user" {
                continue;
            }
            let stored = schema + "." + &table;
            let rs = self.exec(
                &("SELECT 1 FROM main.sqlite_master WHERE type = 'table' AND name = ".to_string()
                    + &quote_literal(&stored)),
                &[],
            );
            if rs.is_ok_and(|rs| !rs.rows.is_empty()) {
                return stored;
            }
        }
        table
    }

    /// The stored name of a table reference as a statement writes it: public is the
    /// bare name, another schema is part of the name, and no schema goes by the
    /// search_path.
    pub(crate) fn stored_name(&mut self, reference: &str) -> String {
        let (schema, table) = split_qualified_ref(reference);
        if schema.is_empty() {
            return self.stored_table(table);
        }
        stored_table_name(&schema, &table)
    }

    /// Renames a table. It stays in its schema, which is part of its stored name, and
    /// its owner and grants go with it.
    fn alter_rename(&mut self, table: &str, new_name: &str) -> Result<()> {
        let (schema, _) = split_stored_name(table);
        let new = stored_table_name(&schema, new_name);
        self.exec(
            &("ALTER TABLE ".to_string() + &quote_ident(table) + " RENAME TO " + &quote_ident(&new)),
            &[],
        )?;
        self.exec(&move_table_rows_sql(table, &new), &[])?;
        self.rls_changed();
        Ok(())
    }

    /// Moves a table to another schema via the engine's SchemaManager.
    fn alter_set_schema(&mut self, table_ref: &str, new_schema: &str) -> Result<()> {
        let moved = self.db.set_table_schema(table_ref, new_schema);
        self.rls_changed();
        Ok(moved?)
    }
}

/// Reports whether the extended path should run this statement through
/// alter_table_prepared, the one place that checks ownership for all of these.
pub(crate) fn alter_table_routed(sql: &str) -> bool {
    if !eq_fold(&first_word_upper(sql), "ALTER") || second_word_upper(sql) != "TABLE" {
        return false;
    }
    alter_table_handled(sql)
        || split_alter_actions(sql).len() > 1
        || alter_is_noop(sql)
        || is_rls_ddl(sql)
        || alter_target(sql).is_some_and(|t| t.if_exists)
}

/// What an ALTER TABLE acts on, read from its node (an ALTER TABLE ... RENAME TO and
/// SET SCHEMA are statements of their own there).
struct AlterTarget<'a> {
    if_exists: bool,
    /// The table as a reference (`split_qualified_ref`'s), as Postgres reads it.
    reference: String,
    /// The table without a public qualifier: `t`, or `s.t` in another schema.
    unqualified: String,
    /// The text of its first action, as written.
    action: &'a str,
    kind: AlterKind,
}

/// The first action of an ALTER TABLE.
enum AlterKind {
    /// ADD COLUMN or ADD CONSTRAINT.
    Add,
    /// ALTER [COLUMN] name ...
    AlterColumn(String, ColumnCmd),
    /// VALIDATE CONSTRAINT name.
    Validate(String),
    DropConstraint(DropConstraint),
    DropColumn(DropColumn),
    /// RENAME TO name.
    Rename(String),
    /// SET SCHEMA name.
    SetSchema(String),
    Other,
}

/// What ALTER COLUMN does.
enum ColumnCmd {
    Type,
    SetNotNull,
    DropNotNull,
    SetDefault,
    DropDefault,
    AddIdentity,
    DropIdentity { if_exists: bool },
    SetGenerated { always: bool },
    Other,
}

fn alter_target(sql: &str) -> Option<AlterTarget<'_>> {
    use pg_query::protobuf::{AlterTableType as A, DropBehavior, ObjectType};
    let (relation, if_exists, kind) = tree::read(sql, |n| match n {
        NodeEnum::AlterTableStmt(a) if tree::object(a.objtype) == ObjectType::ObjectTable => {
            let Some(NodeEnum::AlterTableCmd(c)) = a.cmds.first()?.node.as_ref() else {
                return None;
            };
            let cascade = c.behavior == DropBehavior::DropCascade as i32;
            let kind = match A::try_from(c.subtype).ok()? {
                A::AtAddColumn | A::AtAddConstraint => AlterKind::Add,
                A::AtAlterColumnType => AlterKind::AlterColumn(c.name.clone(), ColumnCmd::Type),
                A::AtSetNotNull => AlterKind::AlterColumn(c.name.clone(), ColumnCmd::SetNotNull),
                A::AtDropNotNull => AlterKind::AlterColumn(c.name.clone(), ColumnCmd::DropNotNull),
                A::AtColumnDefault if c.def.is_some() => AlterKind::AlterColumn(c.name.clone(), ColumnCmd::SetDefault),
                A::AtColumnDefault => AlterKind::AlterColumn(c.name.clone(), ColumnCmd::DropDefault),
                A::AtAddIdentity => AlterKind::AlterColumn(c.name.clone(), ColumnCmd::AddIdentity),
                A::AtDropIdentity => AlterKind::AlterColumn(
                    c.name.clone(),
                    ColumnCmd::DropIdentity {
                        if_exists: c.missing_ok,
                    },
                ),
                A::AtSetIdentity => {
                    let always = tree::defs(match c.def.as_deref().and_then(|d| d.node.as_ref()) {
                        Some(NodeEnum::List(l)) => &l.items,
                        _ => &[][..],
                    })
                    .find(|d| d.defname == "generated")
                    .is_some_and(|d| {
                        matches!(d.arg.as_deref().and_then(|a| a.node.as_ref()), Some(NodeEnum::Integer(i)) if i.ival == i32::from(b'a'))
                    });
                    AlterKind::AlterColumn(c.name.clone(), ColumnCmd::SetGenerated { always })
                }
                A::AtSetExpression
                | A::AtDropExpression
                | A::AtSetStatistics
                | A::AtSetOptions
                | A::AtResetOptions
                | A::AtSetStorage
                | A::AtSetCompression => AlterKind::AlterColumn(c.name.clone(), ColumnCmd::Other),
                A::AtValidateConstraint => AlterKind::Validate(c.name.clone()),
                A::AtDropConstraint => AlterKind::DropConstraint(DropConstraint::new(&c.name, c.missing_ok, cascade)),
                // DROP COLUMN IF EXISTS is column_if_exists's.
                A::AtDropColumn if !c.missing_ok => AlterKind::DropColumn(DropColumn::new(&c.name, cascade)),
                _ => AlterKind::Other,
            };
            Some((a.relation.clone()?, a.missing_ok, kind))
        }
        NodeEnum::RenameStmt(r) if tree::object(r.rename_type) == ObjectType::ObjectTable => {
            Some((r.relation.clone()?, r.missing_ok, AlterKind::Rename(r.newname.clone())))
        }
        // RENAME COLUMN and RENAME CONSTRAINT: the engine's, IF EXISTS answered here.
        NodeEnum::RenameStmt(r) if tree::object(r.relation_type) == ObjectType::ObjectTable => {
            Some((r.relation.clone()?, r.missing_ok, AlterKind::Other))
        }
        NodeEnum::AlterObjectSchemaStmt(a) if tree::object(a.object_type) == ObjectType::ObjectTable => Some((
            a.relation.clone()?,
            a.missing_ok,
            AlterKind::SetSchema(a.newschema.clone()),
        )),
        _ => None,
    })?;
    let action = crate::ddl::alter_actions::alter_parts(sql)
        .and_then(|p| p.actions.first().copied())
        .unwrap_or("");
    let unqualified = if relation.schemaname.is_empty() || eq_fold(&relation.schemaname, "public") {
        relation.relname.clone()
    } else {
        [relation.schemaname.as_str(), ".", &relation.relname].concat()
    };
    Some(AlterTarget {
        if_exists,
        reference: tree::reference(&relation),
        unqualified,
        action,
        kind,
    })
}

/// Reports whether try_alter_table would take over this ALTER.
fn alter_table_handled(sql: &str) -> bool {
    if !eq_fold(&first_word_upper(sql), "ALTER") || second_word_upper(sql) != "TABLE" {
        return false;
    }
    let Some(target) = alter_target(sql) else {
        return false;
    };
    if has_column_if_exists(sql) {
        return true;
    }
    match target.kind {
        AlterKind::Add => {
            add_column_computed_default(sql).is_some()
                || adds_unique(sql)
                || parse_add_constraint(sql).is_some()
                || parse_add_exclusion(sql).is_some()
        }
        AlterKind::AlterColumn(..)
        | AlterKind::DropConstraint(_)
        | AlterKind::DropColumn(_)
        | AlterKind::SetSchema(_)
        | AlterKind::Rename(_) => true,
        AlterKind::Validate(_) | AlterKind::Other => false,
    }
}

/// The UNIQUE key an ALTER TABLE ... ADD [CONSTRAINT name] UNIQUE (cols) adds: its
/// name ("" when none is given), and whether its NULLs are not distinct.
fn added_unique(sql: &str) -> Option<(String, bool)> {
    use pg_query::protobuf::{AlterTableType as A, ConstrType};
    tree::read(sql, |n| {
        let NodeEnum::AlterTableStmt(a) = n else { return None };
        let Some(NodeEnum::AlterTableCmd(c)) = a.cmds.first()?.node.as_ref() else {
            return None;
        };
        if c.subtype != A::AtAddConstraint as i32 {
            return None;
        }
        match c.def.as_deref()?.node.as_ref()? {
            NodeEnum::Constraint(k) if k.contype == ConstrType::ConstrUnique as i32 => {
                Some((k.conname.clone(), k.nulls_not_distinct))
            }
            _ => None,
        }
    })
}

/// Whether an ADD is about a UNIQUE key.
fn adds_unique(sql: &str) -> bool {
    added_unique(sql).is_some()
}

impl Session {
    /// Turns ADD [CONSTRAINT name] UNIQUE (cols) into a unique index.
    fn alter_add_unique(&mut self, sql: &str, table: &str) -> Result<()> {
        let (Some((mut name, nulls_not_distinct)), Some(cols)) = (added_unique(sql), unique_columns(sql)) else {
            return Err(Error::with_state(SqlState::SYNTAX_ERROR, "syntax error in ADD UNIQUE"));
        };
        if name.is_empty() {
            name = table.to_string() + "_" + &cols.replace(' ', "") + "_key";
        }
        let mut index = "CREATE UNIQUE INDEX ".to_string()
            + &quote_ident(&name)
            + " ON "
            + &quote_ident(table)
            + " ("
            + &cols
            + ")";
        if nulls_not_distinct {
            index = rewrite(&(index + " NULLS NOT DISTINCT"));
        }
        self.exec(&index, &[])?;
        Ok(())
    }
}

/// The column list of the UNIQUE an ALTER TABLE adds, as written: what is inside the
/// parentheses after UNIQUE [NULLS [NOT] DISTINCT], as Postgres' scanner finds them.
fn unique_columns(sql: &str) -> Option<String> {
    let toks = code_tokens(sql);
    let u = toks.iter().position(|t| t.depth == 0 && t.is_word(sql, "unique"))?;
    let open = (u + 1..toks.len()).find(|&k| toks[k].is_punct(sql, "("))?;
    let close = (open + 1..toks.len()).find(|&k| toks[k].depth == toks[open].depth && toks[k].is_punct(sql, ")"))?;
    Some(sql[toks[open].span.end..toks[close].span.start].trim().to_string())
}

impl Session {
    /// Handles ALTER [COLUMN] c { TYPE t | SET/DROP NOT NULL | SET/DROP DEFAULT }
    /// via a table rebuild.
    fn alter_column(&mut self, sql: &str, table: &str, col: &str, cmd: &ColumnCmd) -> Result<()> {
        let col = col.to_string();
        // Identity: ADD GENERATED, DROP IDENTITY [IF EXISTS], SET GENERATED.
        match cmd {
            ColumnCmd::AddIdentity => return self.alter_add_identity(table, &col, sql),
            ColumnCmd::DropIdentity { if_exists } => return self.alter_drop_identity(table, &col, *if_exists),
            ColumnCmd::SetGenerated { always } => return self.alter_set_generated(table, &col, *always),
            _ => {}
        }
        // A nextval default is no SQLite default: it is kept in _overlite_defaults, which
        // an INSERT that leaves the column out reads, as CREATE TABLE keeps one.
        let nextval = match cmd {
            ColumnCmd::SetDefault => crate::ddl::defaults::nextval_in(&default_arg(sql)),
            _ => None,
        };
        if matches!(cmd, ColumnCmd::SetDefault | ColumnCmd::DropDefault) {
            let (t, c) = (quote_literal(table), quote_literal(&col));
            self.exec(
                &format!(
                    "DELETE FROM _overlite_defaults WHERE lower(tablename) = lower({t}) AND lower(colname) = lower({c})"
                ),
                &[],
            )?;
            if let Some(seq) = &nextval {
                self.exec(
                    &format!(
                        "INSERT INTO _overlite_defaults (tablename, colname, seqname) VALUES ({t}, {c}, {})",
                        quote_literal(seq)
                    ),
                    &[],
                )?;
            }
        }
        // A new type's modifier, which the rows are made to fit, as Postgres rewrites them.
        let mut fit: Option<String> = None;
        let edit: ColumnEdit = if matches!(cmd, ColumnCmd::Type) {
            let new_type = column_type_arg(sql);
            if new_type.is_empty() {
                return Err(Error::with_state(
                    SqlState::SYNTAX_ERROR,
                    "syntax error in ALTER COLUMN TYPE",
                ));
            }
            // A collation goes as it does in CREATE TABLE ("C" is SQLite's default),
            // and the old one with the old type, as in Postgres.
            let new_type = column_type_for_sqlite(&new_type);
            // The type alone, without the collation that comes with a numeric one.
            let bare = new_type
                .to_ascii_lowercase()
                .find(" collate ")
                .map_or(new_type.as_str(), |c| &new_type[..c]);
            fit = assignment_type(&bare.replace(", ", ","));
            Box::new(move |name, typ, cons| {
                match retype_identity(typ, cons, &new_type).or_else(|| retype_serial_key(typ, cons, &new_type)) {
                    Some((typ, cons)) => join_def(name, &typ, &drop_collate(&cons)),
                    None => join_def(name, &new_type, &drop_collate(cons)),
                }
            })
        } else if matches!(cmd, ColumnCmd::SetNotNull) {
            Box::new(|name, typ, cons| join_def(name, typ, &add_not_null(cons)))
        } else if matches!(cmd, ColumnCmd::DropNotNull) {
            Box::new(|name, typ, cons| join_def(name, typ, &drop_not_null(cons)))
        } else if nextval.is_some() {
            Box::new(|name, typ, cons| join_def(name, typ, &drop_default(cons)))
        } else if matches!(cmd, ColumnCmd::SetDefault) {
            let def = default_arg(sql);
            Box::new(move |name, typ, cons| join_def(name, typ, &set_default(cons, &default_value(name, typ, &def))))
        } else if matches!(cmd, ColumnCmd::DropDefault) {
            Box::new(|name, typ, cons| join_def(name, typ, &drop_default(cons)))
        } else {
            return Err(errf!(
                SqlState::FEATURE_NOT_SUPPORTED,
                "unsupported ALTER COLUMN action"
            ));
        };

        let ddl = self.table_ddl(table);
        if ddl.is_empty() {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation {:?} does not exist",
                quoted(table)
            ));
        }
        let Some(new_ddl) = edit_column(&ddl, &col, edit) else {
            return Err(errf!(
                SqlState::UNDEFINED_COLUMN,
                "column {:?} of relation {:?} does not exist",
                quoted(col),
                quoted(table)
            ));
        };
        let Some(ty) = fit else {
            return self.rebuild_table(table, &new_ddl);
        };
        // numeric(12,3) holds 9.500 and varchar(3) refuses 'abcd', in the rows there are.
        self.exec("SAVEPOINT _ty", &[])?;
        let column = quote_ident(&col);
        let refit = "UPDATE ".to_string()
            + &quote_ident(table)
            + " SET "
            + &column
            + " = overlite_assign("
            + &column
            + ", "
            + &quote_literal(&ty)
            + ") WHERE "
            + &column
            + " IS NOT NULL";
        let done = self.exec(&refit, &[]).and_then(|_| self.rebuild_table(table, &new_ddl));
        if done.is_err() {
            let _ = self.exec("ROLLBACK TO _ty", &[]);
        }
        let _ = self.exec("RELEASE _ty", &[]);
        done
    }
}

/// The type after TYPE in an ALTER COLUMN ... [SET DATA] TYPE, up to an optional
/// USING clause, as written: where Postgres' scanner finds those keywords.
fn column_type_arg(sql: &str) -> String {
    let toks = code_tokens(sql);
    let Some(t) = toks.iter().position(|t| t.depth == 0 && t.is_word(sql, "type")) else {
        return String::new();
    };
    let end = toks[t + 1..]
        .iter()
        .find(|k| k.depth == 0 && (k.is_word(sql, "using") || k.is_punct(sql, ";")))
        .map_or(sql.len(), |k| k.span.start);
    sql[toks[t].span.end..end].trim().to_string()
}

/// The expression after SET DEFAULT, as written.
fn default_arg(sql: &str) -> String {
    let toks = code_tokens(sql);
    let Some(d) = (1..toks.len())
        .find(|&i| toks[i].depth == 0 && toks[i].is_word(sql, "default") && toks[i - 1].is_word(sql, "set"))
    else {
        return String::new();
    };
    let end = toks[d + 1..]
        .iter()
        .find(|k| k.depth == 0 && k.is_punct(sql, ";"))
        .map_or(sql.len(), |k| k.span.start);
    sql[toks[d].span.end..end].trim().to_string()
}

/// A column type as a CREATE TABLE's translation writes it for SQLite (numeric as
/// DECIMALTEXT with its collation, a collation SQLite has, a citext's NOCASE, an
/// array's type quoted): what the rebuild declares the column as.
fn column_type_for_sqlite(typ: &str) -> String {
    let created = rewrite(&["CREATE TABLE _overlite_type (c ", typ, ")"].concat());
    created
        .find("(c ")
        .zip(created.rfind(')'))
        .filter(|(open, close)| open + 3 <= *close)
        .map_or_else(
            || typ.trim().to_string(),
            |(open, close)| created[open + 3..close].trim().to_string(),
        )
}

// --- rebuild ----------------------------------------------------------------

impl Session {
    pub(crate) fn table_ddl(&mut self, table: &str) -> String {
        let rs = self.exec(
            &("SELECT sql FROM sqlite_master WHERE type='table' AND lower(name)=lower(".to_string()
                + &quote_literal(table)
                + ")"),
            &[],
        );
        match rs {
            Ok(rs) if !rs.rows.is_empty() => text_or_empty(&rs.rows[0][0]),
            _ => String::new(),
        }
    }

    /// Replaces table with new_ddl (same column set) via create-copy-swap inside a
    /// savepoint, recreating its indexes and triggers.
    pub(crate) fn rebuild_table(&mut self, table: &str, new_ddl: &str) -> Result<()> {
        self.rebuild_table_copying(&quote_ident(table), new_ddl, &[], "main.sqlite_master", table)
    }

    /// rebuild_table for a DDL whose column set differs from the current one: copy
    /// names the columns to carry over, and any column of the new table missing
    /// from it takes its default.
    ///
    /// table is the qualified spelling to write in statements; master and name say
    /// where to read the table's indexes and triggers back from.
    pub(crate) fn rebuild_table_copying(
        &mut self,
        table: &str,
        new_ddl: &str,
        copy: &[String],
        master: &str,
        name: &str,
    ) -> Result<()> {
        let (schema, _) = split_qualified_ref(table);
        let aux = qualify_aux_ddl(self.aux_ddl_in(master, name), &schema);
        let tmp = temp_rebuild_name(table);
        let tmp_ddl = rename_create_table_to(new_ddl, &tmp);

        let db = master.strip_suffix(".sqlite_master").unwrap_or("main").to_string();
        let old_cols = self.table_column_kinds(&db, name);

        // The DROP of the old table deletes its rows first, and with foreign keys on
        // each row another table references counts as a violation that the RENAME of
        // the new one does not take back: the end of the transaction would fail. Out of
        // a transaction foreign keys are turned off for the swap, as SQLite's own
        // procedure for altering a table does, and the new table's own keys checked
        // after it (foreign_key_check).
        let fks_off = self.db.is_autocommit();
        if fks_off {
            self.exec("PRAGMA foreign_keys = OFF", &[])?;
        }
        let r = self.rebuild_swap(table, new_ddl, copy, name, &db, &tmp, &tmp_ddl, aux, &old_cols, fks_off);
        if fks_off {
            let _ = self.exec("PRAGMA foreign_keys = ON", &[]);
        }
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn rebuild_swap(
        &mut self,
        table: &str,
        new_ddl: &str,
        copy: &[String],
        name: &str,
        db: &str,
        tmp: &str,
        tmp_ddl: &str,
        aux: Vec<String>,
        old_cols: &[(String, bool)],
        fks_off: bool,
    ) -> Result<()> {
        self.exec("SAVEPOINT _rb", &[])?;
        // In a transaction, the table disappears for an instant between the DROP and
        // the RENAME, and SQLite checks any foreign key pointing at it as soon as it
        // goes. Deferring enforcement to the end of the transaction keeps those keys
        // intact without turning them off: PRAGMA foreign_keys itself is ignored
        // inside a transaction.
        let _ = self.exec("PRAGMA defer_foreign_keys = ON", &[]);
        // RENAME TO re-resolves every reference in the schema so it can fix the
        // ones pointing at the renamed table. Here the table it is replacing has
        // just been dropped, so that pass fails on any foreign key still naming it.
        // The legacy behaviour renames without the fix-up, which is what a
        // create-copy-swap wants: the references already name the final table.
        let _ = self.exec("PRAGMA legacy_alter_table = ON", &[]);
        if let Err(err) = self.exec(tmp_ddl, &[]) {
            let _ = self.exec("ROLLBACK TO _rb", &[]);
            let _ = self.exec("RELEASE _rb", &[]);
            let _ = self.exec("PRAGMA legacy_alter_table = OFF", &[]);
            let _ = self.exec("PRAGMA defer_foreign_keys = OFF", &[]);
            return Err(err);
        }
        let new_cols = self.table_column_kinds(db, "_overlite_rebuild");
        // A CHECK added NOT VALID does not hold for the rows from before it, which
        // the copy carries over: SQLite checks none of them as it copies, and every
        // other CHECK held for them already.
        let not_valid = new_ddl.contains(NOT_VALID_MARK);
        if not_valid {
            let _ = self.exec("PRAGMA ignore_check_constraints = ON", &[]);
        }
        let copied = self.exec(&copy_rows_sql(tmp, table, copy, old_cols, &new_cols), &[]);
        if not_valid {
            let _ = self.exec("PRAGMA ignore_check_constraints = OFF", &[]);
        }
        if let Err(err) = copied {
            let _ = self.exec("ROLLBACK TO _rb", &[]);
            let _ = self.exec("RELEASE _rb", &[]);
            let _ = self.exec("PRAGMA legacy_alter_table = OFF", &[]);
            let _ = self.exec("PRAGMA defer_foreign_keys = OFF", &[]);
            return Err(err);
        }
        let mut steps = vec![
            "DROP TABLE ".to_string() + table,
            // RENAME TO takes a bare name: the table stays where it already is.
            "ALTER TABLE ".to_string() + tmp + " RENAME TO " + &quote_ident(name),
        ];
        steps.extend(aux);
        for st in &steps {
            log_query("rebuild", st);
            if let Err(err) = self.exec(st, &[]) {
                let _ = self.exec("ROLLBACK TO _rb", &[]);
                let _ = self.exec("RELEASE _rb", &[]);
                let _ = self.exec("PRAGMA legacy_alter_table = OFF", &[]);
                let _ = self.exec("PRAGMA defer_foreign_keys = OFF", &[]);
                return Err(err);
            }
        }
        if fks_off {
            // A row the new table's own foreign keys find no parent for.
            let check = "SELECT 1 FROM pragma_foreign_key_check(".to_string()
                + &quote_literal(name)
                + ", "
                + &quote_literal(db)
                + ") LIMIT 1";
            if self.exec(&check, &[]).is_ok_and(|rs| !rs.rows.is_empty()) {
                let _ = self.exec("ROLLBACK TO _rb", &[]);
                let _ = self.exec("RELEASE _rb", &[]);
                let _ = self.exec("PRAGMA legacy_alter_table = OFF", &[]);
                let _ = self.exec("PRAGMA defer_foreign_keys = OFF", &[]);
                let (_, relation) = split_stored_name(name);
                return Err(errf!(
                    SqlState::FOREIGN_KEY_VIOLATION,
                    "insert or update on table \"{relation}\" violates foreign key constraint"
                ));
            }
        }
        let released = self.exec("RELEASE _rb", &[]);
        if released.is_err() && !self.db.is_autocommit() && !self.in_tx() {
            // The swap's own transaction would not end: it is undone, not left open.
            let _ = self.exec("ROLLBACK", &[]);
        }
        let _ = self.exec("PRAGMA legacy_alter_table = OFF", &[]);
        released?;
        // Deferral lasts until the enclosing transaction ends, which is not always
        // here: a rebuild inside a client transaction would otherwise leave foreign
        // keys unchecked for the rest of it.
        let _ = self.exec("PRAGMA defer_foreign_keys = OFF", &[]);
        Ok(())
    }
}

impl Session {
    /// A table's columns in order, each with whether it is generated, which a copy
    /// can neither read into nor write.
    fn table_column_kinds(&mut self, db: &str, name: &str) -> Vec<(String, bool)> {
        let sql = [
            "SELECT name, hidden FROM pragma_table_xinfo(",
            &quote_literal(name),
            ", ",
            &quote_literal(db),
            ")",
        ]
        .concat();
        let Ok(rs) = self.exec(&sql, &[]) else {
            return Vec::new();
        };
        rs.rows
            .iter()
            .map(|r| (text_or_empty(&r[0]), matches!(r[1], overlite_core::Value::Int(2 | 3))))
            .collect()
    }
}

/// The INSERT that copies a table's rows into its rebuilt replacement: every column
/// in order when copy is empty, else the columns copy names. A generated column is
/// computed by the new table, so it is never copied: SQLite refuses a value for one,
/// and `SELECT *` would bring one value too many.
fn copy_rows_sql(tmp: &str, table: &str, copy: &[String], old: &[(String, bool)], new: &[(String, bool)]) -> String {
    let generated = |cols: &[(String, bool)], c: &str| cols.iter().any(|(n, g)| *g && n.eq_ignore_ascii_case(c));
    let (into, from): (Vec<String>, Vec<String>) = if copy.is_empty() {
        if !old.iter().chain(new).any(|(_, g)| *g) || old.len() != new.len() {
            return ["INSERT INTO ", tmp, " SELECT * FROM ", table].concat();
        }
        old.iter()
            .zip(new)
            .filter(|((_, og), (_, ng))| !og && !ng)
            .map(|((o, _), (n, _))| (quote_ident(n), quote_ident(o)))
            .unzip()
    } else {
        copy.iter()
            .filter(|c| !generated(old, c) && !generated(new, c))
            .map(|c| (quote_ident(c), quote_ident(c)))
            .unzip()
    };
    [
        "INSERT INTO ",
        tmp,
        " (",
        &into.join(", "),
        ") SELECT ",
        &from.join(", "),
        " FROM ",
        table,
    ]
    .concat()
}

/// Rewrites one column definition from its name, type and constraints.
type ColumnEdit = Box<dyn Fn(&str, &str, &str) -> String>;

// --- DDL editing ------------------------------------------------------------

/// rename_create_table for a name already rendered as SQL, which a
/// schema-qualified one has to be.
fn rename_create_table_to(ddl: &str, rendered: &str) -> String {
    let low = ddl.to_ascii_lowercase();
    let Some(i) = index_word(&low, "table") else {
        return ddl.to_string();
    };
    let b = ddl.as_bytes();
    let mut j = i + "table".len();
    // skip IF NOT EXISTS
    if let Some(k) = index_word(&low[j..], "if")
        && k < 6
        && let Some(p) = index_word(&low, "exists")
        && p > j
    {
        j = p + "exists".len();
    }
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t' || b[j] == b'\n') {
        j += 1;
    }
    let mut end = j;
    if end < b.len() && b[end] == b'"' {
        end += 1;
        while end < b.len() && (b[end] != b'"' || b.get(end + 1) == Some(&b'"')) {
            end += if b[end] == b'"' { 2 } else { 1 };
        }
        if end < b.len() {
            end += 1;
        }
    } else {
        while end < b.len() && (is_ident_part(b[end]) || b[end] == b'.') {
            end += 1;
        }
    }
    ddl[..j].to_string() + rendered + &ddl[end..]
}

/// Finds the definition of col in a CREATE TABLE's column list and rewrites it
/// with edit(name, type, constraints).
fn edit_column(ddl: &str, col: &str, edit: impl Fn(&str, &str, &str) -> String) -> Option<String> {
    let open = ddl.find('(')?;
    let (inner, after) = balanced_paren(ddl, open);
    let mut defs = split_top_level(&inner);
    let mut found = false;
    for d in &mut defs {
        let (name, typ, cons) = parse_col_def(d);
        if !name.is_empty() && eq_fold(&unquote_ident(&name), col) {
            *d = " ".to_string() + &edit(&name, &typ, &cons) + " ";
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    Some(ddl[..=open].to_string() + &defs.join(",") + slice(ddl, after - 1, ddl.len()))
}

/// Splits a column definition into its name, type, and the remaining constraint
/// text.
pub(crate) fn parse_col_def(def: &str) -> (String, String, String) {
    let d = def.trim();
    if d.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    // A table-level constraint isn't a column def.
    let first = trim_start_chars(d, "\"").to_uppercase();
    for kw in ["CONSTRAINT ", "PRIMARY ", "UNIQUE ", "CHECK", "FOREIGN ", "EXCLUDE "] {
        if first.starts_with(kw) {
            return (String::new(), String::new(), String::new());
        }
    }
    // name
    let db = d.as_bytes();
    let mut np = 0;
    if db[0] == b'"' {
        np = 1;
        while np < db.len() && db[np] != b'"' {
            np += 1;
        }
        if np < db.len() {
            np += 1;
        }
    } else {
        while np < db.len() && (is_ident_part(db[np]) || db[np] == b'.') {
            np += 1;
        }
    }
    let name = &d[..np];
    let rest = d[np..].trim();
    // type = tokens up to the first column-constraint keyword (paren-aware)
    let rb = rest.as_bytes();
    let mut depth = 0;
    let mut k = 0;
    while k < rb.len() {
        if rb[k] == b'(' {
            depth += 1;
            k += 1;
            continue;
        }
        if rb[k] == b')' {
            depth -= 1;
            k += 1;
            continue;
        }
        if depth == 0 && is_ident_start(rb[k]) {
            let mut w = k;
            while w < rb.len() && is_ident_part(rb[w]) {
                w += 1;
            }
            if matches!(
                rest[k..w].to_lowercase().as_str(),
                "not"
                    | "null"
                    | "default"
                    | "primary"
                    | "unique"
                    | "check"
                    | "references"
                    | "collate"
                    | "generated"
                    | "as"
                    | "constraint"
            ) {
                break;
            }
            k = w;
            continue;
        }
        k += 1;
    }
    (
        name.to_string(),
        slice(rest, 0, k).trim().to_string(),
        slice(rest, k, rest.len()).trim().to_string(),
    )
}

pub(crate) fn join_def(name: &str, typ: &str, cons: &str) -> String {
    let mut out = name.to_string();
    if !typ.is_empty() {
        out.push(' ');
        out.push_str(typ);
    }
    if !cons.is_empty() {
        out.push(' ');
        out.push_str(cons);
    }
    out
}

/// Where a column's own NOT NULL clause is among its constraints: one inside a
/// CHECK or a literal is not it.
fn not_null_clause(cons: &str) -> Option<std::ops::Range<usize>> {
    let toks = code_tokens(cons);
    toks.windows(2)
        .find(|w| w[0].depth == 0 && w[0].is_word(cons, "not") && w[1].is_word(cons, "null"))
        .map(|w| w[0].span.start..w[1].span.end)
}

fn add_not_null(cons: &str) -> String {
    if not_null_clause(cons).is_some() {
        return cons.to_string();
    }
    if cons.is_empty() {
        return "NOT NULL".to_string();
    }
    cons.to_string() + " NOT NULL"
}

fn drop_not_null(cons: &str) -> String {
    if let Some(r) = not_null_clause(cons) {
        return (cons[..r.start].to_string() + &cons[r.end..]).trim().to_string();
    }
    cons.to_string()
}

/// A default as the client wrote it, in the form SQLite stores: a cast folded into
/// its value or a CAST in parentheses, as CREATE TABLE writes it.
fn default_value(name: &str, typ: &str, def: &str) -> String {
    let cons =
        rewrite(&("ALTER TABLE t ADD COLUMN ".to_string() + &join_def(name, typ, &("DEFAULT ".to_string() + def))));
    let toks = code_tokens(&cons);
    match toks.iter().find(|t| t.depth == 0 && t.is_word(&cons, "default")) {
        Some(t) => cons[t.span.end..].trim().to_string(),
        None => def.to_string(),
    }
}

fn set_default(cons: &str, def: &str) -> String {
    let cons = drop_default(cons);
    if cons.is_empty() {
        return "DEFAULT ".to_string() + def;
    }
    cons + " DEFAULT " + def
}

/// Removes the COLLATE clauses of a column's constraints.
fn drop_collate(cons: &str) -> String {
    let toks = code_tokens(cons);
    let mut out = String::with_capacity(cons.len());
    let mut from = 0;
    let mut i = 0;
    while i < toks.len() {
        if toks[i].depth == 0 && toks[i].is_word(cons, "collate") && i + 1 < toks.len() {
            // The name, schema-qualified or not.
            let mut last = i + 1;
            while last + 2 < toks.len() && toks[last + 1].is_punct(cons, ".") {
                last += 2;
            }
            out.push_str(&cons[from..toks[i].span.start]);
            from = toks[last].span.end;
            i = last + 1;
            continue;
        }
        i += 1;
    }
    out.push_str(&cons[from..]);
    out.trim().to_string()
}

/// Removes a DEFAULT clause: its expression runs, as Postgres' grammar reads it, up to
/// the next of the column's constraints.
pub(crate) fn drop_default(cons: &str) -> String {
    // A column's own: not one in a literal or a CHECK, or ON DELETE SET DEFAULT.
    let toks = code_tokens(cons);
    let at = (0..toks.len()).find(|&i| {
        toks[i].depth == 0 && toks[i].is_word(cons, "default") && !(i > 0 && toks[i - 1].is_word(cons, "set"))
    });
    let Some(at) = at else {
        return cons.to_string();
    };
    let starts_constraint = |i: usize| {
        let t = &toks[i];
        t.depth == 0
            && [
                "not",
                "null",
                "constraint",
                "check",
                "unique",
                "primary",
                "references",
                "collate",
                "generated",
                "default",
            ]
            .iter()
            .any(|w| t.is_word(cons, w))
    };
    let end = (at + 2..toks.len())
        .find(|&i| starts_constraint(i))
        .map_or(cons.len(), |i| toks[i].span.start);
    (cons[..toks[at].span.start].to_string() + &cons[end..])
        .trim()
        .to_string()
}

/// A clause without the public qualifiers of its names (`REFERENCES public.u`): in
/// SQLite a table of public is its bare name. Where Postgres' scanner finds them, so
/// a literal or a longer name that spells it is not one.
fn without_public(clause: &str) -> String {
    let toks = code_tokens(clause);
    let mut out = String::with_capacity(clause.len());
    let mut at = 0;
    for w in toks.windows(2) {
        let name = &clause[w[0].span.clone()];
        if (eq_fold(name, "public") || name == "\"public\"") && w[1].is_punct(clause, ".") {
            out.push_str(&clause[at..w[0].span.start]);
            at = w[1].span.end;
        }
    }
    out.push_str(&clause[at..]);
    out
}

/// Finds sub in s, case-insensitively.
fn index_word_fold(s: &str, sub: &str) -> Option<usize> {
    s.to_ascii_uppercase().find(&sub.to_ascii_uppercase())
}

// --- ADD CONSTRAINT ---------------------------------------------------------

/// The name and the clause from EXCLUDE on of an ALTER TABLE ... ADD [CONSTRAINT
/// name] EXCLUDE ...
fn parse_add_exclusion(sql: &str) -> Option<(Option<String>, String)> {
    let action = alter_target(sql)?.action;
    let toks = code_tokens(action);
    let is = |k: usize, w: &str| toks.get(k).is_some_and(|t| t.is_word(action, w));
    if !is(0, "add") {
        return None;
    }
    let (name, at) = if is(1, "constraint") {
        (Some(unquote_ident(&action[toks.get(2)?.span.clone()])), 3)
    } else {
        (None, 1)
    };
    let opens = toks
        .get(at + 1)
        .is_some_and(|t| t.is_punct(action, "(") || t.is_word(action, "using"));
    (is(at, "exclude") && opens).then(|| (name, action[toks[at].span.start..].to_string()))
}

/// A table constraint an ALTER TABLE ... ADD gives, in the form a CREATE TABLE
/// accepts.
struct AddedConstraint {
    clause: String,
    /// For a CHECK, whether it was NOT VALID: the rows already there are not held to it.
    check: Option<bool>,
}

/// Extracts the table-level constraint clause from an ALTER TABLE ... ADD
/// [CONSTRAINT name] {PRIMARY KEY|FOREIGN KEY|CHECK} ... and returns it in the form
/// a CREATE TABLE accepts. A CHECK's expression is translated as CREATE TABLE's is,
/// and a NOT VALID one carries NOT_VALID_MARK.
fn parse_add_constraint(sql: &str) -> Option<AddedConstraint> {
    let t = sql.trim();
    let body = t.strip_suffix(';').unwrap_or(t).trim();
    let i = index_word_fold(body, " ADD ")?;
    let mut clause = body[i + " ADD ".len()..].trim().to_string();

    // The name is optional; keep it when given so a later dump reads the same.
    let mut name = String::new();
    {
        let toks = code_tokens(&clause);
        if toks.len() >= 2 && toks[0].is_word(&clause, "constraint") {
            name = clause[toks[1].span.clone()].to_string();
            clause = clause[toks[1].span.end..].trim().to_string();
        }
    }

    let up = clause.to_uppercase();
    // A key whose NULLs are not distinct is an index of its own: alter_add_unique.
    if up.starts_with("UNIQUE NULLS") {
        return None;
    }
    if up.starts_with("PRIMARY KEY") || up.starts_with("CHECK") || up.starts_with("UNIQUE") {
    } else if up.starts_with("FOREIGN KEY") {
        clause = without_public(&clause);
    } else {
        return None; // UNIQUE, EXCLUDE, anything else
    }

    // NOT VALID says "do not check the existing rows". A foreign key is checked as
    // the rebuild copies them anyway; a CHECK keeps the mark, which the copy reads.
    let mut not_valid = false;
    let toks = code_tokens(&clause);
    if let Some(j) = (1..toks.len())
        .find(|&i| toks[i].depth == 0 && toks[i - 1].is_word(&clause, "not") && toks[i].is_word(&clause, "valid"))
    {
        clause = clause[..toks[j - 1].span.start].trim().to_string();
        not_valid = true;
    }
    let check = up.starts_with("CHECK").then_some(not_valid);
    if check.is_some() {
        clause = translate_check(&clause);
        if not_valid {
            clause = clause + " " + NOT_VALID_MARK;
        }
    }
    if !name.is_empty() {
        clause = "CONSTRAINT ".to_string() + &name + " " + &clause;
    }
    // A key's deferral as the translation of a CREATE TABLE writes it (the comment
    // the catalog reads); a CHECK has been translated already.
    let clause = if check.is_some() {
        clause
    } else {
        translate_check(&clause)
    };
    Some(AddedConstraint { clause, check })
}

/// A table constraint in the SQLite form CREATE TABLE would give it: a CHECK's cast,
/// operator or literal of Postgres' own translated the same way, a key's deferral in
/// the comment the catalog reads.
pub(crate) fn translate_check(check: &str) -> String {
    let created = rewrite(&("CREATE TABLE _overlite_check (".to_string() + check + ")"));
    match (created.find('('), created.rfind(')')) {
        (Some(open), Some(close)) if open < close => created[open + 1..close].trim().to_string(),
        _ => check.to_string(),
    }
}

impl Session {
    /// Applies a table-level constraint by rebuilding the table with it in the
    /// CREATE TABLE, which is the only way SQLite takes one after the fact. The
    /// rebuild copies the rows, so an existing violation surfaces here as an error
    /// rather than as a constraint that silently does not hold.
    fn alter_add_constraint(&mut self, table: &str, added: &AddedConstraint) -> Result<()> {
        let ddl = self.table_ddl(table);
        if ddl.is_empty() {
            return Err(errf!(SqlState::UNDEFINED_TABLE, "unknown table {:?}", quoted(table)));
        }
        let Some(new_ddl) = insert_table_constraint(&ddl, &added.clause) else {
            return Err(errf!(
                SqlState::INTERNAL_ERROR,
                "cannot place constraint in DDL for {:?}",
                quoted(table)
            ));
        };
        if added.check == Some(false) {
            // The new one is the last CHECK of the list, as it is placed at its end.
            if let Some((name, expr, _)) = self.table_checks(table, &new_ddl)?.pop() {
                self.check_rows(table, &name, &expr)?;
            }
        }
        self.rebuild_table(table, &new_ddl)
    }

    /// The CHECK constraints of a table's DDL as the catalog reads them: name,
    /// expression and whether it is NOT VALID.
    fn table_checks(&mut self, table: &str, ddl: &str) -> Result<Vec<(String, String, bool)>> {
        let (_, relation) = split_stored_name(table);
        let rs = self.exec(
            &("SELECT json_extract(value, '$.name'), json_extract(value, '$.expr'), \
               CASE WHEN json_extract(value, '$.notvalid') THEN 't' ELSE 'f' END FROM json_each(overlite_checks("
                .to_string()
                + &quote_literal(ddl)
                + ", "
                + &quote_literal(&relation)
                + ")) ORDER BY key"),
            &[],
        )?;
        Ok(rs
            .rows
            .iter()
            .map(|r| (text_or_empty(&r[0]), text_or_empty(&r[1]), text_or_empty(&r[2]) == "t"))
            .collect())
    }

    /// Refuses, as Postgres does, when a row of table makes a CHECK's expression false.
    fn check_rows(&mut self, table: &str, name: &str, expr: &str) -> Result<()> {
        let probe = "SELECT 1 FROM ".to_string() + &quote_ident(table) + " WHERE NOT (" + expr + ") LIMIT 1";
        if self.exec(&probe, &[])?.rows.is_empty() {
            return Ok(());
        }
        let (_, relation) = split_stored_name(table);
        Err(errf!(
            SqlState::CHECK_VIOLATION,
            "check constraint {:?} of relation {:?} is violated by some row",
            quoted(name),
            quoted(&relation)
        ))
    }

    /// VALIDATE CONSTRAINT of a CHECK added NOT VALID: the rows already there are
    /// checked, and the mark goes. False when name is no such constraint, which
    /// Postgres' other kinds are here: they held for every row when they were added.
    fn validate_check(&mut self, table: &str, name: &str) -> Result<bool> {
        let ddl = self.table_ddl(table);
        let Some((_, expr, _)) = self
            .table_checks(table, &ddl)?
            .into_iter()
            .find(|(n, _, not_valid)| *not_valid && n == name)
        else {
            return Ok(false);
        };
        self.check_rows(table, name, &expr)?;
        let Some(at) = not_valid_mark_of(&ddl, name) else {
            return Ok(false);
        };
        let new_ddl = ddl[..at].trim_end().to_string() + &ddl[at + NOT_VALID_MARK.len()..];
        self.rebuild_table(table, &new_ddl)?;
        Ok(true)
    }
}

/// Where the NOT_VALID_MARK of the constraint named name starts in a table's DDL:
/// the first mark after `CONSTRAINT name`.
fn not_valid_mark_of(ddl: &str, name: &str) -> Option<usize> {
    let toks = code_tokens(ddl);
    let named = toks
        .windows(2)
        .find(|w| w[0].is_word(ddl, "constraint") && unquote_ident(&ddl[w[1].span.clone()]) == name)?;
    let from = named[1].span.end;
    ddl[from..].find(NOT_VALID_MARK).map(|i| from + i)
}

/// Puts clause at the end of a CREATE TABLE's column list, where SQLite reads it
/// as a table constraint. It walks to the paren that closes the list rather than
/// the last one in the string, so a DEFAULT (expr) or a trailing clause after the
/// list does not mislead it.
fn insert_table_constraint(ddl: &str, clause: &str) -> Option<String> {
    let open = ddl.find('(')?;
    let b = ddl.as_bytes();
    let mut depth = 0;
    let mut in_str = 0u8;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        if in_str != 0 {
            if c == in_str {
                in_str = 0;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => in_str = c,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(ddl[..i].to_string() + ", " + clause + &ddl[i..]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// --- ADD COLUMN with a computed default ---------------------------------------

// SQLite refuses `ALTER TABLE ... ADD COLUMN ... DEFAULT <expr>` for anything it
// cannot evaluate to a constant, because it would have to compute the value for
// every existing row. Postgres allows it, and migrations lean on it heavily
// (`ADD COLUMN created_at timestamptz DEFAULT now()`), so it is implemented the
// way SQLite's own documentation recommends changing a table: rebuild it with
// the column in the CREATE TABLE, where a computed default is accepted.

/// The column definition of an `ADD COLUMN ... DEFAULT <call>`, if the statement
/// is one.
fn add_column_computed_default(sql: &str) -> Option<String> {
    // ADD [COLUMN] [IF NOT EXISTS] def
    let sql = alter_target(sql)?.action;
    let toks = code_tokens(sql);
    let is_word = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.is_word(sql, w));
    if !is_word(0, "add") {
        return None;
    }
    let mut i = 1;
    if is_word(i, "column") {
        i += 1;
    }
    if is_word(i, "if") && is_word(i + 1, "not") && is_word(i + 2, "exists") {
        i += 3;
    }
    let r = sql[toks.get(i)?.span.start..].trim();
    let rest = r.strip_suffix(';').unwrap_or(r);

    // Only the computed form needs the rebuild; a constant default is fine as a
    // plain ADD COLUMN. This runs on the statement as the client sent it, so the
    // value is still `now()` rather than its SQLite spelling. So does a column that
    // is a key or an identity, or is stored generated, which SQLite's ADD COLUMN
    // refuses.
    let top = |w: &str| toks[i..].iter().any(|t| t.depth == 0 && t.is_word(sql, w));
    let computed = toks[i..]
        .iter()
        .find(|t| t.depth == 0 && t.is_word(sql, "default"))
        .is_some_and(|d| is_computed_default(sql[d.span.end..].trim()));
    let keyed = top("unique") || top("primary") || top("identity") || top("stored");
    if !computed && !keyed {
        return None;
    }
    let (name, _, _) = parse_col_def(rest);
    if name.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// Reports whether a DEFAULT value is an expression SQLite cannot store through
/// ADD COLUMN: a parenthesised expression or a function call. A literal or a bare
/// keyword such as CURRENT_TIMESTAMP is constant enough for SQLite to accept.
fn is_computed_default(value: &str) -> bool {
    if value.starts_with('(') {
        return true;
    }
    let b = value.as_bytes();
    let mut i = 0;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    if i == 0 {
        return false;
    }
    while i < b.len() && b[i] == b' ' {
        i += 1;
    }
    i < b.len() && b[i] == b'('
}

impl Session {
    /// Adds a column carrying a computed default by rebuilding the table with it
    /// in place.
    fn alter_add_column_default(&mut self, ref_: &str, def: &str) -> Result<()> {
        let (name, master, qualified) = self.resolve_table(ref_);
        let ddl = self.table_ddl_in(&master, &name);
        if ddl.is_empty() {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation {:?} does not exist",
                quoted(ref_)
            ));
        }
        let Some(open) = ddl.find('(') else {
            return Err(errf!(
                SqlState::INTERNAL_ERROR,
                "cannot read the definition of {:?}",
                quoted(ref_)
            ));
        };
        let (inner, after) = balanced_paren(&ddl, open);

        let cols = existing_column_names(&inner);
        if cols.is_empty() {
            return Err(errf!(
                SqlState::INTERNAL_ERROR,
                "cannot read the columns of {:?}",
                quoted(ref_)
            ));
        }
        // The definition still carries the client's spelling; the rebuild runs it
        // against the engine directly, so it needs the dialect rewrite applied.
        // Among the columns: SQLite takes no column after a table constraint.
        let mut items = split_top_level(&inner);
        let at = items
            .iter()
            .position(|d| parse_col_def(d).0.is_empty())
            .unwrap_or(items.len());
        // An identity column is made as CREATE TABLE makes one: a key that is the
        // rowid, which numbers the rows copied in, or a column with a sequence.
        let def = self
            .create_identity_columns(&["CREATE TABLE ", ref_, " (", def, ")"].concat())
            .into_owned();
        let def = def
            .find('(')
            .map_or(def.as_str(), |o| &def[o + 1..def.len() - 1])
            .to_string();
        // The column is translated as a CREATE TABLE's, a statement the translation
        // reads whole, and taken back out of it.
        let created = rewrite(&["CREATE TABLE t (", &def, ")"].concat());
        let Some(def) = created
            .strip_prefix("CREATE TABLE t (")
            .and_then(|c| c.strip_suffix(')'))
        else {
            return Err(errf!(
                SqlState::INTERNAL_ERROR,
                "cannot read the definition of the column added to {:?}",
                quoted(ref_)
            ));
        };
        items.insert(at, " ".to_string() + def + " ");
        let new_ddl = ddl[..=open].to_string() + &items.join(",") + slice(&ddl, after - 1, ddl.len());
        self.rebuild_table_copying(&qualified, &new_ddl, &cols, &master, &name)
    }
}

/// Lists the column names in a CREATE TABLE body, skipping table-level constraints.
fn existing_column_names(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    for d in split_top_level(inner) {
        let (name, _, _) = parse_col_def(&d);
        if name.is_empty() {
            continue;
        }
        match name.to_uppercase().as_str() {
            "PRIMARY" | "FOREIGN" | "UNIQUE" | "CHECK" | "CONSTRAINT" | "EXCLUDE" => continue,
            _ => {}
        }
        out.push(unquote_ident(&name));
    }
    out
}

impl Session {
    /// Asks the engine how the storage names a table reference: (name, master,
    /// qualified).
    fn resolve_table(&mut self, ref_: &str) -> (String, String, String) {
        self.db.resolve_table(ref_)
    }
}

/// Places the scratch table beside the one being rebuilt.
///
/// It has to land in the same database: in multi-file mode a schema is an attached
/// one, and building the replacement in main would leave the rebuilt table in
/// public, out of the schema it started in.
fn temp_rebuild_name(qualified_table: &str) -> String {
    const TMP: &str = "_overlite_rebuild";
    // A dot inside quotes is part of a single stored name (single-file mode), not
    // an attached-database qualifier.
    let (schema, _) = split_qualified_ref(qualified_table);
    if !schema.is_empty() {
        return quote_ident(&schema) + "." + &quote_ident(TMP);
    }
    quote_ident(TMP)
}

impl Session {
    /// Reads a table's CREATE statement from a given sqlite_master.
    fn table_ddl_in(&mut self, master: &str, name: &str) -> String {
        let rs = self.exec(
            &("SELECT sql FROM ".to_string()
                + master
                + " WHERE type='table' AND lower(name)=lower("
                + &quote_literal(name)
                + ")"),
            &[],
        );
        match rs {
            Ok(rs) if !rs.rows.is_empty() => text_or_empty(&rs.rows[0][0]),
            _ => String::new(),
        }
    }

    /// Reads a table's indexes and triggers from a given sqlite_master.
    fn aux_ddl_in(&mut self, master: &str, name: &str) -> Vec<String> {
        let Ok(rs) = self.exec(
            &("SELECT sql FROM ".to_string()
                + master
                + " WHERE lower(tbl_name)=lower("
                + &quote_literal(name)
                + ") AND type IN ('index','trigger') AND sql IS NOT NULL"),
            &[],
        ) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for row in &rs.rows {
            let d = text_or_empty(&row[0]);
            if !d.is_empty() {
                out.push(d);
            }
        }
        out
    }
}

/// Strips the quoting from each part of a possibly qualified table reference,
/// leaving `schema.table` or `table`.
fn unquote_ref(ref_: &str) -> String {
    let (schema, table) = split_qualified_ref(ref_);
    if schema.is_empty() {
        return table;
    }
    schema + "." + &table
}

/// Splits `"schema"."table"` (in any combination of quoting) into its unquoted
/// parts.
pub(crate) fn split_qualified_ref(ref_: &str) -> (String, String) {
    let b = ref_.as_bytes();
    let mut depth = 0;
    for i in 0..b.len() {
        match b[i] {
            b'"' => depth ^= 1,
            b'.' if depth == 0 => {
                return (unquote_ident(&ref_[..i]), unquote_ident(&ref_[i + 1..]));
            }
            _ => {}
        }
    }
    (String::new(), unquote_ident(ref_))
}

// Captures the name of an index or trigger in its CREATE.
lazy_regex!(
    RE_AUX_OBJECT_NAME,
    r#"(?is)^(\s*CREATE\s+(?:UNIQUE\s+)?(?:INDEX|TRIGGER)\s+(?:IF\s+NOT\s+EXISTS\s+)?)("[^"]+"|[\w.]+)"#
);

/// Puts an index or trigger back in the schema it came from.
///
/// A schema is an attached database in multi-file mode, and SQLite names those
/// objects by qualifying the object rather than the table. sqlite_master stores
/// the statement without that qualifier, so replaying it verbatim would rebuild the
/// index in main, against a table that is not there.
fn qualify_aux_ddl(stmts: Vec<String>, schema: &str) -> Vec<String> {
    if schema.is_empty() {
        return stmts;
    }
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        let replaced = match RE_AUX_OBJECT_NAME.captures(&stmt) {
            None => None,
            Some(m) => {
                let name = unquote_ident(&m[2]);
                Some(m[1].to_string() + &quote_ident(schema) + "." + &quote_ident(&name) + &stmt[m.get_match().end()..])
            }
        };
        out.push(replaced.unwrap_or(stmt));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn computed_default_is_the_clause() {
        for sql in [
            "ALTER TABLE t ADD COLUMN note text CHECK (note <> 'default (1)')",
            "ALTER TABLE t ADD COLUMN \"default\" text",
            "ALTER TABLE t ADD COLUMN a int DEFAULT 1",
            "ALTER TABLE t DROP COLUMN a",
        ] {
            assert_eq!(add_column_computed_default(sql), None, "{sql}");
        }
        for (sql, want) in [
            (
                "ALTER TABLE t ADD a text DEFAULT lower('X')",
                "a text DEFAULT lower('X')",
            ),
            (
                "ALTER TABLE IF EXISTS ONLY s.t ADD COLUMN IF NOT EXISTS \"default\" text DEFAULT (1);",
                "\"default\" text DEFAULT (1)",
            ),
            (
                "ALTER TABLE \"add\" ADD COLUMN a int DEFAULT now()",
                "a int DEFAULT now()",
            ),
        ] {
            assert_eq!(add_column_computed_default(sql).as_deref(), Some(want), "{sql}");
        }
    }
}
