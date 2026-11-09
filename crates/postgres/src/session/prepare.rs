//! SQL-level prepared statements: PREPARE name [(types)] AS `<query>`, then
//! EXECUTE name [(args)], then DEALLOCATE name. psql and pg_dump use these (e.g.
//! pg_dump prepares a per-enum label query and executes it for each enum type).
//! This is separate from the wire Extended-Query prepared statements.

use crate::ddl::tree;
use crate::error::Error;
use crate::error::Result;
use crate::session::utility::log_query_error;
use crate::session::{Session, quote_name};
use overlite_core::SqlState;
use overlite_core::text::end_of_string_literal;
use pg_query::NodeEnum;

/// PREPARE, EXECUTE or DEALLOCATE, read from the node; the texts are the client's:
/// a prepared query after its AS, each argument of EXECUTE between its commas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SqlPrepare {
    Prepare {
        name: String,
        query: String,
    },
    Execute {
        name: String,
        args: Vec<String>,
    },
    /// None for DEALLOCATE ALL.
    Deallocate(Option<String>),
    /// PREPARE TRANSACTION, which two-phase commit would need.
    Transaction,
}

pub(crate) fn sql_prepare_plan(sql: &str, n: &NodeEnum) -> Option<SqlPrepare> {
    let text = |t: &tree::Token| &sql[t.start..t.end];
    match n {
        NodeEnum::PrepareStmt(p) => {
            let toks = tree::tokens(sql);
            let at = toks
                .iter()
                .position(|t| t.depth == 0 && t.keyword && text(t).eq_ignore_ascii_case("as"))?;
            let query = sql[toks.get(at + 1)?.start..].trim();
            Some(SqlPrepare::Prepare {
                name: p.name.clone(),
                query: query.strip_suffix(';').unwrap_or(query).trim_end().to_string(),
            })
        }
        NodeEnum::ExecuteStmt(e) => {
            let mut args = Vec::new();
            if !e.params.is_empty() {
                let toks = tree::tokens(sql);
                let open = toks.iter().position(|t| t.depth == 0 && text(t) == "(")?;
                let mut start = toks[open].end;
                for t in &toks[open + 1..] {
                    if t.depth == 1 && text(t) == "," {
                        args.push(sql[start..t.start].trim().to_string());
                        start = t.end;
                    } else if t.depth == 0 && text(t) == ")" {
                        args.push(sql[start..t.start].trim().to_string());
                        break;
                    }
                }
            }
            Some(SqlPrepare::Execute {
                name: e.name.clone(),
                args,
            })
        }
        NodeEnum::DeallocateStmt(d) => Some(SqlPrepare::Deallocate((!d.isall).then(|| d.name.clone()))),
        NodeEnum::TransactionStmt(_) => Some(SqlPrepare::Transaction),
        _ => None,
    }
}

impl Session {
    /// Handles PREPARE/EXECUTE/DEALLOCATE in the simple-query path; None when it
    /// did not recognize the statement.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_sql_prepare(&mut self, sql: &str) -> Option<Result<()>> {
        if !crate::ast::classify::read(sql, crate::ast::classify::sql_prepare).unwrap_or(false) {
            return None;
        }
        Some(match tree::read(sql, |n| sql_prepare_plan(sql, n))? {
            SqlPrepare::Prepare { name, query } => {
                self.sql_prepared.insert(name, query);
                self.c.send_command_complete("PREPARE")
            }
            SqlPrepare::Execute { name, args } => self.handle_execute_sql(&name, &args),
            SqlPrepare::Deallocate(name) => {
                if let Some(name) = name
                    && self.sql_prepared.remove(&name).is_none()
                {
                    let message = format!("prepared statement {} does not exist", quote_name(&name));
                    return Some(self.fail_send(&Error::with_state(SqlState::INVALID_SQL_STATEMENT_NAME, message)));
                }
                self.c.send_command_complete("DEALLOCATE")
            }
            SqlPrepare::Transaction => self.c.send_error(SqlState::SYNTAX_ERROR, "PREPARE requires AS <query>"),
        })
    }

    /// Runs a prepared statement, substituting its $N parameters with the EXECUTE
    /// arguments.
    fn handle_execute_sql(&mut self, name: &str, args: &[String]) -> Result<()> {
        let query = match self.sql_prepared.get(name) {
            Some(q) => q.clone(),
            None => {
                return self.c.send_error(
                    SqlState::INVALID_SQL_STATEMENT_NAME,
                    &("prepared statement ".to_string() + &quote_name(name) + " does not exist"),
                );
            }
        };
        let final_ = substitute_params(&query, args);

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

/// Replaces $1, $2, ... in query with the given args (already SQL literals),
/// skipping placeholders inside string literals.
fn substitute_params(query: &str, args: &[String]) -> String {
    let q = query.as_bytes();
    let mut b = String::with_capacity(query.len());
    let mut i = 0;
    while i < q.len() {
        if q[i] == b'\'' {
            let j = end_of_string_literal(query, i);
            b.push_str(&query[i..j]);
            i = j;
            continue;
        }
        if q[i] == b'$' && i + 1 < q.len() && q[i + 1] >= b'1' && q[i + 1] <= b'9' {
            let mut j = i + 1;
            let mut n: i64 = 0;
            while j < q.len() && q[j] >= b'0' && q[j] <= b'9' {
                n = n.wrapping_mul(10).wrapping_add((q[j] - b'0') as i64);
                j += 1;
            }
            if n >= 1 && n as usize <= args.len() {
                b.push_str(&args[(n - 1) as usize]);
                i = j;
                continue;
            }
        }
        let Some(ch) = query[i..].chars().next() else { break };
        b.push(ch);
        i += ch.len_utf8();
    }
    b
}
