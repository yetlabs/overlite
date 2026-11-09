//! What Postgres calls a result column the statement does not name: FigureColname of
//! its parser (parse_target.c), on the same tree, so there is nothing to read back
//! from text.

use pg_query::NodeEnum;
use pg_query::protobuf::{SubLinkType, a_const::Val};

/// The label an unnamed result column takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Label {
    /// SQLite already gives the column its Postgres name: a column or a star.
    Itself,
    /// To be written as `AS "label"`.
    Named(String),
}

/// One label per item of a statement's result list, None where the item is named
/// already (an AS of its own) or names itself (a column, a star): the outermost select
/// list (a set operation's first arm, whose names Postgres gives the result), or a
/// write's RETURNING. None for a statement without such a list.
pub(crate) fn result_labels(stmt: &NodeEnum) -> Option<Vec<Option<String>>> {
    use pg_query::protobuf::SetOperation;
    let items = match stmt {
        NodeEnum::SelectStmt(s) => {
            let mut s = s.as_ref();
            while s.op != SetOperation::SetopNone as i32 {
                s = s.larg.as_deref()?;
            }
            &s.target_list
        }
        NodeEnum::InsertStmt(s) => &s.returning_clause.as_ref()?.exprs,
        NodeEnum::UpdateStmt(s) => &s.returning_clause.as_ref()?.exprs,
        NodeEnum::DeleteStmt(s) => &s.returning_clause.as_ref()?.exprs,
        _ => return None,
    };
    if items.is_empty() {
        return None;
    }
    items
        .iter()
        .map(|t| match t.node.as_ref()? {
            NodeEnum::ResTarget(t) if !t.name.is_empty() => Some(None),
            NodeEnum::ResTarget(t) => match figure_colname(t.val.as_deref()?.node.as_ref()?) {
                Label::Itself => Some(None),
                Label::Named(l) => Some(Some(l)),
            },
            _ => None,
        })
        .collect()
}

/// The label of an unnamed select-list item.
pub(crate) fn figure_colname(n: &NodeEnum) -> Label {
    match n {
        NodeEnum::ColumnRef(_) => Label::Itself,
        _ => Label::Named(figure(n).map_or_else(|| "?column?".to_string(), |(name, _)| name)),
    }
}

/// FigureColnameInternal: a name and its strength, 2 for one the expression states
/// (a column, a function), 1 for one it only suggests (a cast's type, CASE).
fn figure(n: &NodeEnum) -> Option<(String, u8)> {
    match n {
        NodeEnum::ColumnRef(c) => match c.fields.last()?.node.as_ref()? {
            NodeEnum::String(s) => Some((s.sval.clone(), 2)),
            _ => None,
        },
        NodeEnum::FuncCall(f) => match f.funcname.last()?.node.as_ref()? {
            NodeEnum::String(s) => Some((s.sval.clone(), 2)),
            _ => None,
        },
        NodeEnum::AExpr(a) if a.kind == pg_query::protobuf::AExprKind::AexprNullif as i32 => {
            Some(("nullif".to_string(), 2))
        }
        NodeEnum::TypeCast(t) => {
            let inner = t.arg.as_deref().and_then(|a| a.node.as_ref()).and_then(figure);
            match inner {
                Some(x) if x.1 >= 2 => Some(x),
                _ => match t.type_name.as_ref()?.names.last()?.node.as_ref()? {
                    NodeEnum::String(s) => Some((s.sval.clone(), 1)),
                    _ => None,
                },
            }
        }
        NodeEnum::CaseExpr(c) => {
            let inner = c.defresult.as_deref().and_then(|a| a.node.as_ref()).and_then(figure);
            match inner {
                Some(x) if x.1 >= 2 => Some(x),
                _ => Some(("case".to_string(), 1)),
            }
        }
        NodeEnum::CoalesceExpr(_) => Some(("coalesce".to_string(), 2)),
        NodeEnum::MinMaxExpr(m) => Some((if m.op == 1 { "greatest" } else { "least" }.to_string(), 2)),
        NodeEnum::SubLink(s) => match SubLinkType::try_from(s.sub_link_type).ok()? {
            SubLinkType::ExistsSublink => Some(("exists".to_string(), 2)),
            SubLinkType::ArraySublink => Some(("array".to_string(), 2)),
            SubLinkType::ExprSublink | SubLinkType::MultiexprSublink => {
                // The name of the subquery's single column, if it has one.
                let NodeEnum::SelectStmt(q) = s.subselect.as_deref()?.node.as_ref()? else {
                    return None;
                };
                let NodeEnum::ResTarget(t) = q.target_list.first()?.node.as_ref()? else {
                    return None;
                };
                if t.name.is_empty() {
                    t.val.as_deref().and_then(|v| v.node.as_ref()).and_then(figure)
                } else {
                    Some((t.name.clone(), 2))
                }
            }
            _ => None,
        },
        NodeEnum::AArrayExpr(_) => Some(("array".to_string(), 2)),
        NodeEnum::RowExpr(_) => Some(("row".to_string(), 2)),
        NodeEnum::SqlvalueFunction(f) => Some((sql_value_name(f.op)?.to_string(), 2)),
        NodeEnum::AConst(k) => match &k.val {
            // A boolean literal is a cast of a string to bool in Postgres' grammar.
            Some(Val::Boolval(_)) => Some(("bool".to_string(), 1)),
            _ => None,
        },
        _ => None,
    }
}

/// The name of a niladic SQL function, by its SVFOP number.
fn sql_value_name(op: i32) -> Option<&'static str> {
    use pg_query::protobuf::SqlValueFunctionOp as O;
    Some(match O::try_from(op).ok()? {
        O::SvfopCurrentDate => "current_date",
        O::SvfopCurrentTime | O::SvfopCurrentTimeN => "current_time",
        O::SvfopCurrentTimestamp | O::SvfopCurrentTimestampN => "current_timestamp",
        O::SvfopLocaltime | O::SvfopLocaltimeN => "localtime",
        O::SvfopLocaltimestamp | O::SvfopLocaltimestampN => "localtimestamp",
        O::SvfopCurrentRole => "current_role",
        O::SvfopCurrentUser => "current_user",
        O::SvfopUser => "user",
        O::SvfopSessionUser => "session_user",
        O::SvfopCurrentCatalog => "current_catalog",
        O::SvfopCurrentSchema => "current_schema",
        O::SqlvalueFunctionOpUndefined => return None,
    })
}
