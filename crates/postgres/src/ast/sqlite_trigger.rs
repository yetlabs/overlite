//! SQLite's own `CREATE TRIGGER ... BEGIN ... END`, the one statement of SQLite's
//! dialect the server takes (docs/adr/0001-sql-ast.md, stage 5c): while no PL/pgSQL
//! runs, it is the only way to have a trigger, so it goes to SQLite as written, without
//! translation. Every other statement Postgres' grammar refuses is refused with
//! Postgres' error.
//!
//! The forms are told apart on the tree. A trigger's head (its name, timing, events,
//! table, FOR EACH and WHEN) is Postgres' grammar too: [`head`] reads it with
//! `EXECUTE FUNCTION overlite_sqlite_trigger_body()` in place of the body, so a SQLite
//! trigger is a `CreateTrigStmt` whose function is [`BODY_MARK`]. A Postgres
//! `CREATE TRIGGER ... EXECUTE FUNCTION f()` parses as a `CreateTrigStmt` naming f,
//! which a PL/pgSQL of the future can take beside it.

use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Token};

/// The function a SQLite trigger's head names in place of its body.
pub(crate) const BODY_MARK: &str = "overlite_sqlite_trigger_body";

/// The head of sql as Postgres reads it, when sql is a SQLite trigger: `CREATE [TEMP]
/// TRIGGER [IF NOT EXISTS] name ... ON table [FOR EACH ROW] [WHEN expr] BEGIN ...;
/// END`. SQLite's TEMP and IF NOT EXISTS, which Postgres' trigger has not, are left
/// out of the head, and its WHEN, which takes no parentheses, is given them.
pub(crate) fn head(sql: &str) -> Option<pb::CreateTrigStmt> {
    let scanned = pg_query::scan(sql).ok()?;
    let toks = &scanned.tokens;
    let kind = |i: usize| {
        toks.get(i)
            .map_or(Token::Nul, |t| Token::try_from(t.token).unwrap_or(Token::Nul))
    };
    if kind(0) != Token::Create {
        return None;
    }
    let mut i = 1;
    let mut drop: Vec<(usize, usize)> = Vec::new();
    while matches!(kind(i), Token::Temp | Token::Temporary) {
        drop.push((i, i));
        i += 1;
    }
    if kind(i) != Token::Trigger {
        return None;
    }
    if kind(i + 1) == Token::IfP && kind(i + 2) == Token::Not && kind(i + 3) == Token::Exists {
        drop.push((i + 1, i + 3));
    }
    // The body: from the first BEGIN outside parentheses to the END closing it.
    let mut depth = 0i32;
    let mut when = None;
    let mut begin = None;
    for j in i..toks.len() {
        match kind(j) {
            Token::Ascii40 => depth += 1,
            Token::Ascii41 => depth -= 1,
            Token::When if depth == 0 => when = Some(j),
            Token::BeginP if depth == 0 => {
                begin = Some(j);
                break;
            }
            _ => {}
        }
    }
    let begin = begin?;
    let last = (0..toks.len()).rev().find(|&j| kind(j) != Token::Ascii59)?;
    if kind(last) != Token::EndP || last <= begin {
        return None;
    }
    let at = |j: usize| usize::try_from(toks[j].start).unwrap_or(0);
    let end = |j: usize| usize::try_from(toks[j].end).unwrap_or(0);
    let mut text = String::with_capacity(at(begin) + 48);
    let mut from = 0;
    for (a, b) in drop {
        text.push_str(sql.get(from..at(a))?);
        from = end(b);
    }
    match when {
        Some(w) => {
            text.push_str(sql.get(from..end(w))?);
            text.push_str(" (");
            text.push_str(sql.get(end(w)..at(begin))?);
            text.push(')');
        }
        None => text.push_str(sql.get(from..at(begin))?),
    }
    text.push_str(" EXECUTE FUNCTION ");
    text.push_str(BODY_MARK);
    text.push_str("()");
    let parsed = crate::ast::parse::parse(&text).ok()?;
    let [raw] = parsed.stmts.as_slice() else {
        return None;
    };
    match raw.stmt.as_deref().and_then(|s| s.node.as_ref())? {
        NodeEnum::CreateTrigStmt(t) => Some((**t).clone()),
        _ => None,
    }
}

/// Whether sql is a SQLite trigger (`head`), which goes to SQLite as written.
pub(crate) fn is_sqlite_trigger(sql: &str) -> bool {
    head(sql).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(sql: &str) -> Option<String> {
        head(sql).and_then(|t| t.relation).map(|r| r.relname)
    }

    #[test]
    fn a_sqlite_trigger_is_told_by_its_head() {
        for sql in [
            "CREATE TRIGGER t_ai AFTER INSERT ON t BEGIN INSERT INTO audit VALUES ('x'); END",
            "create temp trigger if not exists x before update of a, b on T for each row \
             when new.a > 1 and old.a <> new.a begin update u set n = n + 1; delete from v; end;",
            "CREATE TRIGGER x AFTER DELETE ON t FOR EACH ROW WHEN (old.id > 0) BEGIN SELECT 1; END",
        ] {
            assert_eq!(table(sql).as_deref(), Some("t"), "{sql}");
        }
        assert_eq!(
            head("CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1; END")
                .unwrap()
                .funcname
                .len(),
            1
        );
        for sql in [
            // Postgres' own form, and what is no trigger.
            "CREATE TRIGGER x AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
            "CREATE TABLE t (a int)",
            "BEGIN",
            "CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1;",
            "CREATE TRIGGER x INSERT ON t BEGIN SELECT 1; END",
        ] {
            assert!(head(sql).is_none(), "{sql}");
        }
    }
}
