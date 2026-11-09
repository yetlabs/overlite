//! A cast to a type SQLite has no such notion of is `overlite_cast(x, 'type')`, the
//! engine's input functions: SQLite's CAST goes by affinity, and reads
//! `'2024-01-02 03:04:05'` as 2024, `'abc'` as 0, and a uuid as a float.
//!
//! A table's DDL keeps SQLite's own CAST, so the database file still opens without
//! overlite's functions: a default that casts a literal is folded here, to its value.
//! The rest of DDL holds a query, or an index on an expression, which needs the type.

use crate::auth::rls::top_level_keyword;
use crate::ddl::sequences::split_top_level;
use crate::rewrite::bool_cast::{is_cast_at, last_top_level_as};
use crate::session::utility::{first_word_upper, second_word_upper};
use crate::types::param_types::closing;
use overlite_core::text::{quote_ident, quote_literal, trim_end_chars};
use overlite_core::{Value, parse_bool};
use overlite_engine::{CastType, cast, code_only};

/// Whether sql is a table's definition, which keeps SQLite's CAST.
fn is_table_ddl(sql: &str) -> bool {
    match first_word_upper(sql).as_str() {
        "ALTER" => true,
        "CREATE" => second_word_upper(sql) == "TABLE" && !code_only(sql).to_ascii_uppercase().contains(" AS SELECT"),
        _ => false,
    }
}

/// What a CAST to type_name becomes, given its operand.
fn typed_cast(operand: &str, type_name: &str, table_ddl: bool) -> Option<String> {
    let name = type_name.trim();
    if name.eq_ignore_ascii_case("text") || name.ends_with(']') {
        return None;
    }
    if name.eq_ignore_ascii_case("hstore") {
        // As a `::hstore` is: a literal is read, and anything else is one already.
        let is_literal = operand.trim_start().starts_with('\'');
        return Some(if is_literal {
            ["hstore_in(", operand, ")"].concat()
        } else {
            operand.to_string()
        });
    }
    if !table_ddl {
        return Some(["overlite_cast(", operand, ", ", &quote_literal(name), ")"].concat());
    }
    fold_literal(operand, name)
}

