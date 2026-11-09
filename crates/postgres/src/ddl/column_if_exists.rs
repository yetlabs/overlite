//! ALTER TABLE ... ADD COLUMN IF NOT EXISTS and DROP COLUMN IF EXISTS. SQLite has
//! neither ("near EXISTS: syntax error"); Evolution API's migrations add a column
//! that way. The column is looked up in the table: when the IF already decides,
//! nothing runs, and otherwise the statement runs without it.

use crate::ddl::tree::{self, code_tokens};
use crate::error::Result;
use crate::rewrite::rewrite;
use crate::session::Session;
use overlite_core::SqlState;
use overlite_core::text::quote_literal;
use overlite_engine::split_stored_name;
use pg_query::NodeEnum;
use pg_query::protobuf::AlterTableType;

/// The one action of an ALTER TABLE when it is ADD COLUMN IF NOT EXISTS or DROP
/// COLUMN IF EXISTS: the column, and whether the action wants it there (DROP).
fn column_if(sql: &str) -> Option<(String, bool)> {
    tree::read(sql, |n| {
        let NodeEnum::AlterTableStmt(a) = n else { return None };
        let [cmd] = a.cmds.as_slice() else { return None };
        let Some(NodeEnum::AlterTableCmd(c)) = &cmd.node else {
            return None;
        };
        if !c.missing_ok {
            return None;
        }
        match AlterTableType::try_from(c.subtype).ok()? {
            AlterTableType::AtAddColumn => match c.def.as_deref()?.node.as_ref()? {
                NodeEnum::ColumnDef(d) => Some((d.colname.clone(), false)),
                _ => None,
            },
            AlterTableType::AtDropColumn => Some((c.name.clone(), true)),
            _ => None,
        }
    })
}

/// sql without its IF [NOT] EXISTS, which opens the action after ADD [COLUMN] or DROP
/// [COLUMN], as Postgres' scanner finds it.
fn without_clause(sql: &str) -> Option<String> {
    let toks = code_tokens(sql);
    let is = |i: usize, w: &str| toks.get(i).is_some_and(|t| t.depth == 0 && t.is_word(sql, w));
    let at =
        (0..toks.len()).find(|&i| is(i, "if") && (is(i + 1, "exists") || (is(i + 1, "not") && is(i + 2, "exists"))))?;
    let end = if is(at + 1, "not") { at + 2 } else { at + 1 };
    Some(
        [
            sql[..toks[at].span.start].trim_end(),
            " ",
            sql[toks[end].span.end..].trim_start(),
        ]
        .concat(),
    )
}

impl Session {
    /// Handles the two forms; None when sql is neither.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn column_if_exists(&mut self, sql: &str, table: &str) -> Option<Result<()>> {
        let (column, want_present) = column_if(sql)?;
        let present = match self.has_column(table, &column) {
            Ok(present) => present,
            Err(e) => return Some(Err(e)),
        };
        if present != want_present {
            // ADD of a column that is there, DROP of one that is not
            let (_, relation) = split_stored_name(table);
            return Some(if present {
                let message = format!("column \"{column}\" of relation \"{relation}\" already exists, skipping");
                self.c.send_notice_state(SqlState::DUPLICATE_COLUMN, &message)
            } else {
                self.c.send_notice(&format!(
                    "column \"{column}\" of relation \"{relation}\" does not exist, skipping"
                ))
            });
        }
        // Without the IF [NOT] EXISTS.
        let plain = without_clause(sql)?;
        if let Some(r) = self.try_alter_table(&plain) {
            return Some(r.map(|_| ()));
        }
        Some(self.exec(&rewrite(&plain), &[]).map(|_| ()))
    }

    /// Whether table, by its stored name, has the column.
    fn has_column(&mut self, table: &str, col: &str) -> Result<bool> {
        let q = "SELECT 1 FROM pragma_table_info(".to_string()
            + &quote_literal(table)
            + ") WHERE lower(name) = lower("
            + &quote_literal(col)
            + ")";
        Ok(!self.exec(&q, &[])?.rows.is_empty())
    }
}

/// Reports whether sql is one of the two forms.
pub(crate) fn has_column_if_exists(sql: &str) -> bool {
    column_if(sql).is_some()
}
