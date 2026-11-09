//! What the session's handlers read of their statement's node: names as Postgres reads
//! them (folded unless quoted), a role, an option's value. The classifiers have
//! already parsed the statement (`ast::parse::shared` keeps the parse on the thread),
//! so a handler reading it here pays no second parse.

use crate::error::{Error, Result};
use overlite_core::SqlState;
use overlite_core::text::quote_ident;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// f over sql's one statement. A statement Postgres' grammar does not read, or more
/// than one, is a syntax error: the session answers a parse error before any handler
/// is asked.
///
/// # Errors
///
/// 42601 for such a statement; what f answers otherwise.
pub(crate) fn statement<T>(sql: &str, f: impl FnOnce(&NodeEnum) -> Result<T>) -> Result<T> {
    let parsed = crate::ast::parse::shared(sql).map_err(|_| parse_failure(sql))?;
    let [raw] = parsed.stmts.as_slice() else {
        return Err(parse_failure(sql));
    };
    match raw.stmt.as_deref().and_then(|s| s.node.as_ref()) {
        Some(n) => f(n),
        None => Err(parse_failure(sql)),
    }
}

/// What Postgres answers a statement its grammar refuses: its parser's message, with
/// 42939 for a reserved role name (which the grammar itself refuses) and 42601 for
/// the rest.
pub(crate) fn parse_failure(sql: &str) -> Error {
    match pg_query::parse(sql, 0) {
        Err(pg_query::Error::Parse(m)) if m.contains("is reserved") => Error::with_state(SqlState::RESERVED_NAME, m),
        Err(pg_query::Error::Parse(m)) => Error::with_state(SqlState::SYNTAX_ERROR, m),
        _ => syntax_error(),
    }
}

/// What f reads of sql's one statement, None for anything Postgres' grammar does not
/// read as one statement.
pub(crate) fn read<T>(sql: &str, f: impl FnOnce(&NodeEnum) -> Option<T>) -> Option<T> {
    let parsed = crate::ast::parse::shared(sql).ok()?;
    let [raw] = parsed.stmts.as_slice() else {
        return None;
    };
    raw.stmt.as_deref().and_then(|s| s.node.as_ref()).and_then(f)
}

pub(crate) fn syntax_error() -> Error {
    Error::with_state(SqlState::SYNTAX_ERROR, "syntax error")
}

/// The text of a String node.
pub(crate) fn string(n: &pb::Node) -> Option<&str> {
    match &n.node {
        Some(NodeEnum::String(s)) => Some(&s.sval),
        _ => None,
    }
}

/// The String nodes of a list, in order (a qualified name's parts).
pub(crate) fn strings(list: &[pb::Node]) -> Vec<String> {
    list.iter().filter_map(string).map(str::to_string).collect()
}

/// A relation as the reference the session's name helpers read
/// (`split_qualified_ref`): each part quoted, so it reads back as the tree has it.
pub(crate) fn reference(r: &pb::RangeVar) -> String {
    if r.schemaname.is_empty() {
        quote_ident(&r.relname)
    } else {
        quote_ident(&r.schemaname) + "." + &quote_ident(&r.relname)
    }
}

/// A qualified name's parts (`[schema, name]` or `[name]`) as such a reference.
pub(crate) fn reference_of(parts: &[String]) -> String {
    parts.iter().map(|p| quote_ident(p)).collect::<Vec<_>>().join(".")
}

/// The relations of a list of RangeVar nodes, as references.
pub(crate) fn references(list: &[pb::Node]) -> Vec<String> {
    list.iter()
        .filter_map(|n| match &n.node {
            Some(NodeEnum::RangeVar(r)) => Some(reference(r)),
            _ => None,
        })
        .collect()
}

/// Who a role specification names, given the session's current and session roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Role {
    Named(String),
    /// CURRENT_ROLE or CURRENT_USER.
    Current,
    /// SESSION_USER.
    Session,
    /// PUBLIC.
    Public,
}