/// The value of a literal under a cast, as a literal. None when it is not one or fails
/// to convert, which leaves it to SQLite.
pub(crate) fn fold_literal(operand: &str, name: &str) -> Option<String> {
    let body = operand.strip_prefix('\'')?.strip_suffix('\'')?;
    if body.replace("''", "").contains('\'') {
        return None;
    }
    if name.eq_ignore_ascii_case("boolean") || name.eq_ignore_ascii_case("bool") {
        // As the storage format keeps one.
        return parse_bool(body).map(|b| if b { "1" } else { "0" }.to_string());
    }
    let value = Value::Text(body.replace("''", "'"));
    match cast(&value, &CastType::parse(name)?).ok()? {
        Value::Text(s) => Some(quote_literal(&s)),
        Value::Int(n) => Some(n.to_string()),
        Value::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

/// Turns each CAST(x AS type) that SQLite would get wrong into its overlite_cast. It
/// runs after rewrite_casts, which writes x::type as a CAST, and after every pass that
/// looks for a CAST by its type.
pub(crate) fn rewrite_typed_casts(sql: &str) -> String {
    if !sql.to_ascii_uppercase().contains("CAST") {
        return sql.to_string();
    }
    let table_ddl = is_table_ddl(sql);
    let out = rewrite_within(sql, table_ddl);
    if table_ddl { out } else { label_select_list(&out) }
}

/// What Postgres calls a result column that is a cast: what the operand is called if
/// it is a column, and otherwise the type.
fn cast_label(operand: &str, type_name: &str) -> String {
    let op = operand.trim();
    if let Some((inner, ty)) = cast_parts(op) {
        let inner_label = cast_label(inner, &ty);
        let of_type = CastType::parse(&ty).is_some_and(|t| t.label() == inner_label);
        if !of_type && inner_label != ty.to_ascii_lowercase() {
            return inner_label;
        }
    }
    let last = op.rsplit('.').next().unwrap_or(op);
    if let Some(name) = last.strip_prefix('"').and_then(|l| l.strip_suffix('"')) {
        return name.replace("\"\"", "\"");
    }
    let is_name = last
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && last
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$');
    if is_name && !matches!(last.to_ascii_lowercase().as_str(), "null" | "true" | "false") {
        return last.to_ascii_lowercase();
    }
    CastType::parse(type_name).map_or_else(|| type_name.to_ascii_lowercase(), |ty| ty.label().to_string())
}

/// The operand and the type of what is one overlite_cast call and nothing else.
fn cast_parts(item: &str) -> Option<(&str, String)> {
    let open = item.strip_prefix("overlite_cast")?.len();
    let open = item.len() - open;
    if closing(&item[open..])? + open != item.len() - 1 {
        return None;
    }
    let inner = &item[open + 1..item.len() - 1];
    let comma = inner.rfind(", '")?;
    let name = inner[comma + 3..].strip_suffix('\'')?.replace("''", "'");
    Some((&inner[..comma], name))
}

/// Names the casts of the outermost select list, which would otherwise be labelled
/// with the text of the call. Words that open the next clause end the list.
fn label_select_list(sql: &str) -> String {
    if first_word_upper(sql) != "SELECT" || !sql.contains("overlite_cast(") {
        return sql.to_string();
    }
    let Some(start) = top_level_keyword(sql, "select").map(|s| s + "select".len()) else {
        return sql.to_string();
    };
    let mut end = trim_end_chars(sql, "; \t\r\n").len();
    for word in [
        "from",
        "where",
        "group",
        "having",
        "order",
        "limit",
        "offset",
        "union",
        "except",
        "intersect",
    ] {
        if let Some(p) = top_level_keyword(&sql[start..], word) {
            end = end.min(start + p);
        }
    }
    let list = &sql[start..end];
    let items: Vec<String> = split_top_level(list)
        .iter()
        .map(|item| match cast_parts(item.trim()) {
            Some((operand, ty)) => {
                let (head, tail) = (&item[..item.len() - item.trim_start().len()], item.trim_start());
                let tail = tail.trim_end();
                let pad = &item[item.trim_end().len()..];
                format!("{head}{tail} AS {}{pad}", quote_ident(&cast_label(operand, &ty)))
            }
            None => item.clone(),
        })
        .collect();
    [&sql[..start], &items.join(","), &sql[end..]].concat()
}

fn rewrite_within(sql: &str, table_ddl: bool) -> String {
    let s = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut copied = 0;
    let (mut quote, mut i) = (0u8, 0);
    while i < s.len() {
        let c = s[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'\'' || c == b'"' {
            quote = c;
        } else if is_cast_at(sql, i)
            && let Some(open) = sql[i..].find('(').map(|o| o + i)
            && let Some(end) = closing(&sql[open..]).map(|e| open + e).filter(|e| *e > open)
        {
            let inside = &sql[open + 1..end];
            if let Some(at) = last_top_level_as(inside) {
                let operand = rewrite_within(trim_end_chars(inside[..at].trim(), " "), table_ddl);
                let type_name = &inside[at + 2..];
                let done = typed_cast(&operand, type_name, table_ddl)
                    .unwrap_or_else(|| ["CAST(", &operand, " AS", &inside[at + 2..], ")"].concat());
                out.push_str(&sql[copied..i]);
                out.push_str(&done);
                copied = end + 1;
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&sql[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::assert_rewrites;

    #[test]
    fn casts_become_the_engine_function() {
        assert_rewrites(
            rewrite_typed_casts,
            &[
                ("SELECT CAST(x AS date)", "SELECT overlite_cast(x, 'date') AS \"x\""),
                (
                    "SELECT CAST(CAST(x AS timestamp) AS date) FROM t",
                    "SELECT overlite_cast(overlite_cast(x, 'timestamp'), 'date') AS \"x\" FROM t",
                ),
                ("SELECT CAST(x AS text)", "SELECT CAST(x AS text)"),
                (
                    "SELECT CAST(CAST(x AS int) AS text)",
                    "SELECT CAST(overlite_cast(x, 'int') AS text)",
                ),
                (
                    "SELECT CAST('a' AS date), t.b",
                    "SELECT overlite_cast('a', 'date') AS \"date\", t.b",
                ),
                (
                    "SELECT CAST(t.\"B\" AS int) AS c",
                    "SELECT overlite_cast(t.\"B\", 'int') AS c",
                ),
                (
                    "SELECT 1 + CAST(x AS int) FROM t",
                    "SELECT 1 + overlite_cast(x, 'int') FROM t",
                ),
                ("SELECT 'CAST(x AS date)'", "SELECT 'CAST(x AS date)'"),
                ("SELECT \"CAST(x AS date)\"", "SELECT \"CAST(x AS date)\""),
                (
                    "SELECT 1, CAST('a b' AS uuid)",
                    "SELECT 1, overlite_cast('a b', 'uuid') AS \"uuid\"",
                ),
                (
                    "SELECT CAST(x AS numeric(10, 2)) WHERE 1",
                    "SELECT overlite_cast(x, 'numeric(10, 2)') AS \"x\" WHERE 1",
                ),
                (
                    "CREATE VIEW v AS SELECT CAST(x AS date) FROM t",
                    "CREATE VIEW v AS SELECT overlite_cast(x, 'date') FROM t",
                ),
            ],
        );
    }

    #[test]
    fn table_ddl_folds_a_literal_and_leaves_the_rest() {
        assert_rewrites(
            rewrite_typed_casts,
            &[
                (
                    "CREATE TABLE t (a timestamp DEFAULT CAST('2024-01-02 03:04:05' AS timestamp))",
                    "CREATE TABLE t (a timestamp DEFAULT '2024-01-02 03:04:05')",
                ),
                (
                    "CREATE TABLE t (a date CHECK (CAST(a AS date) > '2000-01-01'))",
                    "CREATE TABLE t (a date CHECK (CAST(a AS date) > '2000-01-01'))",
                ),
                (
                    "CREATE TABLE t AS SELECT CAST(x AS date) FROM s",
                    "CREATE TABLE t AS SELECT overlite_cast(x, 'date') FROM s",
                ),
            ],
        );
    }
}
