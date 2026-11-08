//! `::` casts, turned into CAST.

use crate::rewrite::splice;
use crate::rewrite::tokens::{CodeTokKind, code_tokens};
use overlite_core::text::{quote_literal, trim_chars};

/// Turns every Postgres "operand::type" cast into SQLite's CAST(operand AS
/// type). A hand-written scan (rather than a regex) is used so the operand can
/// be a balanced parenthesized group, a quoted string, or a chained cast.
pub(crate) fn rewrite_casts(sql: &str) -> String {
    let mut sql = sql.to_string();
    loop {
        let Some(i) = index_cast(&sql) else { return sql };
        let b = sql.as_bytes();
        let mut ts = i + 2;
        while ts < b.len() && b[ts] == b' ' {
            ts += 1;
        }
        let Some((type_name, mut te)) = read_type_name(&sql, ts) else {
            return sql; // unterminated
        };
        // Drop array-type suffixes (int2[], text[][]); SQLite has no arrays and
        // these appear only in catalog queries that return no rows here.
        while b[te..].starts_with(b"[]") {
            te += 2;
        }
        let (os, oe) = operand_before(&sql, i);
        if type_name.is_empty() || os == oe {
            return sql; // no type name or no operand: leave it, avoid looping
        }
        let repl = cast_expr(&sql[os..oe], &type_name);
        sql = splice(&sql, os, te, &repl);
    }
}

/// The word or words of the type name starting at ts with its modifier, as they are
/// to be written in a CAST, and the index past it. None for a quote left open.
pub(crate) fn read_type_name(sql: &str, ts: usize) -> Option<(String, usize)> {
    let b = sql.as_bytes();
    if b.get(ts) == Some(&b'"') {
        // Quoted type name, e.g. 's'::"char". Use the unquoted name.
        let j = ts + 1 + sql[ts + 1..].find('"')?;
        return Some((sql[ts + 1..j].to_string(), j + 1));
    }
    let word_end = |from: usize| (from..b.len()).find(|&j| !is_type_char(b[j])).unwrap_or(b.len());
    // A dot leaves the schema behind: `public.t` names t.
    let mut start = ts;
    let mut te = word_end(start);
    while b.get(te) == Some(&b'.') {
        start = te + 1;
        te = word_end(start);
    }
    let mut name = sql[start..te].to_string();
    let next_word = |from: usize| -> Option<(usize, usize)> {
        let p = (from..b.len()).find(|&j| b[j] != b' ')?;
        (p > from).then_some((p, word_end(p)))
    };
    let lower = name.to_ascii_lowercase();
    let second: &[&str] = match lower.as_str() {
        "double" => &["precision"],
        "character" | "char" | "bit" => &["varying"],
        _ => &[],
    };
    if let Some((p, e)) = next_word(te)
        && second.iter().any(|w| sql[p..e].eq_ignore_ascii_case(w))
    {
        name = [name.as_str(), " ", &sql[p..e]].concat();
        te = e;
    }
    // (3) and (10, 2); anything else in parentheses is not a modifier.
    if let Some(p) = (te..b.len()).find(|&j| b[j] != b' ')
        && b[p] == b'('
        && let Some(close) = sql[p..].find(')').map(|c| p + c)
        && close > p + 1
        && sql[p + 1..close]
            .bytes()
            .all(|c| c.is_ascii_digit() || c == b',' || c == b' ' || c == b'-')
    {
        name.push_str(&sql[p..=close]);
        te = close + 1;
    }
    // timestamp(3) with time zone.
    if (lower == "timestamp" || lower == "time")
        && let Some((p, e)) = next_word(te)
        && (sql[p..e].eq_ignore_ascii_case("with") || sql[p..e].eq_ignore_ascii_case("without"))
        && let Some((tp, te2)) = next_word(e)
        && sql[tp..te2].eq_ignore_ascii_case("time")
        && let Some((zp, ze)) = next_word(te2)
        && sql[zp..ze].eq_ignore_ascii_case("zone")
    {
        name = [name.as_str(), " ", &sql[p..ze]].concat();
        te = ze;
    }
    Some((name, te))
}

/// The type words that make a typed literal of the string after them.
const LITERAL_TYPES: [&str; 28] = [
    "date",
    "time",
    "timetz",
    "timestamp",
    "timestamptz",
    "interval",
    "uuid",
    "json",
    "jsonb",
    "numeric",
    "decimal",
    "int",
    "int2",
    "int4",
    "int8",
    "integer",
    "bigint",
    "smallint",
    "real",
    "float4",
    "float8",
    "bool",
    "boolean",
    "text",
    "varchar",
    "bytea",
    "inet",
    "cidr",
];

