//! SQL-level prepared statements: PREPARE name [(types)] AS `<query>`, then
//! EXECUTE name [(args)], then DEALLOCATE name. psql and pg_dump use these (e.g.
//! pg_dump prepares a per-enum label query and executes it for each enum type).
//! This is separate from the wire Extended-Query prepared statements.

use crate::error::Error;
use crate::error::Result;
use crate::session::utility::log_query_error;
use crate::session::{Session, quote_name};
use overlite_core::SqlState;
use pg_query::NodeEnum;
use pg_query::protobuf::Token;

/// PREPARE, EXECUTE or DEALLOCATE (of a name, or of all), by the name the grammar reads.
enum SqlPrepare {
    Prepare(String),
    Execute(String),
    Deallocate(Option<String>),
}

/// The tokens of sql as Postgres' scanner reads them: each one's kind, start and end.
fn tokens(sql: &str) -> Vec<(Token, usize, usize)> {
    pg_query::scan(sql).map_or_else(
        |_| Vec::new(),
        |r| {
            r.tokens
                .iter()
                .map(|t| {
                    (
                        Token::try_from(t.token).unwrap_or(Token::Nul),
                        usize::try_from(t.start).unwrap_or(0),
                        usize::try_from(t.end).unwrap_or(0),
                    )
                })
                .collect()
        },
    )
}

/// The text of the query `PREPARE name [(types)] AS query` prepares: what follows the
/// first AS outside parentheses.
fn prepared_query(sql: &str) -> Option<&str> {
    let mut depth = 0;
    for (t, _, end) in tokens(sql) {
        match t {
            Token::Ascii40 => depth += 1,
            Token::Ascii41 => depth -= 1,
            Token::As if depth == 0 => return sql.get(end..).map(str::trim),
            _ => {}
        }
    }
    None
}

/// The text of each argument of `EXECUTE name (a, b, ...)`, as written.
fn execute_args(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start = None;
    for (t, at, end) in tokens(sql) {
        match t {
            Token::Ascii40 => {
                depth += 1;
                if depth == 1 {
                    start = Some(end);
                }
            }
            Token::Ascii41 => {
                depth -= 1;
                if depth == 0
                    && let Some(s) = start.take()
                {
                    out.push(sql[s..at].trim().to_string());
                }
            }
            Token::Ascii44 if depth == 1 => {
                if let Some(s) = start {
                    out.push(sql[s..at].trim().to_string());
                }
                start = Some(end);
            }
            _ => {}
        }
    }
    out.retain(|a| !a.is_empty());
    out
}

impl Session {
    /// Handles PREPARE/EXECUTE/DEALLOCATE in the simple-query path, read from the
    /// statement's tree; None when it is none of them.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_sql_prepare(&mut self, sql: &str) -> Option<Result<()>> {
        // A statement that reads or writes rows is none of them, and not parsed here.
        let stmt = crate::ast::classify::read(sql, |n| match n {
            NodeEnum::PrepareStmt(p) => Some(SqlPrepare::Prepare(p.name.clone())),
            NodeEnum::ExecuteStmt(e) => Some(SqlPrepare::Execute(e.name.clone())),
            NodeEnum::DeallocateStmt(d) => Some(SqlPrepare::Deallocate((!d.isall).then(|| d.name.clone()))),
            _ => None,
        })??;
        match stmt {
            SqlPrepare::Prepare(name) => Some(self.handle_prepare(&name, sql)),
            SqlPrepare::Execute(name) => Some(self.handle_execute_sql(&name, sql)),
            SqlPrepare::Deallocate(name) => {
                if let Some(name) = name
                    && self.sql_prepared.remove(&name).is_none()
                {
                    let message = format!("prepared statement {} does not exist", quote_name(&name));
                    return Some(self.fail_send(&Error::with_state(SqlState::INVALID_SQL_STATEMENT_NAME, message)));
                }
                Some(self.c.send_command_complete("DEALLOCATE"))
            }
        }
    }

    /// Stores "PREPARE name [(types)] AS <query>".
    fn handle_prepare(&mut self, name: &str, sql: &str) -> Result<()> {
        let Some(query) = prepared_query(sql) else {
            return self.c.send_error(SqlState::SYNTAX_ERROR, "PREPARE requires AS <query>");
        };
        self.sql_prepared.insert(name.to_string(), query.to_string());
        self.c.send_command_complete("PREPARE")
    }

    /// Runs a prepared statement, substituting its $N parameters with the EXECUTE
    /// arguments.
    fn handle_execute_sql(&mut self, name: &str, sql: &str) -> Result<()> {
        let query = match self.sql_prepared.get(name) {
            Some(q) => q.clone(),
            None => {
                return self.c.send_error(
                    SqlState::INVALID_SQL_STATEMENT_NAME,
                    &("prepared statement ".to_string() + &quote_name(name) + " does not exist"),
                );
            }
        };
        let final_ = substitute_params(&query, &execute_args(sql));

        if let Err(e) = self.check_privileges(&final_) {
            self.mark_tx_failed();
            return self.c.send_error(e.sql_state(), e.message());
        }
        // As the statement itself runs, its INSERT held to its table's policies.
        let rs = match self.run_statement(&final_, &[]) {
            Ok(rs) => rs,
            Err(e) => {
                log_query_error("execute-sql", &e, &final_, &final_);
                self.mark_tx_failed();
                return self.send_exec_error(&e);
            }
        };
        if rs.is_query {
            self.c.send_result_set(&rs)?;
        }
        self.c.send_command_complete(&rs.command_tag())
    }
}

/// Replaces $1, $2, ... in query with the given args (already SQL literals), each a
/// parameter token of Postgres' scanner, so none in a literal or a comment.
fn substitute_params(query: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(query.len());
    let mut at = 0;
    for (t, start, end) in tokens(query) {
        if t != Token::Param {
            continue;
        }
        let Some(arg) = query[start + 1..end]
            .parse::<usize>()
            .ok()
            .and_then(|n| args.get(n.checked_sub(1)?))
        else {
            continue;
        };
        out.push_str(&query[at..start]);
        out.push_str(arg);
        at = end;
    }
    out.push_str(&query[at..]);
    out
}
