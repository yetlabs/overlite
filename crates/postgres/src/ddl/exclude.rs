//! EXCLUDE constraints, which SQLite has no form of. Each is two triggers on its
//! table, BEFORE INSERT and BEFORE UPDATE, that look for a row the new one conflicts
//! with: a row for which every element's operator holds between the two, past the
//! constraint's WHERE. The first trigger's body carries the constraint as Postgres
//! gave it, `/*overlite exclude {json}*/`, which pg_constraint and DROP CONSTRAINT
//! read back; the triggers go with the table, through a rebuild too.
//!
//! The operators are the commutative ones Postgres takes: `=`, `<>`, `&&` and `-|-`.
//! Any other is 0A000. A DEFERRABLE one is checked as each row is written, which can
//! only refuse sooner than Postgres would, never let a conflict through.

use crate::ddl::alter::translate_check;
use crate::error::{Result, errf};
use crate::rewrite::tokens::{CodeTok, CodeTokKind, code_tokens};
use crate::session::Session;
use crate::session::utility::unquote_ident;
use overlite_core::text::{quote_ident, quote_literal};
use overlite_core::{Command, SqlState};
use overlite_engine::split_stored_name;

/// What a trigger of an exclusion constraint is named after.
const TRIGGER_PREFIX: &str = "_overlite_excl_";
/// The comment that carries the constraint in its first trigger.
const MARK: &str = "/*overlite exclude ";

/// An EXCLUDE constraint as written.
#[derive(Debug, Default)]
pub(crate) struct Exclusion {
    name: Option<String>,
    /// The Postgres text from EXCLUDE on, as pg_get_constraintdef gives it.
    def: String,
    elems: Vec<Element>,
    predicate: Option<String>,
    deferrable: bool,
    deferred: bool,
}

#[derive(Debug)]
struct Element {
    /// The expression, in Postgres' words.
    expr: String,
    /// The column it is, when it is one.
    column: Option<String>,
    op: String,
}

/// The index of the token closing the parenthesis at open.
fn closing(toks: &[CodeTok], sql: &str, open: usize) -> Option<usize> {
    let depth = toks[open].depth;
    (open + 1..toks.len()).find(|&k| toks[k].depth == depth && toks[k].is_punct(sql, ")"))
}

/// Reads `EXCLUDE [USING method] (elem WITH op, ...) [INCLUDE (...)] [WITH (...)]
/// [WHERE (pred)] [[NOT] DEFERRABLE] [INITIALLY DEFERRED|IMMEDIATE]`, name given apart.
pub(crate) fn parse_exclusion(name: Option<String>, clause: &str) -> Result<Exclusion> {
    let bad = || errf!(SqlState::SYNTAX_ERROR, "syntax error in EXCLUDE constraint");
    let toks = code_tokens(clause);
    if !toks.first().is_some_and(|t| t.is_word(clause, "exclude")) {
        return Err(bad());
    }
    let mut i = 1;
    if toks.get(i).is_some_and(|t| t.is_word(clause, "using")) {
        i += 2;
    }
    if !toks.get(i).is_some_and(|t| t.is_punct(clause, "(")) {
        return Err(bad());
    }
    let close = closing(&toks, clause, i).ok_or_else(bad)?;
    let mut x = Exclusion {
        name,
        def: clause.trim().trim_end_matches(';').trim().to_string(),
        ..Exclusion::default()
    };
    // The elements, split at the commas of the list.
    let inner_depth = toks[i].depth + 1;
    let mut from = toks[i].span.end;
    let mut cuts = Vec::new();
    for t in &toks[i + 1..close] {
        if t.depth == inner_depth && t.is_punct(clause, ",") {
            cuts.push(&clause[from..t.span.start]);
            from = t.span.end;
        }
    }
    cuts.push(&clause[from..toks[close].span.start]);
    for elem in cuts {
        x.elems.push(parse_element(elem).ok_or_else(bad)?);
    }
    // What follows the list.
    let mut k = close + 1;
    while k < toks.len() {
        let t = &toks[k];
        let word = |w: &str| t.is_word(clause, w);
        if word("include") || word("with") {
            let open = k + 1;
            if !toks.get(open).is_some_and(|t| t.is_punct(clause, "(")) {
                return Err(bad());
            }
            k = closing(&toks, clause, open).ok_or_else(bad)? + 1;
        } else if word("where") {
            let open = k + 1;
            if !toks.get(open).is_some_and(|t| t.is_punct(clause, "(")) {
                return Err(bad());
            }
            let end = closing(&toks, clause, open).ok_or_else(bad)?;
            x.predicate = Some(clause[toks[open].span.end..toks[end].span.start].trim().to_string());
            k = end + 1;
        } else if word("using") {
            // USING INDEX TABLESPACE name
            k += 4;
        } else if word("not") && toks.get(k + 1).is_some_and(|t| t.is_word(clause, "deferrable")) {
            k += 2;
        } else if word("deferrable") {
            x.deferrable = true;
            k += 1;
        } else if word("initially") {
            x.deferred = toks.get(k + 1).is_some_and(|t| t.is_word(clause, "deferred"));
            k += 2;
        } else if t.is_punct(clause, ";") {
            k += 1;
        } else {
            return Err(bad());
        }
    }
    Ok(x)
}