/// Turns `type 'text'`, Postgres' other spelling of a cast, into one.
pub(crate) fn rewrite_typed_literals(sql: &str) -> String {
    if !sql.contains('\'') {
        return sql.to_string();
    }
    let toks = code_tokens(sql);
    let is_word = |i: usize, w: &str| toks[i].is_word(sql, w);
    let mut out = String::with_capacity(sql.len() + 16);
    let mut at = 0;
    for (i, t) in toks.iter().enumerate() {
        if t.kind != CodeTokKind::Value || !sql[t.span.clone()].starts_with('\'') || i == 0 {
            continue;
        }
        // timestamp with time zone 'x'; the bare type stands right before the string.
        let first = if i >= 4
            && is_word(i - 1, "zone")
            && is_word(i - 2, "time")
            && (is_word(i - 3, "with") || is_word(i - 3, "without"))
            && (is_word(i - 4, "timestamp") || is_word(i - 4, "time"))
        {
            i - 4
        } else if i >= 2 && is_word(i - 1, "precision") && is_word(i - 2, "double") {
            i - 2
        } else if LITERAL_TYPES.iter().any(|w| is_word(i - 1, w)) {
            i - 1
        } else {
            continue;
        };
        // A column or a table called so stands after a dot, and a label after AS.
        if first > 0 && (toks[first - 1].is_punct(sql, ".") || toks[first - 1].is_word(sql, "as")) {
            continue;
        }
        let start = toks[first].span.start;
        if start < at {
            continue;
        }
        out.push_str(&sql[at..start]);
        out.push_str("CAST(");
        out.push_str(&sql[t.span.clone()]);
        out.push_str(" AS ");
        out.push_str(&sql[start..toks[i - 1].span.end]);
        out.push(')');
        at = t.span.end;
    }
    if at == 0 {
        return sql.to_string();
    }
    out.push_str(&sql[at..]);
    out
}

/// Renders "operand::type". Postgres OID-alias types (regclass, ...) become a
/// name lookup against the catalog so oids display as names, as psql expects;
/// everything else becomes a plain SQLite CAST.
fn cast_expr(operand: &str, type_name: &str) -> String {
    // A reg* cast on a numeric literal is a comparison seed (regclass is an oid
    // under the hood), so keep it numeric. On anything else it's for display,
    // so resolve the oid to its catalog name.
    let reg_lookup = |col: &str, table: &str| -> String {
        let op = operand.trim();
        // A quoted numeric oid ('16384'::regclass) must become a bare integer so
        // it compares equal to integer oid columns (SQLite won't equate 1 = '1').
        if is_numeric_literal(op) {
            return trim_chars(op, "'").to_string();
        }
        // A quoted name ('emp'::regclass, 'public.emp'::regclass) is resolved
        // name -> oid: regclass IS an oid, and callers compare it to oid columns.
        let ob = op.as_bytes();
        if ob.len() >= 2 && ob[0] == b'\'' && ob[ob.len() - 1] == b'\'' {
            return [
                "(SELECT oid FROM ",
                table,
                " WHERE ",
                col,
                " = ",
                &reg_class_name(op),
                ")",
            ]
            .concat();
        }
        // An oid-valued expression (c.oid::regclass) is resolved oid -> name for
        // display, as psql expects.
        ["(SELECT ", col, " FROM ", table, " WHERE oid = ", op, ")"].concat()
    };
    match type_name.trim().to_lowercase().as_str() {
        "regclass" => system_catalog_oid(operand).map_or_else(|| reg_lookup("relname", "pg_class"), str::to_string),
        "regnamespace" => reg_lookup("nspname", "pg_namespace"),
        "regtype" => reg_lookup("typname", "pg_type"),
        "regrole" => reg_lookup("rolname", "pg_roles"),
        "regproc" | "regprocedure" => reg_lookup("proname", "pg_proc"),
        _ => ["CAST(", operand, " AS ", type_name, ")"].concat(),
    }
}

/// The oid of a system catalog named by a literal, `'pg_class'::regclass`, which
/// pg_depend and pg_description go by as classid. pg_class lists no catalog of its own.
fn system_catalog_oid(operand: &str) -> Option<&'static str> {
    let name = operand.trim().strip_prefix('\'')?.strip_suffix('\'')?;
    let name = name.strip_prefix("pg_catalog.").unwrap_or(name);
    Some(match name {
        "pg_class" => "1259",
        "pg_type" => "1247",
        "pg_attribute" => "1249",
        "pg_proc" => "1255",
        "pg_namespace" => "2615",
        "pg_constraint" => "2606",
        "pg_index" => "2610",
        "pg_attrdef" => "2604",
        "pg_trigger" => "2620",
        "pg_rewrite" => "2618",
        "pg_description" => "2609",
        "pg_policy" => "3256",
        "pg_database" => "1262",
        "pg_authid" => "1260",
        _ => return None,
    })
}

