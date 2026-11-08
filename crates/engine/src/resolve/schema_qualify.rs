//! In single-file mode a schema-qualified name like `vendas.pedidos` must reach
//! SQLite as the single quoted identifier `"vendas.pedidos"` (the real table
//! name). This is the one place that distinguishes a `schema.table` qualifier
//! from an `alias.column` one: only a qualifier whose first part is a
//! *registered schema* is rewritten. The registered-schema set is held per
//! database so the rewrite doesn't hit the database per statement.

use crate::catalog::quote_ident_if_needed;
use crate::catalog::rls_cache::ascii_lower;
use crate::db_state::DbState;
use overlite_core::lazy_regex;
use overlite_core::text::{eq_fold, string_from_cut_utf8, trim_chars};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

/// A rewrite's buffer as a String: `b` holds runs and single bytes
/// copied in order from valid UTF-8 strings, cut only at ASCII delimiters.
#[inline]
pub(crate) fn builder_string(b: Vec<u8>) -> String {
    string_from_cut_utf8(b)
}

/// Lowers s, returning it as is when there is nothing to fold.
#[inline]
pub(crate) fn lowercase(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        ascii_lower(s)
    } else {
        Cow::Owned(s.to_lowercase())
    }
}

/// Rewrites `<schema>.<name>` to `"<schema>.<name>"` for every registered
/// schema, outside string/identifier literals. A no-op when no schema is
/// registered.
pub(crate) fn qualify_schema_names(st: &DbState, query: &str) -> String {
    if !query.contains('.') {
        return query.to_string();
    }
    let schemas = st.schema_list();
    let mut query = query.to_string();
    for s in schemas.iter() {
        query = qualify_one_schema(&query, s);
    }
    prefix_schema_index_name(query)
}

// Captures a CREATE INDEX's index name and its (already-qualified) target table.
lazy_regex!(
    RE_CREATE_INDEX,
    r#"(?is)^(\s*CREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:IF\s+NOT\s+EXISTS\s+)?)("[^"]+"|[A-Za-z_]\w*)(\s+ON\s+)("[^"]+")"#
);

/// Gives an index on a schema table the same "<schema>." prefix as the table, so
/// it belongs to that schema (and pg_index/\d find it). The target has already
/// been rewritten to "<schema>.<table>" by this point.
fn prefix_schema_index_name(query: String) -> String {
    let rewritten = {
        let Some(m) = RE_CREATE_INDEX.captures(&query) else {
            return query;
        };
        let (schema, _) = split_stored_name(trim_chars(&m[4], "\""));
        if schema.is_empty() {
            None // target isn't schema-qualified: index stays in public
        } else {
            let idx = trim_chars(&m[2], "\"");
            let new_name = format!("\"{schema}.{idx}\"");
            let end = m.get_match().end();
            Some(format!("{}{}{}{}{}", &m[1], new_name, &m[3], &m[4], &query[end..]))
        }
    };
    rewritten.unwrap_or(query)
}

/// Splits a stored table name "schema.table" into its parts; a name without a
/// dot has an empty schema.
pub fn split_stored_name(name: &str) -> (String, String) {
    if let Some(i) = name.find('.') {
        return (name[..i].to_string(), name[i + 1..].to_string());
    }
    (String::new(), name.to_string())
}

fn qualify_one_schema(sql: &str, schema: &str) -> String {
    let s = sql.as_bytes();
    let mut b: Vec<u8> = Vec::new();
    let mut prev: Cow<'_, str> = Cow::Borrowed("");
    let mut prev2: Cow<'_, str> = Cow::Borrowed("");
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\'' {
            let j = skip_literal(sql, i, b'\'');
            b.extend_from_slice(&s[i..j]);
            i = j;
            prev2 = std::mem::replace(&mut prev, Cow::Borrowed("'"));
            continue;
        }
        // A qualifier starts at a word boundary. Anything glued to an
        // identifier byte or a dot belongs to a longer name.
        if (i == 0 || (!is_ident_byte(s[i - 1]) && s[i - 1] != b'.'))
            && let Some((table, end)) = match_qualifier(sql, i, schema)
        {
            b.extend_from_slice(quote_ident_if_needed(&format!("{schema}.{table}")).as_bytes());
            // The statement may go on to qualify columns with the bare table
            // name, which every ORM does: `FROM sales.orders WHERE orders.id
            // = $1`. Renaming the table would strand those, so the old name
            // is kept as an alias.
            if needs_alias(sql, end, &prev, &prev2) {
                // Quoted whatever it looks like: `add` is a word SQLite reserves.
                b.extend_from_slice(format!(" AS \"{}\"", table.replace('"', "\"\"")).as_bytes());
            }
            i = end;
            prev2 = std::mem::replace(&mut prev, table);
            continue;
        }
        if s[i] == b'"' {
            let j = skip_literal(sql, i, b'"');
            b.extend_from_slice(&s[i..j]);
            let word = trim_chars(&sql[i..j], "\"");
            i = j;
            prev2 = std::mem::replace(&mut prev, lowercase(word));
            continue;
        }
        if is_ident_byte(s[i]) {
            let start = i;
            while i < s.len() && is_ident_byte(s[i]) {
                i += 1;
            }
            b.extend_from_slice(&s[start..i]);
            prev2 = std::mem::replace(&mut prev, lowercase(&sql[start..i]));
            continue;
        }
        let c = s[i];
        b.push(c);
        i += 1;
        if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
            continue; // whitespace keeps the keyword context
        }
        prev2 = Cow::Borrowed("");
        prev = Cow::Borrowed("");
    }
    builder_string(b)
}

