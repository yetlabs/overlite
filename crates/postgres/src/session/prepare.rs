//! SQL-level prepared statements: PREPARE name [(types)] AS `<query>`, then
//! EXECUTE name [(args)], then DEALLOCATE name. psql and pg_dump use these (e.g.
//! pg_dump prepares a per-enum label query and executes it for each enum type).
//! This is separate from the wire Extended-Query prepared statements.

use crate::ddl::sequences::{is_ident_part, read_paren_args, split_top_level};
use crate::error::Error;
use crate::error::Result;
use crate::rewrite::distinct_on::index_top_level_word;
use crate::rewrite::strings::end_of_string_literal;
use crate::session::utility::{first_word_upper, log_query_error, unquote_ident};
use crate::session::{Session, quote_name};
use overlite_core::SqlState;
use overlite_core::text::{split_words, trim_start_chars};

impl Session {
    /// Handles PREPARE/EXECUTE/DEALLOCATE in the simple-query path; None when it
    /// did not recognize the statement.
    #[must_use = "the error inside is the answer to the statement"]
    pub(crate) fn try_sql_prepare(&mut self, sql: &str) -> Option<Result<()>> {
        match first_word_upper(sql).as_str() {
            "PREPARE" => Some(self.handle_prepare(sql)),
            "EXECUTE" => Some(self.handle_execute_sql(sql)),
            "DEALLOCATE" => {
                let f = split_words(sql);
                if let Some(word) = f.iter().skip(1).find(|w| !w.eq_ignore_ascii_case("prepare"))
                    && !word.eq_ignore_ascii_case("all")
                    && self.sql_prepared.remove(&unquote_ident(word)).is_none()
                {
                    let message = format!("prepared statement {} does not exist", quote_name(&unquote_ident(word)));
                    return Some(self.fail_send(&Error::with_state(SqlState::INVALID_SQL_STATEMENT_NAME, message)));
                }
                Some(self.c.send_command_complete("DEALLOCATE"))
            }
            _ => None,
        }
    }

    /// Stores "PREPARE name [(types)] AS <query>".
    fn handle_prepare(&mut self, sql: &str) -> Result<()> {
        let rest = sql["prepare".len()..].trim();
        let name = read_ident(rest);
        if name.is_empty() {
            return self.c.send_error(SqlState::SYNTAX_ERROR, "syntax error in PREPARE");
        }
        // The query begins after the top-level " AS " (any argument-type list is in
        // parens, so the first depth-0 AS is the separator).
        let Some(as_) = index_top_level_word(sql, "prepare".len(), "as") else {
            return self.c.send_error(SqlState::SYNTAX_ERROR, "PREPARE requires AS <query>");
        };
        self.sql_prepared
            .insert(name, sql[as_ + "as".len()..].trim().to_string());
        self.c.send_command_complete("PREPARE")
    }

    /// Runs a prepared statement, substituting its $N parameters with the EXECUTE
    /// arguments.
    fn handle_execute_sql(&mut self, sql: &str) -> Result<()> {
        let rest = sql["execute".len()..].trim();
        let name = read_ident(rest);
        let query = match self.sql_prepared.get(&name) {
            Some(q) => q.clone(),
            None => {
                return self.c.send_error(
                    SqlState::INVALID_SQL_STATEMENT_NAME,
                    &("prepared statement ".to_string() + &quote_name(&name) + " does not exist"),
                );
            }
        };
        let mut args: Vec<String> = Vec::new();
        if let Some(i) = rest.find('(')
            && let Some((inner, _)) = read_paren_args(rest, i)
        {
            for a in split_top_level(&inner) {
                args.push(a.trim().to_string());
            }
        }
        let final_ = substitute_params(&query, &args);

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

/// Reads a leading (optionally "quoted") identifier from s.
fn read_ident(s: &str) -> String {
    let s = trim_start_chars(s, " \t\r\n");
    if s.is_empty() {
        return String::new();
    }
    let b = s.as_bytes();
    if b[0] == b'"' {
        if let Some(j) = s[1..].find('"') {
            return s[1..=j].to_string();
        }
        return String::new();
    }
    let mut j = 0;
    while j < b.len() && is_ident_part(b[j]) {
        j += 1;
    }
    s[..j].to_string()
}
