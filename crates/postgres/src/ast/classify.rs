//! The session's classifiers on the tree (docs/adr/0001-sql-ast.md, stage 5b): which
//! route a statement takes, transaction control, a schema's, a role's or a sequence's
//! DDL, GRANT, SET ROLE..., read from the statement's node instead of its first words.
//!
//! Each classifier of the session asks `read` with what it looks for in a node, and
//! reads the text as before only where Postgres' grammar does not read the statement
//! (a statement of SQLite's own dialect, which the session still serves, or one
//! Postgres refuses). A statement that reads or writes rows is none of these, and is
//! not parsed for them: the translation parses it, once (`parse::shared`).

use crate::session::utility::{SavepointKind, TxControl};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, ObjectType, TransactionStmtKind, VariableSetKind};

/// What f reads in sql's node: None when Postgres' grammar does not read sql as one
/// statement, which leaves it to the text classifier. A statement that reads or
/// writes rows is T's default, without a parse.
pub(crate) fn read<T: Default>(sql: &str, f: impl FnOnce(&NodeEnum) -> T) -> Option<T> {
    if crate::rewrite::cache::reads_or_writes_rows(sql) {
        return Some(T::default());
    }
    let parsed = crate::ast::parse::shared(sql).ok()?;
    let [raw] = parsed.stmts.as_slice() else {
        return None;
    };
    raw.stmt.as_deref().and_then(|s| s.node.as_ref()).map(f)
}

fn object(t: i32) -> ObjectType {
    ObjectType::try_from(t).unwrap_or(ObjectType::Undefined)
}

fn transaction(n: &NodeEnum) -> Option<TransactionStmtKind> {
    match n {
        NodeEnum::TransactionStmt(t) => TransactionStmtKind::try_from(t.kind).ok(),
        _ => None,
    }
}

/// BEGIN, COMMIT, ROLLBACK and their other spellings. COMMIT PREPARED and ROLLBACK
/// PREPARED end the transaction as COMMIT and ROLLBACK do; PREPARE TRANSACTION is none.
pub(crate) fn tx_control(n: &NodeEnum) -> Option<TxControl> {
    use TransactionStmtKind as K;
    match transaction(n)? {
        K::TransStmtBegin | K::TransStmtStart => Some(TxControl::Begin),
        K::TransStmtCommit | K::TransStmtCommitPrepared => Some(TxControl::Commit),
        K::TransStmtRollback | K::TransStmtRollbackPrepared => Some(TxControl::Rollback),
        _ => None,
    }
}

/// SAVEPOINT, RELEASE and ROLLBACK TO.
pub(crate) fn savepoint(n: &NodeEnum) -> Option<SavepointKind> {
    use TransactionStmtKind as K;
    match transaction(n)? {
        K::TransStmtSavepoint => Some(SavepointKind::Savepoint),
        K::TransStmtRelease => Some(SavepointKind::Release),
        K::TransStmtRollbackTo => Some(SavepointKind::RollbackTo),
        _ => None,
    }
}

/// DECLARE, FETCH, MOVE and CLOSE.
pub(crate) fn cursor(n: &NodeEnum) -> bool {
    matches!(
        n,
        NodeEnum::DeclareCursorStmt(_) | NodeEnum::FetchStmt(_) | NodeEnum::ClosePortalStmt(_)
    )
}

/// CREATE, DROP and ALTER SCHEMA.
pub(crate) fn schema_ddl(n: &NodeEnum) -> bool {
    match n {
        NodeEnum::CreateSchemaStmt(_) => true,
        NodeEnum::DropStmt(d) => object(d.remove_type) == ObjectType::ObjectSchema,
        NodeEnum::RenameStmt(r) => object(r.rename_type) == ObjectType::ObjectSchema,
        NodeEnum::AlterOwnerStmt(a) => object(a.object_type) == ObjectType::ObjectSchema,
        _ => false,
    }
}

/// CREATE, ALTER and DROP ROLE, USER and GROUP (not USER MAPPING).
pub(crate) fn role_ddl(n: &NodeEnum) -> bool {
    match n {
        NodeEnum::CreateRoleStmt(_)
        | NodeEnum::AlterRoleStmt(_)
        | NodeEnum::AlterRoleSetStmt(_)
        | NodeEnum::DropRoleStmt(_) => true,
        NodeEnum::RenameStmt(r) => object(r.rename_type) == ObjectType::ObjectRole,
        _ => false,
    }
}

fn routine(t: i32) -> bool {
    matches!(
        object(t),
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectRoutine
    )
}