/// Whether a rewritten table reference should carry its original name as an
/// alias: it must sit where a table may be aliased, and not already have one.
fn needs_alias(sql: &str, end: usize, prev: &str, prev2: &str) -> bool {
    match prev {
        "from" | "join" | "update" => {}
        "into" => {
            if prev2 != "insert" {
                return false;
            }
        }
        _ => return false, // CREATE/ALTER/DROP TABLE take no alias
    }

    let s = sql.as_bytes();
    let mut j = end;
    while j < s.len() && (s[j] == b' ' || s[j] == b'\t' || s[j] == b'\n' || s[j] == b'\r') {
        j += 1;
    }
    if j >= s.len() {
        return true;
    }
    if s[j] == b'"' {
        // an explicitly quoted alias
        return false;
    }
    let start = j;
    while j < s.len() && is_ident_byte(s[j]) {
        j += 1;
    }
    if j == start {
        return true; // punctuation: "(", ",", ";" -- no alias present
    }
    // A word follows: a clause keyword means no alias, anything else is one.
    CLAUSE_KEYWORDS
        .get(lowercase(&sql[start..j]).as_ref())
        .copied()
        .unwrap_or(false)
}

/// The words that may follow a table reference without being an alias for it.
static CLAUSE_KEYWORDS: LazyLock<HashMap<&'static str, bool>> = LazyLock::new(|| {
    HashMap::from([
        ("where", true),
        ("set", true),
        ("values", true),
        ("select", true),
        ("join", true),
        ("inner", true),
        ("left", true),
        ("right", true),
        ("full", true),
        ("cross", true),
        ("natural", true),
        ("on", true),
        ("using", true),
        ("group", true),
        ("order", true),
        ("limit", true),
        ("offset", true),
        ("having", true),
        ("union", true),
        ("returning", true),
        ("default", true),
        ("as", false),
    ])
});

/// Reads a `<schema>.<name>` qualifier at i, in any of the four quoting
/// combinations a client may send. Returns the unquoted table name and the index
/// just past the qualifier.
fn match_qualifier<'a>(sql: &'a str, i: usize, schema: &str) -> Option<(Cow<'a, str>, usize)> {
    let (name, j) = read_ident(sql, i)?;
    if !eq_fold(&name, schema) {
        return None;
    }
    let s = sql.as_bytes();
    if j >= s.len() || s[j] != b'.' {
        return None;
    }
    // `sales.*` and the like are not a table qualifier.
    read_ident(sql, j + 1)
}

/// Reads one identifier at i, quoted or bare, and returns its value with any
/// quoting removed.
fn read_ident(sql: &str, i: usize) -> Option<(Cow<'_, str>, usize)> {
    let s = sql.as_bytes();
    if i >= s.len() {
        return None;
    }
    if s[i] == b'"' {
        let j = skip_literal(sql, i, b'"');
        if j <= i + 1 || s[j - 1] != b'"' {
            return None; // unterminated
        }
        let inner = &sql[i + 1..j - 1];
        if inner.contains("\"\"") {
            return Some((Cow::Owned(inner.replace("\"\"", "\"")), j));
        }
        return Some((Cow::Borrowed(inner), j));
    }
    let start = i;
    let mut i = i;
    while i < s.len() && is_ident_byte(s[i]) {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some((Cow::Borrowed(&sql[start..i]), i))
}

/// The index just past a quoted run starting at i (quote q), honoring
/// doubled-quote escapes.
pub(crate) fn skip_literal(s: &str, i: usize, q: u8) -> usize {
    let b = s.as_bytes();
    let mut i = i + 1; // opening quote
    while i < b.len() {
        if b[i] == q {
            if i + 1 < b.len() && b[i + 1] == q {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

#[inline]
pub(crate) fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_lowercase() || b.is_ascii_uppercase() || b.is_ascii_digit()
}