/// Turns a quoted relation name literal ('public."My Table"') into a SQL string
/// of the bare identifier ('My Table'), dropping the surrounding single quotes,
/// any schema qualifier and identifier double quotes.
fn reg_class_name(lit: &str) -> String {
    let mut s = trim_chars(lit.trim(), "'");
    if !s.contains('"')
        && let Some(i) = s.rfind('.')
    {
        s = &s[i + 1..];
    }
    quote_literal(trim_chars(s, "\""))
}

/// Reports whether s is an integer literal, optionally quoted (e.g. 16384 or
/// '16384').
pub(crate) fn is_numeric_literal(s: &str) -> bool {
    let s = s.trim();
    let s = s.strip_prefix('\'').unwrap_or(s);
    let s = s.strip_suffix('\'').unwrap_or(s);
    if s.is_empty() {
        return false;
    }
    s.bytes().all(|c| c.is_ascii_digit())
}

/// The index of the first "::" that is not inside a string literal.
fn index_cast(sql: &str) -> Option<usize> {
    let b = sql.as_bytes();
    let mut in_str = false;
    let mut i = 0;
    while i + 1 < b.len() {
        match b[i] {
            b'\'' => in_str = !in_str,
            b':' if !in_str && b[i + 1] == b':' => {
                return Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Returns the [start,end) span of the operand ending just before the "::" at
/// cast_idx. An unmatched quote or parenthesis to the left leaves no operand: an empty
/// span, which every caller leaves alone for SQLite to refuse.
pub(crate) fn operand_before(sql: &str, cast_idx: usize) -> (usize, usize) {
    let s = sql.as_bytes();
    let mut end = cast_idx;
    while end > 0 && s[end - 1] == b' ' {
        end -= 1;
    }
    if end == 0 {
        return (end, end);
    }
    match s[end - 1] {
        b')' => {
            let mut depth = 0;
            let mut j = end as isize - 1;
            while j >= 0 {
                if s[j as usize] == b')' {
                    depth += 1;
                } else if s[j as usize] == b'(' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j -= 1;
            }
            if j < 0 {
                return (end, end);
            }
            let j = j as usize;
            // Include a preceding function name, so CAST(x)::t keeps the CAST.
            let mut name = j;
            while name > 0 && is_operand_char(s[name - 1]) {
                name -= 1;
            }
            // An aggregate's FILTER (...) or a window's OVER (...) is not a call:
            // the operand is the whole expression, count(*) FILTER (...) OVER (...).
            let mut kw = j;
            while kw > 0 && s[kw - 1] == b' ' {
                kw -= 1;
            }
            let mut word = kw;
            while word > 0 && is_operand_char(s[word - 1]) {
                word -= 1;
            }
            let w = sql[word..kw].to_uppercase();
            if w == "FILTER" || w == "OVER" {
                name = word;
                let mut prev = name;
                while prev > 0 && s[prev - 1] == b' ' {
                    prev -= 1;
                }
                if prev > 0 && s[prev - 1] == b')' {
                    let (start, _) = operand_before(sql, prev);
                    return (start, end);
                }
            }
            (name, end)
        }
        b'\'' => {
            let mut j = end as isize - 2;
            loop {
                while j >= 0 && s[j as usize] != b'\'' {
                    j -= 1;
                }
                // A doubled quote is inside the literal.
                if j >= 1 && s[j as usize - 1] == b'\'' {
                    j -= 2;
                    continue;
                }
                break;
            }
            if j < 0 {
                return (end, end);
            }
            (j as usize, end)
        }
        _ => (ident_chain_start(sql, end), end),
    }
}

/// Where the name ending at end starts: a run of name characters and quoted
/// identifiers joined by dots, so "public"."T"."c", t."c" and "t".c are each one
/// operand. A quoted identifier ends at its closing quote, a doubled quote staying
/// inside it.
fn ident_chain_start(sql: &str, end: usize) -> usize {
    let s = sql.as_bytes();
    let mut j = end;
    while j > 0 {
        let c = s[j - 1];
        if c == b'"' {
            let mut k = j as isize - 2;
            while k >= 0 {
                if s[k as usize] == b'"' {
                    if k > 0 && s[k as usize - 1] == b'"' {
                        k -= 2; // "" inside the name
                        continue;
                    }
                    break;
                }
                k -= 1;
            }
            if k < 0 {
                return j; // unbalanced: stop at what we have
            }
            j = k as usize;
        } else if is_operand_char(c) {
            j -= 1; // letters, digits, _, $, and the dots between parts
        } else {
            return j;
        }
    }
    j
}

fn is_type_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_lowercase() || b.is_ascii_uppercase() || b.is_ascii_digit()
}

pub(crate) fn is_operand_char(b: u8) -> bool {
    b == b'_' || b == b'$' || b == b'.' || b.is_ascii_lowercase() || b.is_ascii_uppercase() || b.is_ascii_digit()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::{assert_rewrites, rewrite};

    #[test]
    fn rewrite_casts_cases() {
        assert_rewrites(
            rewrite_casts,
            &[
                ("SELECT $1::int", "SELECT CAST($1 AS int)"),
                ("SELECT $1::int + 1", "SELECT CAST($1 AS int) + 1"),
                ("SELECT col::text", "SELECT CAST(col AS text)"),
                ("SELECT 'x'::varchar", "SELECT CAST('x' AS varchar)"),
                ("SELECT $1::int::text", "SELECT CAST(CAST($1 AS int) AS text)"),
                ("SELECT (a + b)::float8", "SELECT CAST((a + b) AS float8)"),
                ("SELECT 1", "SELECT 1"),
                ("SELECT x::timestamp(3)", "SELECT CAST(x AS timestamp(3))"),
                (
                    "SELECT x::timestamp(3) with time zone, y",
                    "SELECT CAST(x AS timestamp(3) with time zone), y",
                ),
                (
                    "SELECT x::time without time zone",
                    "SELECT CAST(x AS time without time zone)",
                ),
                ("SELECT x::double precision", "SELECT CAST(x AS double precision)"),
                (
                    "SELECT x::character varying(3)",
                    "SELECT CAST(x AS character varying(3))",
                ),
                ("SELECT x::numeric(10, 2) + 1", "SELECT CAST(x AS numeric(10, 2)) + 1"),
                ("SELECT x::public.mood", "SELECT CAST(x AS mood)"),
                ("SELECT x::int (1)", "SELECT CAST(x AS int(1))"),
                ("SELECT x::timestamp, a", "SELECT CAST(x AS timestamp), a"),
                ("SELECT count(x::int) with", "SELECT count(CAST(x AS int)) with"),
                // Quoted names, as Prisma writes every column.
                (r#"SELECT "c"::text"#, r#"SELECT CAST("c" AS text)"#),
                (
                    r#"SELECT "public"."T"."c"::text FROM t"#,
                    r#"SELECT CAST("public"."T"."c" AS text) FROM t"#,
                ),
                (
                    r#"SELECT t."c"::text, "t".c::int"#,
                    r#"SELECT CAST(t."c" AS text), CAST("t".c AS int)"#,
                ),
                (r#"SELECT "we""ird"::text"#, r#"SELECT CAST("we""ird" AS text)"#),
                (
                    r#"SELECT a, "x"."y"::text = 'open'"#,
                    r#"SELECT a, CAST("x"."y" AS text) = 'open'"#,
                ),
            ],
        );
    }

    #[test]
    fn typed_literals() {
        assert_rewrites(
            rewrite_typed_literals,
            &[
                ("SELECT DATE '2024-01-02'", "SELECT CAST('2024-01-02' AS DATE)"),
                (
                    "SELECT timestamp with time zone 'x', 1",
                    "SELECT CAST('x' AS timestamp with time zone), 1",
                ),
                ("SELECT double precision '1'", "SELECT CAST('1' AS double precision)"),
                ("SELECT interval '1 day'", "SELECT CAST('1 day' AS interval)"),
                ("SELECT 'date' 'x'", "SELECT 'date' 'x'"),
                ("SELECT t.date 'x'", "SELECT t.date 'x'"),
                ("SELECT 1 AS date", "SELECT 1 AS date"),
                ("SELECT \"date\" 'x'", "SELECT \"date\" 'x'"),
                ("WHERE d = 'x'", "WHERE d = 'x'"),
            ],
        );
    }

    #[test]
    fn rewrite_casts_reg() {
        assert_rewrites(
            rewrite,
            &[
                (
                    "c.reloftype::pg_catalog.regtype::pg_catalog.text",
                    "CAST((SELECT typname FROM pg_type WHERE oid = c.reloftype) AS text)",
                ),
                ("'1'::pg_catalog.regclass", "1"),
                (
                    "conrelid::pg_catalog.regclass",
                    "(SELECT relname FROM pg_class WHERE oid = conrelid)",
                ),
                ("'emp'::regclass", "(SELECT oid FROM pg_class WHERE relname = 'emp')"),
                (
                    "'public.emp'::regclass",
                    "(SELECT oid FROM pg_class WHERE relname = 'emp')",
                ),
            ],
        );
    }
}