pub(crate) fn role(r: &pb::RoleSpec) -> Role {
    use pb::RoleSpecType as T;
    match T::try_from(r.roletype).unwrap_or(T::Undefined) {
        T::RolespecCurrentRole | T::RolespecCurrentUser => Role::Current,
        T::RolespecSessionUser => Role::Session,
        T::RolespecPublic => Role::Public,
        _ => Role::Named(r.rolename.clone()),
    }
}

/// The RoleSpec of a node, if it is one.
pub(crate) fn role_spec(n: &pb::Node) -> Option<&pb::RoleSpec> {
    match &n.node {
        Some(NodeEnum::RoleSpec(r)) => Some(r),
        _ => None,
    }
}

/// The DefElem nodes of a list.
pub(crate) fn defs(list: &[pb::Node]) -> impl Iterator<Item = &pb::DefElem> {
    list.iter().filter_map(|n| match &n.node {
        Some(NodeEnum::DefElem(d)) => Some(&**d),
        _ => None,
    })
}

/// An option's argument as text: a string, a name, a number as written.
pub(crate) fn def_text(d: &pb::DefElem) -> Option<String> {
    arg_text(d.arg.as_deref()?)
}

/// A constant's or a name's text.
pub(crate) fn arg_text(n: &pb::Node) -> Option<String> {
    use pb::a_const::Val;
    match &n.node {
        Some(NodeEnum::String(s)) => Some(s.sval.clone()),
        Some(NodeEnum::Integer(i)) => Some(i.ival.to_string()),
        Some(NodeEnum::Float(f)) => Some(f.fval.clone()),
        Some(NodeEnum::Boolean(b)) => Some(if b.boolval { "true" } else { "false" }.to_string()),
        Some(NodeEnum::TypeName(t)) => strings(&t.names).last().cloned(),
        Some(NodeEnum::AConst(c)) => match &c.val {
            Some(Val::Sval(s)) => Some(s.sval.clone()),
            Some(Val::Ival(i)) => Some(i.ival.to_string()),
            Some(Val::Fval(f)) => Some(f.fval.clone()),
            Some(Val::Boolval(b)) => Some(if b.boolval { "true" } else { "false" }.to_string()),
            _ => None,
        },
        Some(NodeEnum::List(l)) => Some(strings(&l.items).join(".")),
        _ => None,
    }
}

/// An option's argument as a boolean: a bare option is true.
pub(crate) fn def_bool(d: &pb::DefElem) -> bool {
    match d.arg.as_deref().map(|n| &n.node) {
        None => true,
        Some(Some(NodeEnum::Boolean(b))) => b.boolval,
        Some(Some(NodeEnum::Integer(i))) => i.ival != 0,
        Some(_) => def_text(d).is_some_and(|t| matches!(t.to_ascii_lowercase().as_str(), "true" | "on" | "1" | "yes")),
    }
}

/// The ObjectType of a raw enum value.
pub(crate) fn object(t: i32) -> pb::ObjectType {
    pb::ObjectType::try_from(t).unwrap_or(pb::ObjectType::Undefined)
}

/// The names sql writes as quoted identifiers, as they read: the tree folds an
/// unquoted name and keeps a quoted one, and says no more of which it was, which a
/// reader matching a name in any case where Postgres would fold it needs.
pub(crate) fn quoted_names(sql: &str) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let Ok(tokens) = pg_query::scan_tokens(sql) else {
        return out;
    };
    for t in tokens {
        let (Ok(start), Ok(end)) = (usize::try_from(t.start), usize::try_from(t.end)) else {
            continue;
        };
        if let Some(q) = sql
            .get(start..end)
            .and_then(|s| s.strip_prefix('"'))
            .and_then(|s| s.strip_suffix('"'))
        {
            out.insert(q.replace("\"\"", "\""));
        }
    }
    out
}