/// One `expr [opclass] [ASC|DESC] [NULLS FIRST|LAST] WITH op`.
fn parse_element(elem: &str) -> Option<Element> {
    let toks = code_tokens(elem);
    let with = toks.iter().rposition(|t| t.depth == 0 && t.is_word(elem, "with"))?;
    let op = elem[toks.get(with + 1)?.span.start..].trim().to_string();
    let first = toks.first()?;
    let (expr, column) = if first.is_punct(elem, "(") {
        let end = closing(&toks, elem, 0)?;
        (elem[first.span.end..toks[end].span.start].trim().to_string(), None)
    } else {
        let text = &elem[first.span.clone()];
        (text.to_string(), Some(unquote_ident(text)))
    };
    Some(Element { expr, column, op })
}

/// The SQL that holds two values to an element's operator, None for one not taken.
fn operator_sql(op: &str, left: &str, right: &str) -> Option<String> {
    let op = op.trim();
    // OPERATOR(pg_catalog.&&) names the same operator.
    let op = op
        .strip_prefix("OPERATOR(")
        .or_else(|| op.strip_prefix("operator("))
        .and_then(|o| o.strip_suffix(')'))
        .map_or(op, |o| o.rsplit('.').next().unwrap_or(o).trim());
    Some(match op {
        "=" => format!("({left}) = ({right})"),
        "<>" | "!=" => format!("({left}) <> ({right})"),
        "&&" => format!("overlite_overlaps({left}, {right})"),
        "-|-" => format!("overlite_range_op('-|-', {left}, {right})"),
        _ => return None,
    })
}

/// sql with each name of a column of the table read through alias: `"a"` becomes
/// `NEW."a"`. A name before a parenthesis is a function, after a dot or AS is not a
/// column of this table.
fn qualify(sql: &str, columns: &[String], alias: &str) -> String {
    let toks = code_tokens(sql);
    let mut out = String::with_capacity(sql.len() + 16);
    let mut last = 0;
    for (k, t) in toks.iter().enumerate() {
        let text = &sql[t.span.clone()];
        let name = match t.kind {
            CodeTokKind::Word => text.to_ascii_lowercase(),
            CodeTokKind::Value if text.starts_with('"') => unquote_ident(text),
            _ => continue,
        };
        let is_column = columns.iter().any(|c| {
            if t.kind == CodeTokKind::Word {
                c.eq_ignore_ascii_case(&name)
            } else {
                *c == name
            }
        });
        let called = toks.get(k + 1).is_some_and(|n| n.is_punct(sql, "("));
        let after = k > 0 && (toks[k - 1].is_punct(sql, ".") || toks[k - 1].is_word(sql, "as"));
        if !is_column || called || after {
            continue;
        }
        out.push_str(&sql[last..t.span.start]);
        out.push_str(alias);
        out.push('.');
        out.push_str(&quote_ident(
            columns.iter().find(|c| c.eq_ignore_ascii_case(&name)).unwrap_or(&name),
        ));
        last = t.span.end;
    }
    out.push_str(&sql[last..]);
    out
}