/// GRANT and REVOKE, of privileges or roles, and ALTER of a function, procedure or
/// routine, whose owner decides what its SECURITY DEFINER body reads.
pub(crate) fn grant(n: &NodeEnum) -> bool {
    match n {
        NodeEnum::GrantStmt(_) | NodeEnum::GrantRoleStmt(_) | NodeEnum::AlterFunctionStmt(_) => true,
        NodeEnum::AlterOwnerStmt(a) => routine(a.object_type),
        NodeEnum::RenameStmt(r) => routine(r.rename_type),
        NodeEnum::AlterObjectSchemaStmt(a) => routine(a.object_type),
        _ => false,
    }
}

/// CREATE, ALTER and DROP POLICY, and an ALTER TABLE that enables, disables, forces or
/// stops forcing row-level security.
pub(crate) fn rls_ddl(n: &NodeEnum) -> bool {
    use pb::AlterTableType as A;
    match n {
        NodeEnum::CreatePolicyStmt(_) | NodeEnum::AlterPolicyStmt(_) => true,
        NodeEnum::DropStmt(d) => object(d.remove_type) == ObjectType::ObjectPolicy,
        NodeEnum::RenameStmt(r) => object(r.rename_type) == ObjectType::ObjectPolicy,
        NodeEnum::AlterTableStmt(a) if object(a.objtype) == ObjectType::ObjectTable => a.cmds.iter().any(|c| {
            matches!(&c.node, Some(NodeEnum::AlterTableCmd(c)) if matches!(
                A::try_from(c.subtype),
                Ok(A::AtEnableRowSecurity | A::AtDisableRowSecurity | A::AtForceRowSecurity | A::AtNoForceRowSecurity)
            ))
        }),
        _ => false,
    }
}

/// LISTEN, UNLISTEN and NOTIFY.
pub(crate) fn listen_notify(n: &NodeEnum) -> bool {
    matches!(
        n,
        NodeEnum::ListenStmt(_) | NodeEnum::UnlistenStmt(_) | NodeEnum::NotifyStmt(_)
    )
}

/// CREATE, ALTER and DROP SEQUENCE, and TRUNCATE (which restarts a table's
/// sequences). A temporary sequence is not one: the session's are its database's.
pub(crate) fn sequence_ddl(n: &NodeEnum) -> bool {
    let sequence = |t: i32| object(t) == ObjectType::ObjectSequence;
    match n {
        NodeEnum::CreateSeqStmt(c) => c.sequence.as_ref().is_none_or(|r| r.relpersistence != "t"),
        NodeEnum::AlterSeqStmt(_) | NodeEnum::TruncateStmt(_) => true,
        NodeEnum::DropStmt(d) => sequence(d.remove_type),
        NodeEnum::AlterTableStmt(a) => sequence(a.objtype),
        NodeEnum::RenameStmt(r) => sequence(r.rename_type),
        NodeEnum::AlterObjectSchemaStmt(a) => sequence(a.object_type),
        NodeEnum::AlterOwnerStmt(a) => sequence(a.object_type),
        _ => false,
    }
}

/// CREATE, ALTER and DROP TYPE, and the objects of ddl/objects.rs: extensions,
/// collations, foreign data, materialized views and partitioned tables.
pub(crate) fn type_ddl(n: &NodeEnum) -> bool {
    use ObjectType as O;
    let objects = |t: i32| {
        matches!(
            object(t),
            O::ObjectType
                | O::ObjectExtension
                | O::ObjectCollation
                | O::ObjectForeignServer
                | O::ObjectForeignTable
                | O::ObjectFdw
                | O::ObjectMatview
        )
    };
    match n {
        NodeEnum::CreateEnumStmt(_)
        | NodeEnum::AlterEnumStmt(_)
        | NodeEnum::CompositeTypeStmt(_)
        | NodeEnum::CreateRangeStmt(_)
        | NodeEnum::AlterTypeStmt(_)
        | NodeEnum::CreateExtensionStmt(_)
        | NodeEnum::AlterExtensionStmt(_)
        | NodeEnum::AlterExtensionContentsStmt(_)
        | NodeEnum::AlterCollationStmt(_)
        | NodeEnum::CreateFdwStmt(_)
        | NodeEnum::AlterFdwStmt(_)
        | NodeEnum::CreateForeignServerStmt(_)
        | NodeEnum::AlterForeignServerStmt(_)
        | NodeEnum::CreateForeignTableStmt(_)
        | NodeEnum::CreateUserMappingStmt(_)
        | NodeEnum::AlterUserMappingStmt(_)
        | NodeEnum::DropUserMappingStmt(_)
        | NodeEnum::ImportForeignSchemaStmt(_)
        | NodeEnum::RefreshMatViewStmt(_) => true,
        NodeEnum::DefineStmt(d) => matches!(object(d.kind), O::ObjectType | O::ObjectCollation),
        NodeEnum::DropStmt(d) => objects(d.remove_type),
        NodeEnum::RenameStmt(r) => objects(r.rename_type),
        NodeEnum::AlterOwnerStmt(a) => objects(a.object_type),
        NodeEnum::AlterObjectSchemaStmt(a) => objects(a.object_type),
        NodeEnum::AlterTableStmt(a) => matches!(
            object(a.objtype),
            O::ObjectType | O::ObjectMatview | O::ObjectForeignTable
        ),
        NodeEnum::CreateTableAsStmt(c) => object(c.objtype) == O::ObjectMatview,
        NodeEnum::CreateStmt(c) => c.partspec.is_some() || c.partbound.is_some(),
        _ => false,
    }
}

