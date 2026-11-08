//! Postgres takes several actions in one ALTER TABLE, comma-separated, which is
//! how Prisma writes a migration that changes more than one column of a table.
//! SQLite's ALTER TABLE does one thing at a time. Each action becomes an ALTER
//! TABLE of its own, run in order inside the batch's transaction, so they still
//! apply together or not at all.

use crate::ddl::sequences::split_top_level;
use crate::error::Result;
use crate::rewrite::blank::strip_leading_comments;
use crate::session::Session;
use crate::session::utility::TxControl;
use crate::session::utility::{alter_is_noop, first_word_upper, second_word_upper};
use overlite_core::Command;
use overlite_core::lazy_regex;
use overlite_core::text::eq_fold;

// Matches `ALTER TABLE [IF EXISTS] [ONLY] <table>` and what
// follows it, the table possibly schema-qualified and quoted.
lazy_regex!(pub(crate) RE_ALTER_TABLE_HEAD, concat!(
    r#"(?is)^(\s*ALTER\s+TABLE\s+(?:IF\s+EXISTS\s+)?(?:ONLY\s+)?"#,
    r#"(?:"(?:[^"]|"")+"|[\w$]+)(?:\s*\.\s*(?:"(?:[^"]|"")+"|[\w$]+))?)\s+(.*?)\s*;?\s*$"#
));

/// Returns one ALTER TABLE per action of stmt, or stmt alone when it is not an
/// ALTER TABLE with more than one action.
pub(crate) fn split_alter_actions(stmt: &str) -> Vec<String> {
    let body = strip_leading_comments(stmt);
    if !eq_fold(&first_word_upper(body), "ALTER") || second_word_upper(body) != "TABLE" {
        return vec![stmt.to_string()];
    }
    let Some(m) = RE_ALTER_TABLE_HEAD.captures(body) else {
        return vec![stmt.to_string()];
    };
    let m1 = m.get(1).map_or("", |x| x.as_str());
    let m2 = m.get(2).map_or("", |x| x.as_str());
    let actions = split_top_level(m2);
    if actions.len() < 2 {
        return vec![stmt.to_string()];
    }
    let mut out = Vec::with_capacity(actions.len());
    for a in &actions {
        out.push([m1.trim(), " ", a.trim()].concat());
    }
    out
}

impl Session {
    /// Runs the ALTER TABLE of a prepared statement. handle_query splits a Query
    /// message before it gets here; Execute has to give the actions of one statement
    /// a single CommandComplete, so it runs them itself, as one transaction.
    pub(crate) fn alter_table_prepared(&mut self, sql: &str) -> Result<Command> {
        self.check_alter_table(sql)?;
        if self.alter_skipped(sql)? {
            return Ok(Command::from("ALTER TABLE"));
        }
        let actions = split_alter_actions(sql);
        if actions.len() < 2 {
            self.alter_table_action(sql)?;
            return Ok(Command::from("ALTER TABLE"));
        }
        let implicit = !self.in_tx();
        if implicit {
            self.apply_tx_control(TxControl::Begin)?;
        }
        let done = actions.iter().try_for_each(|action| self.alter_table_action(action));
        if implicit {
            let ended = self.end_tx(done.is_ok());
            done?;
            ended?;
        } else {
            done?;
        }
        Ok(Command::from("ALTER TABLE"))
    }

    /// One action, by the route handle_simple_query would give it.
    fn alter_table_action(&mut self, action: &str) -> Result<()> {
        if let Some(r) = self.try_rls_ddl(action).or_else(|| self.try_alter_table(action)) {
            return r.map(|_| ());
        }
        if alter_is_noop(action) {
            return Ok(());
        }
        let sql = self.apply_column_defaults(action);
        let sql = self.rewrite_for_exec(&sql, &[])?;
        self.exec(&sql, &[])?;
        self.record_ownership(action);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn split_alter_actions_cases() {
        assert_eq!(
            split_alter_actions(
                "-- AlterTable\nALTER TABLE \"School\" ADD COLUMN     \"nickname\" TEXT,\nADD COLUMN     \"rank\" INTEGER NOT NULL DEFAULT 0"
            ),
            vec![
                r#"ALTER TABLE "School" ADD COLUMN     "nickname" TEXT"#,
                r#"ALTER TABLE "School" ADD COLUMN     "rank" INTEGER NOT NULL DEFAULT 0"#,
            ]
        );
        assert_eq!(
            split_alter_actions(
                "ALTER TABLE IF EXISTS ONLY public.t DROP COLUMN a, ADD COLUMN b numeric(10, 2) DEFAULT 'x,y';"
            ),
            vec![
                "ALTER TABLE IF EXISTS ONLY public.t DROP COLUMN a",
                "ALTER TABLE IF EXISTS ONLY public.t ADD COLUMN b numeric(10, 2) DEFAULT 'x,y'",
            ]
        );
        let one = "ALTER TABLE t ADD CONSTRAINT k FOREIGN KEY (a, b) REFERENCES u (a, b)";
        assert_eq!(split_alter_actions(one), vec![one]);
        assert_eq!(split_alter_actions("SELECT a, b FROM t"), vec!["SELECT a, b FROM t"]);
    }
}