/// An expression in SQLite's words, as a CHECK of CREATE TABLE would have it.
fn translate(expr: &str) -> String {
    let check = translate_check(&("CHECK (".to_string() + expr + ")"));
    check
        .strip_prefix("CHECK")
        .map(str::trim)
        .and_then(|c| c.strip_prefix('('))
        .and_then(|c| c.strip_suffix(')'))
        .map_or(check.clone(), |c| c.trim().to_string())
}

/// A JSON string, `*/` kept out so it can sit in a comment.
fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A constraint's name, when it is given one, and its clause.
type NamedClause = (Option<String>, String);

/// The items of a CREATE TABLE's column list that are EXCLUDE constraints, taken out:
/// the statement without them and each one with its name. None when there are none.
pub(crate) fn split_create_exclusions(sql: &str) -> Option<(String, Vec<NamedClause>)> {
    let toks = code_tokens(sql);
    let is = |k: usize, w: &str| toks.get(k).is_some_and(|t| t.is_word(sql, w));
    if !is(0, "create") {
        return None;
    }
    let open = toks.iter().position(|t| t.is_punct(sql, "("))?;
    if !toks[..open].iter().any(|t| t.is_word(sql, "table")) || !toks.iter().any(|t| t.is_word(sql, "exclude")) {
        return None;
    }
    let close = closing(&toks, sql, open)?;
    // The items as token ranges, split at the list's commas.
    let mut items = Vec::new();
    let mut start = open + 1;
    for k in open + 1..=close {
        if k == close || (toks[k].depth == toks[open].depth + 1 && toks[k].is_punct(sql, ",")) {
            items.push((start, k));
            start = k + 1;
        }
    }
    let mut found = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for &(a, b) in &items {
        if a >= b {
            continue;
        }
        let text = &sql[toks[a].span.start..toks[b - 1].span.end];
        let (name, at) = if is(a, "constraint") && b > a + 2 {
            (Some(unquote_ident(&sql[toks[a + 1].span.clone()])), a + 2)
        } else {
            (None, a)
        };
        let opens = toks
            .get(at + 1)
            .is_some_and(|t| t.is_punct(sql, "(") || t.is_word(sql, "using"));
        if is(at, "exclude") && opens {
            found.push((name, sql[toks[at].span.start..toks[b - 1].span.end].to_string()));
        } else {
            kept.push(text);
        }
    }
    if found.is_empty() {
        return None;
    }
    let stripped = [
        &sql[..toks[open].span.end],
        &kept.join(", "),
        &sql[toks[close].span.start..],
    ]
    .concat();
    Some((stripped, found))
}

