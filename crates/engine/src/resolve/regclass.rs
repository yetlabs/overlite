//! to_regclass(name) resolves a relation name to its oid, or NULL. It is resolved
//! per statement rather than registered as a SQLite function because the answer
//! depends on the connection's schemas.

use crate::resolve::current_schema::replace_call_outside_strings;
use crate::resolve::schema_qualify::lowercase;
use overlite_core::text::quote_literal;

/// Replaces to_regclass('<name>') with a subquery over the catalog, so the
/// result is an oid or NULL exactly as Postgres gives it.
pub(crate) fn resolve_to_regclass(query: &str) -> String {
    if !lowercase(query).contains("to_regclass") {
        return query.to_string();
    }
    replace_call_outside_strings(query, "to_regclass", |args| {
        let expr = args.trim();
        if let Some(name) = string_literal(expr) {
            let (schema, table) = split_relation_name(&name);
            let mut where_ = format!("c.relname = {}", quote_literal(&table));
            if !schema.is_empty() {
                where_ += &format!(" AND n.nspname = {}", quote_literal(&schema));
            }
            return format!(
                "(SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE {where_} LIMIT 1)"
            );
        }
        // A bound parameter or any other expression is split in SQL instead. It
        // is named once, in a derived table, so a placeholder is not duplicated
        // into a second bind position.
        format!(
            "(SELECT c.oid FROM (SELECT {expr} AS v) AS _r JOIN pg_namespace n JOIN pg_class c ON c.relnamespace = n.oid \
WHERE CASE WHEN instr(_r.v, '.') > 0  THEN n.nspname = substr(_r.v, 1, instr(_r.v, '.') - 1)   \
AND c.relname = substr(_r.v, instr(_r.v, '.') + 1)  ELSE c.relname = _r.v END LIMIT 1)"
        )
    })
}

/// Unwraps a single-quoted SQL literal.
fn string_literal(expr: &str) -> Option<String> {
    let b = expr.as_bytes();
    if b.len() < 2 || b[0] != b'\'' || b[b.len() - 1] != b'\'' {
        return None;
    }
    Some(expr[1..expr.len() - 1].replace("''", "'"))
}

/// Splits an optionally schema-qualified relation name, dropping the quoting
/// Postgres allows on either part.
fn split_relation_name(name: &str) -> (String, String) {
    if let Some(i) = name.rfind('.') {
        return (unquote_relation_part(&name[..i]), unquote_relation_part(&name[i + 1..]));
    }
    (String::new(), unquote_relation_part(name))
}

fn unquote_relation_part(s: &str) -> String {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        return s[1..s.len() - 1].replace("\"\"", "\"");
    }
    s.to_string()
}
