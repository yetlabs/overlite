//! ALTER TABLE ... ADD COLUMN IF NOT EXISTS and DROP COLUMN IF EXISTS. SQLite has
//! neither ("near EXISTS: syntax error"); Evolution API's migrations add a column
//! that way. The column is looked up in the table: when the IF already decides,
//! nothing runs, and otherwise the statement runs without it.

use crate::ddl::alter_actions::RE_ALTER_TABLE_HEAD;
use crate::error::Result;
use crate::rewrite::blank::strip_leading_comments;
use crate::rewrite::rewrite;
use crate::session::Session;
use crate::types::param_types::unquote;
use overlite_core::text::quote_literal;
use overlite_core::{SqlState, lazy_regex};
use overlite_engine::split_stored_name;

lazy_regex!(
    RE_ADD_COLUMN_IF_NOT_EXISTS,
    r#"(?is)^ADD\s+(?:COLUMN\s+)?(IF\s+NOT\s+EXISTS\s+)("(?:[^"]|"")+"|[\w$]+)"#
);
lazy_regex!(
    RE_DROP_COLUMN_IF_EXISTS,
    r#"(?is)^DROP\s+(?:COLUMN\s+)?(IF\s+EXISTS\s+)("(?:[^"]|"")+"|[\w$]+)"#
);

impl Session {
    /// Handles the two forms; None when sql is neither.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn column_if_exists(&mut self, sql: &str, table: &str) -> Option<Result<()>> {
        let start = action_start(sql)?;
        let action = &sql[start..];
        let (m, want_present) = match RE_ADD_COLUMN_IF_NOT_EXISTS.captures(action) {
            Some(m) => (m, false),
            None => (RE_DROP_COLUMN_IF_EXISTS.captures(action)?, true),
        };
        let (clause, name) = (m.get(1)?, m.get(2)?);
        let column = unquote(name.as_str());
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
        let plain = [&sql[..start + clause.start()], &sql[start + clause.end()..]].concat();
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

/// Where the action of an ALTER TABLE begins. The clause has to open it: anywhere
/// later it is the text of a literal or a quoted name.
fn action_start(sql: &str) -> Option<usize> {
    let action = RE_ALTER_TABLE_HEAD.captures(sql)?.get(2)?;
    Some(sql.len() - strip_leading_comments(&sql[action.start()..]).len())
}

/// Reports whether sql is one of the two forms.
pub(crate) fn has_column_if_exists(sql: &str) -> bool {
    action_start(sql).is_some_and(|start| {
        let action = &sql[start..];
        RE_ADD_COLUMN_IF_NOT_EXISTS.is_match(action) || RE_DROP_COLUMN_IF_EXISTS.is_match(action)
    })
}
