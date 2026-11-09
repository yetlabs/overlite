//! Postgres takes several actions in one ALTER TABLE, comma-separated, which is
//! how Prisma writes a migration that changes more than one column of a table.
//! SQLite's ALTER TABLE does one thing at a time. Each action becomes an ALTER
//! TABLE of its own, run in order inside the batch's transaction, so they still
//! apply together or not at all.

use crate::ddl::tree::{self, CodeTok, code_tokens};
use crate::error::Result;
use crate::session::Session;
use crate::session::utility::TxControl;
use crate::session::utility::alter_is_noop;
use overlite_core::Command;
use pg_query::NodeEnum;

/// An ALTER TABLE's head and its actions, as Postgres reads them: the node says the
/// actions and where the relation is; the scanner where the head ends (after the
/// relation's name) and the commas between the actions are. None for any other
/// statement.
pub(crate) struct AlterParts<'a> {
    /// `ALTER TABLE [IF EXISTS] [ONLY] name`, as written.
    pub(crate) head: &'a str,
    /// Each action, as written, trimmed.
    pub(crate) actions: Vec<&'a str>,
}

pub(crate) fn alter_parts(sql: &str) -> Option<AlterParts<'_>> {
    let (location, count) = tree::read(sql, |n| match n {
        NodeEnum::AlterTableStmt(a) if tree::object(a.objtype) == pg_query::protobuf::ObjectType::ObjectTable => {
            Some((usize::try_from(a.relation.as_ref()?.location).ok()?, a.cmds.len()))
        }
        _ => None,
    })?;
    let toks: Vec<CodeTok> = code_tokens(sql);
    let first = toks.first()?;
    let mut at = toks.iter().position(|t| t.span.start == location)?;
    while toks.get(at + 1).is_some_and(|t| t.is_punct(sql, ".")) && toks.get(at + 2).is_some() {
        at += 2;
    }
    let head = &sql[first.span.start..toks[at].span.end];
    let mut actions = Vec::new();
    let mut from = toks[at].span.end;
    let mut end = sql.len();
    for t in &toks[at + 1..] {
        if t.depth == 0 && t.is_punct(sql, ",") {
            actions.push(sql[from..t.span.start].trim());
            from = t.span.end;
        } else if t.depth == 0 && t.is_punct(sql, ";") {
            end = t.span.start;
        }
    }
    actions.push(sql[from..end].trim());
    (actions.len() == count).then_some(AlterParts { head, actions })
}

/// Returns one ALTER TABLE per action of stmt, or stmt alone when it is not an
/// ALTER TABLE with more than one action.
pub(crate) fn split_alter_actions(stmt: &str) -> Vec<String> {
    let Some(parts) = alter_parts(stmt).filter(|p| p.actions.len() > 1) else {
        return vec![stmt.to_string()];
    };
    parts
        .actions
        .iter()
        .map(|a| [parts.head.trim(), " ", a].concat())
        .collect()
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
