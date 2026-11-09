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

/// One token of a statement, by Postgres' scanner: where it is, how deep in
/// parentheses, and whether the scanner read it as a keyword (a quoted name never is).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Token {
    pub start: usize,
    pub end: usize,
    pub depth: u32,
    pub keyword: bool,
}

/// sql's tokens, comments left out: where a clause of the tree begins and ends in the
/// client's text, for what is kept as written (a policy's expression, a default).
pub(crate) fn tokens(sql: &str) -> Vec<Token> {
    let Ok(scanned) = pg_query::scan_tokens(sql) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(scanned.len());
    let mut depth = 0u32;
    for t in scanned {
        let (Ok(start), Ok(end)) = (usize::try_from(t.start), usize::try_from(t.end)) else {
            continue;
        };
        let Some(text) = sql.get(start..end) else { continue };
        if text.starts_with("/*") || text.starts_with("--") {
            continue;
        }
        let keyword = t.keyword_kind != 0 && !text.starts_with('"');
        if text == ")" || text == "]" {
            depth = depth.saturating_sub(1);
        }
        out.push(Token {
            start,
            end,
            depth,
            keyword,
        });
        if text == "(" || text == "[" {
            depth += 1;
        }
    }
    out
}

/// The text inside the parentheses that follow the keywords words (in order, at the
/// top level of the statement), trimmed: `USING (expr)`'s expr.
pub(crate) fn parenthesized_after<'a>(sql: &'a str, toks: &[Token], words: &[&str]) -> Option<&'a str> {
    let text = |t: &Token| &sql[t.start..t.end];
    for i in 0..toks.len() {
        let matches = words.iter().enumerate().all(|(k, w)| {
            toks.get(i + k)
                .is_some_and(|t| t.depth == 0 && t.keyword && text(t).eq_ignore_ascii_case(w))
        });
        if !matches {
            continue;
        }
        let open = toks.get(i + words.len()).filter(|t| text(t) == "(")?;
        let close = toks[i + words.len() + 1..]
            .iter()
            .find(|t| t.depth == open.depth && text(t) == ")")?;
        return Some(sql[open.end..close.start].trim());
    }
    None
}

/// What a scanned token is to the handlers that cut a statement's text: a word (a
/// name or keyword written bare, or a number), a value (a literal or a quoted name),
/// or punctuation (an operator, a parenthesis, a comma).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodeTokKind {
    Word,
    Value,
    Punct,
}

/// A token of Postgres' scanner, for the handlers that keep part of a statement as
/// the client wrote it (an expression, a column's definition): where it is, what kind
/// it is, and how deep in parentheses (one of its own counts as outside).
#[derive(Debug, Clone)]
pub(crate) struct CodeTok {
    pub(crate) span: std::ops::Range<usize>,
    pub(crate) kind: CodeTokKind,
    pub(crate) depth: i32,
}

impl CodeTok {
    pub(crate) fn is_word(&self, sql: &str, w: &str) -> bool {
        self.kind == CodeTokKind::Word && sql[self.span.clone()].eq_ignore_ascii_case(w)
    }

    pub(crate) fn is_punct(&self, sql: &str, p: &str) -> bool {
        self.kind == CodeTokKind::Punct && &sql[self.span.clone()] == p
    }
}

/// sql's tokens by Postgres' scanner, comments left out; none for a text it does not
/// scan (an unterminated literal), which no handler is given.
pub(crate) fn code_tokens(sql: &str) -> Vec<CodeTok> {
    // Depth counts parentheses alone, as the readers of these tokens expect.
    let mut depth = 0i32;
    tokens(sql)
        .into_iter()
        .map(|t| {
            let text = &sql[t.start..t.end];
            if text == ")" {
                depth -= 1;
            }
            let here = depth;
            if text == "(" {
                depth += 1;
            }
            let first = text.chars().next().unwrap_or(' ');
            let kind = if first == '"'
                || first == '\''
                || first == '$' && text.len() > 1 && !text[1..].starts_with(|c: char| c.is_ascii_digit())
                || (matches!(first, 'e' | 'E' | 'b' | 'B' | 'x' | 'X' | 'n' | 'N' | 'u' | 'U')
                    && text[1..].starts_with(['\'', '&']))
            {
                CodeTokKind::Value
            } else if first.is_alphanumeric() || first == '_' || first == '$' || !first.is_ascii() {
                CodeTokKind::Word
            } else {
                CodeTokKind::Punct
            };
            CodeTok {
                span: t.start..t.end,
                kind,
                depth: here,
            }
        })
        .collect()
}