/// COMMENT ON.
pub(crate) fn comment(n: &NodeEnum) -> bool {
    matches!(n, NodeEnum::CommentStmt(_))
}

/// What a SET or RESET of the role or the session's authorization asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RoleChange {
    /// SESSION AUTHORIZATION, where it is not ROLE.
    pub session_authorization: bool,
    /// RESET, or SET to DEFAULT or NONE: back to the session's own.
    pub reset: bool,
    /// SET LOCAL: until the transaction ends.
    pub local: bool,
    /// The role asked for, as Postgres reads its name (folded unless quoted).
    pub target: String,
}

/// SET [SESSION | LOCAL] ROLE and SESSION AUTHORIZATION, and their RESET.
pub(crate) fn set_role(n: &NodeEnum) -> Option<RoleChange> {
    let NodeEnum::VariableSetStmt(v) = n else {
        return None;
    };
    let session_authorization = match v.name.as_str() {
        "role" => false,
        "session_authorization" => true,
        _ => return None,
    };
    let target = match v.args.as_slice() {
        [a] => match &a.node {
            Some(NodeEnum::AConst(k)) => match &k.val {
                Some(pb::a_const::Val::Sval(s)) => s.sval.clone(),
                _ => return None,
            },
            _ => return None,
        },
        _ => String::new(),
    };
    let kind = VariableSetKind::try_from(v.kind).unwrap_or(VariableSetKind::Undefined);
    let reset = matches!(kind, VariableSetKind::VarReset | VariableSetKind::VarSetDefault)
        || (!session_authorization && target == "none");
    Some(RoleChange {
        session_authorization,
        reset,
        local: v.is_local,
        target,
    })
}

/// COPY.
pub(crate) fn copy(n: &NodeEnum) -> bool {
    matches!(n, NodeEnum::CopyStmt(_))
}

/// SET, RESET or SHOW of the setting name.
fn setting(n: &NodeEnum, name: &str) -> bool {
    match n {
        NodeEnum::VariableSetStmt(v) => v.name == name,
        NodeEnum::VariableShowStmt(v) => v.name == name,
        _ => false,
    }
}

/// SET [SESSION | LOCAL] TIME ZONE and RESET of it.
pub(crate) fn time_zone(n: &NodeEnum) -> bool {
    matches!(n, NodeEnum::VariableSetStmt(_)) && setting(n, "timezone")
}

/// SET, RESET and SHOW of search_path.
pub(crate) fn search_path(n: &NodeEnum) -> bool {
    setting(n, "search_path")
}

/// EXPLAIN.
pub(crate) fn explain(n: &NodeEnum) -> bool {
    matches!(n, NodeEnum::ExplainStmt(_))
}

/// CREATE, DROP and ALTER DATABASE.
pub(crate) fn database_ddl(n: &NodeEnum) -> bool {
    match n {
        NodeEnum::CreatedbStmt(_)
        | NodeEnum::DropdbStmt(_)
        | NodeEnum::AlterDatabaseStmt(_)
        | NodeEnum::AlterDatabaseSetStmt(_)
        | NodeEnum::AlterDatabaseRefreshCollStmt(_) => true,
        NodeEnum::RenameStmt(r) => object(r.rename_type) == ObjectType::ObjectDatabase,
        NodeEnum::AlterOwnerStmt(a) => object(a.object_type) == ObjectType::ObjectDatabase,
        _ => false,
    }
}

/// A CREATE TABLE with an EXCLUDE constraint, which ddl/exclude.rs makes.
pub(crate) fn creates_exclusions(n: &NodeEnum) -> bool {
    let NodeEnum::CreateStmt(c) = n else {
        return false;
    };
    c.table_elts.iter().any(
        |e| matches!(&e.node, Some(NodeEnum::Constraint(k)) if k.contype == pb::ConstrType::ConstrExclusion as i32),
    )
}