impl Session {
    /// Adds an EXCLUDE constraint to a table: the rows already there are held to it,
    /// then its triggers hold every row written from now on.
    pub(crate) fn add_exclusion(&mut self, table: &str, x: &Exclusion) -> Result<()> {
        let (_, relation) = split_stored_name(table);
        let columns = self.column_names(table)?;
        if columns.is_empty() {
            return Err(errf!(
                SqlState::UNDEFINED_TABLE,
                "relation \"{relation}\" does not exist"
            ));
        }
        let name = x.name.clone().unwrap_or_else(|| {
            let cols: Vec<&str> = x.elems.iter().map(|e| e.column.as_deref().unwrap_or("expr")).collect();
            format!("{relation}_{}_excl", cols.join("_"))
        });
        let mut conds_new = Vec::new();
        let mut conds_pair = Vec::new();
        for e in &x.elems {
            let expr = translate(&e.expr);
            let old = qualify(&expr, &columns, "_overlite_o");
            let (Some(new), Some(pair)) = (
                operator_sql(&e.op, &old, &qualify(&expr, &columns, "NEW")),
                operator_sql(
                    &e.op,
                    &qualify(&expr, &columns, "_overlite_a"),
                    &qualify(&expr, &columns, "_overlite_b"),
                ),
            ) else {
                return Err(errf!(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "exclusion constraint operator {} is not supported",
                    e.op.trim()
                ));
            };
            conds_new.push(new);
            conds_pair.push(pair);
        }
        let pred = x.predicate.as_deref().map(translate);
        let pred_on = |alias: &str| {
            pred.as_deref()
                .map(|p| "(".to_string() + &qualify(p, &columns, alias) + ")")
        };

        // The rows already there.
        let mut probe = "SELECT 1 FROM ".to_string()
            + &quote_ident(table)
            + " AS _overlite_a JOIN "
            + &quote_ident(table)
            + " AS _overlite_b ON _overlite_a.rowid < _overlite_b.rowid WHERE "
            + &conds_pair.join(" AND ");
        if let (Some(a), Some(b)) = (pred_on("_overlite_a"), pred_on("_overlite_b")) {
            probe = probe + " AND " + &a + " AND " + &b;
        }
        if !self.exec(&(probe + " LIMIT 1"), &[])?.rows.is_empty() {
            return Err(errf!(
                SqlState::EXCLUSION_VIOLATION,
                "could not create exclusion constraint \"{name}\""
            ));
        }

        let cols: Vec<String> = x
            .elems
            .iter()
            .filter_map(|e| e.column.as_deref())
            .map(json_str)
            .collect();
        let mark = format!(
            r#"{MARK}{{"name":{},"def":{},"cols":[{}],"deferrable":{},"deferred":{}}}*/"#,
            json_str(&name),
            json_str(&x.def),
            cols.join(","),
            x.deferrable,
            x.deferred
        );
        let message = quote_literal(&format!(
            "conflicting key value violates exclusion constraint \"{name}\""
        ));
        for (event, tag, extra) in [
            ("INSERT", "i", ""),
            ("UPDATE", "u", " AND _overlite_o.rowid <> OLD.rowid"),
        ] {
            let trigger = quote_ident(&format!("{TRIGGER_PREFIX}{tag}_{table}_{name}"));
            let when = pred_on("NEW").map_or(String::new(), |p| " WHEN ".to_string() + &p);
            let mut body = "SELECT RAISE(ABORT, ".to_string()
                + &message
                + ") FROM "
                + &quote_ident(table)
                + " AS _overlite_o WHERE "
                + &conds_new.join(" AND ")
                + extra;
            if let Some(p) = pred_on("_overlite_o") {
                body = body + " AND " + &p;
            }
            let ddl = format!(
                "CREATE TRIGGER {trigger} BEFORE {event} ON {} FOR EACH ROW{when} BEGIN {mark} {body} LIMIT 1; END",
                quote_ident(table)
            );
            self.exec(&ddl, &[])?;
        }
        Ok(())
    }

    /// Drops the EXCLUDE constraint called name, if the table has one: true when it did.
    pub(crate) fn drop_exclusion(&mut self, table: &str, name: &str) -> Result<bool> {
        let rs = self.exec(
            &("SELECT name FROM main.sqlite_master WHERE type = 'trigger' AND lower(tbl_name) = lower(".to_string()
                + &quote_literal(table)
                + ") AND name GLOB '"
                + TRIGGER_PREFIX
                + "*' AND json_extract(overlite_exclusion(sql), '$.name') = "
                + &quote_literal(name)),
            &[],
        )?;
        let names: Vec<String> = rs.rows.iter().map(|r| r[0].to_string()).collect();
        for trigger in &names {
            self.exec(&("DROP TRIGGER ".to_string() + &quote_ident(trigger)), &[])?;
        }
        Ok(!names.is_empty())
    }

    /// The columns of a table, in order.
    fn column_names(&mut self, table: &str) -> Result<Vec<String>> {
        let rs = self.exec(
            &("SELECT name FROM pragma_table_info(".to_string() + &quote_literal(table) + ")"),
            &[],
        )?;
        Ok(rs.rows.iter().map(|r| r[0].to_string()).collect())
    }

    /// CREATE TABLE with EXCLUDE constraints in its list: the table without them, then
    /// each, in one transaction.
    pub(crate) fn create_with_exclusions(&mut self, sql: &str) -> Option<Result<Command>> {
        let (stripped, found) = split_create_exclusions(sql)?;
        let table = create_table_name(&stripped)?;
        Some(self.in_one_transaction(|s| {
            let parsed: Vec<Exclusion> = found
                .iter()
                .map(|(name, clause)| parse_exclusion(name.clone(), clause))
                .collect::<Result<_>>()?;
            let with_defaults = s.apply_column_defaults(&stripped);
            let rewritten = s.rewrite_for_exec(&with_defaults, &[])?;
            s.exec(&rewritten, &[])?;
            s.record_ownership(&with_defaults);
            let stored = s.stored_name(&table);
            for x in &parsed {
                s.add_exclusion(&stored, x)?;
            }
            Ok(Command::from("CREATE TABLE"))
        }))
    }

    /// Runs f inside the open transaction, or in one of its own that commits when it
    /// succeeds.
    pub(crate) fn in_one_transaction<T>(&mut self, f: impl FnOnce(&mut Session) -> Result<T>) -> Result<T> {
        let implicit = !self.in_tx();
        if implicit {
            self.apply_tx_control(crate::session::utility::TxControl::Begin)?;
        }
        let done = f(self);
        if implicit {
            let ended = self.end_tx(done.is_ok());
            let value = done?;
            ended?;
            return Ok(value);
        }
        done
    }
}

/// The table a CREATE TABLE names, as written.
fn create_table_name(sql: &str) -> Option<String> {
    let toks = code_tokens(sql);
    let mut k = toks.iter().position(|t| t.is_word(sql, "table"))? + 1;
    if toks.get(k).is_some_and(|t| t.is_word(sql, "if")) {
        k += 3;
    }
    let mut name = sql[toks.get(k)?.span.clone()].to_string();
    while toks.get(k + 1).is_some_and(|t| t.is_punct(sql, ".")) {
        name = name + "." + &sql[toks.get(k + 2)?.span.clone()];
        k += 2;
    }
    Some(name)
}

/// Whether a CREATE TABLE has an EXCLUDE constraint, which create_with_exclusions runs.
pub(crate) fn creates_exclusions(sql: &str) -> bool {
    crate::ast::classify::read(sql, crate::ast::classify::creates_exclusions).unwrap_or(true)
        && split_create_exclusions(sql).is_some()
}

/// `creates_exclusions` by the text alone.
#[cfg(test)]
pub(crate) fn creates_exclusions_text(sql: &str) -> bool {
    split_create_exclusions(sql).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_exclusion() {
        let x = parse_exclusion(
            Some("x".into()),
            r#"EXCLUDE USING GIST ((TSTZRANGE("start", "end", '[)')) range_ops WITH &&, "room_id" gist_int8_ops WITH =) WHERE (NOT "cancelled") DEFERRABLE INITIALLY DEFERRED"#,
        )
        .unwrap();
        assert_eq!(x.elems.len(), 2);
        assert_eq!(x.elems[0].expr, r#"TSTZRANGE("start", "end", '[)')"#);
        assert_eq!(x.elems[0].op, "&&");
        assert_eq!(x.elems[1].column.as_deref(), Some("room_id"));
        assert_eq!(x.elems[1].op, "=");
        assert_eq!(x.predicate.as_deref(), Some(r#"NOT "cancelled""#));
        assert!(x.deferrable && x.deferred);
        let y = parse_exclusion(None, r#"EXCLUDE USING spgist ("ints" WITH -|-) INCLUDE ("decimals")"#).unwrap();
        assert_eq!(y.elems[0].op, "-|-");
        assert!(parse_exclusion(None, "EXCLUDE (a WITH =) WHAT").is_err());
    }

    #[test]
    fn qualifies_columns() {
        let cols = vec!["a".to_string(), "Room".to_string()];
        assert_eq!(
            qualify(r#"overlite_x(a, "Room", 'a') + lower(b)"#, &cols, "NEW"),
            r#"overlite_x(NEW."a", NEW."Room", 'a') + lower(b)"#
        );
    }

    #[test]
    fn splits_a_create_table() {
        let (sql, found) = split_create_exclusions(
            "CREATE TABLE t (id int, r int4range, CONSTRAINT t_x EXCLUDE USING gist (r WITH &&), n int)",
        )
        .unwrap();
        assert_eq!(sql, "CREATE TABLE t (id int, r int4range, n int)");
        assert_eq!(
            found,
            vec![(Some("t_x".to_string()), "EXCLUDE USING gist (r WITH &&)".to_string())]
        );
        assert!(split_create_exclusions("CREATE TABLE t (exclude int)").is_none());
    }
}
