//! OPERATOR(...), COLLATE, the POSIX regex operators and pg_trgm's.

use crate::rewrite::casts::{is_operand_char, operand_before};
use crate::rewrite::splice;
use crate::rewrite::strings::{end_of_string_literal, map_outside_strings};
use crate::rewrite::tokens::{CodeTok, CodeTokKind, code_tokens};
use overlite_core::lazy_regex;
use overlite_core::text::{replace_all_func, trim_chars};

// Matches Postgres' explicit operator syntax, e.g.
// "OPERATOR(pg_catalog.~)", which psql emits for pattern matching.
lazy_regex!(RE_OPERATOR_CALL, r"(?i)OPERATOR\s*\(\s*([^)]+?)\s*\)");

/// Unwraps OPERATOR(...) back to the bare operator so the rest of the dialect
/// layer (and SQLite) can handle it.
pub(crate) fn rewrite_operator_syntax(sql: &str) -> String {
    map_outside_strings(sql, |code| {
        replace_all_func(&RE_OPERATOR_CALL, code, |m| {
            let Some(sm) = RE_OPERATOR_CALL.captures(m) else {
                return m.to_string();
            };
            let mut inner = sm.get(1).map_or("", |x| x.as_str());
            if let Some(i) = inner.rfind('.') {
                // drop any schema qualifier
                inner = &inner[i + 1..];
            }
            [" ", inner, " "].concat()
        })
    })
}

// Matches a COLLATE clause with an optionally schema-qualified,
// optionally double-quoted collation name.
lazy_regex!(RE_COLLATE, r#"(?i)\s+COLLATE\s+("[^"]*"|\w+)(?:\.("[^"]*"|\w+))?"#);

/// Maps Postgres COLLATE onto SQLite. A language collation (`sv-x-icu`,
/// `sv_SE.utf8`) or one CREATE COLLATION made stays, under its name, and the engine
/// provides it; any other case-insensitive name becomes NOCASE; C, POSIX, default and
/// unknown names fall back to the byte-order default by dropping the clause.
pub(crate) fn rewrite_collate(sql: &str) -> String {
    map_outside_strings(sql, |code| {
        replace_all_func(&RE_COLLATE, code, |m| {
            let Some(sub) = RE_COLLATE.captures(m) else {
                return m.to_string();
            };
            let mut name = sub.get(1).map_or("", |x| x.as_str());
            if let Some(s2) = sub.get(2)
                && !s2.as_str().is_empty()
            {
                name = s2.as_str(); // schema-qualified: the last part is the collation
            }
            let name = trim_chars(name, "\"");
            match name.to_lowercase().as_str() {
                "decimal" | "binary" | "nocase" | "rtrim" => return m.to_string(), // keep overlite/SQLite native collations verbatim
                _ => {}
            }
            // A language collation, or one CREATE COLLATION made: the engine provides
            // it under its own name, which the catalogs then report.
            if overlite_engine::collation_known(name) {
                return [" COLLATE \"", &name.replace('"', "\"\""), "\""].concat();
            }
            if collation_is_ci(name) {
                return " COLLATE NOCASE".to_string();
            }
            String::new()
        })
    })
}

/// Reports whether a Postgres collation name denotes a case-insensitive
/// collation (by convention or ICU comparison level).
pub(crate) fn collation_is_ci(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("nocase")
        || n.contains("insensitive")
        || n.contains("ks-level1")
        || n.contains("ks-level2")
        || n.ends_with("_ci")
        || n.ends_with("-ci")
}

/// Maps Postgres POSIX-regex operators onto SQLite's REGEXP function: ~ (match),
/// !~ (no match), and the *-suffixed case-insensitive variants.
pub(crate) fn rewrite_match_operators(sql: &str) -> String {
    let mut sql = sql.to_string();
    loop {
        let Some((op, idx)) = find_match_op(&sql) else {
            return sql;
        };
        let (ls, le) = operand_before(&sql, idx);
        let (rs, re) = operand_after(&sql, idx + op.len());
        if ls == le || rs == re {
            return sql; // malformed; avoid looping
        }
        let left = &sql[ls..le];
        let mut right = sql[rs..re].to_string();
        if op.ends_with('*') {
            // case-insensitive
            right = inject_case_insensitive(&right);
        }
        let mut repl = [left, " REGEXP ", &right].concat();
        if op.starts_with('!') {
            repl = ["NOT (", left, " REGEXP ", &right, ")"].concat();
        }
        sql = splice(&sql, ls, re, &repl);
    }
}

/// Locates the first ~, !~, ~*, or !~* operator not inside a string literal,
/// returning the operator text and the index of its first byte.
fn find_match_op(sql: &str) -> Option<(&'static str, usize)> {
    let b = sql.as_bytes();
    let mut in_str = false;
    for i in 0..b.len() {
        match b[i] {
            b'\'' => in_str = !in_str,
            b'~' => {
                if in_str {
                    continue;
                }
                let bang = i > 0 && b[i - 1] == b'!';
                let start = if bang { i - 1 } else { i };
                let star = i + 1 < b.len() && b[i + 1] == b'*';
                let op = match (bang, star) {
                    (false, false) => "~",
                    (false, true) => "~*",
                    (true, false) => "!~",
                    (true, true) => "!~*",
                };
                return Some((op, start));
            }
            _ => {}
        }
    }
    None
}

/// Returns the [start,end) span of the operand starting at or after pos
/// (skipping spaces): a string literal, a parenthesized group, or a token.
pub(crate) fn operand_after(sql: &str, pos: usize) -> (usize, usize) {
    let b = sql.as_bytes();
    let mut pos = pos;
    while pos < b.len() && b[pos] == b' ' {
        pos += 1;
    }
    if pos >= b.len() {
        return (pos, pos);
    }
    match b[pos] {
        b'\'' => (pos, end_of_string_literal(sql, pos)),
        b'(' => {
            let mut depth = 0;
            let mut j = pos;
            while j < b.len() {
                if b[j] == b'(' {
                    depth += 1;
                } else if b[j] == b')' {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                j += 1;
            }
            (pos, j)
        }
        _ => {
            let mut j = pos;
            while j < b.len() && is_operand_char(b[j]) {
                j += 1;
            }
            (pos, j)
        }
    }
}

/// Prepends the (?i) flag to a ~* pattern.
fn inject_case_insensitive(operand: &str) -> String {
    let b = operand.as_bytes();
    if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        return ["'(?i)", &operand[1..]].concat();
    }
    // A pattern bound as a parameter or built by an expression is prefixed when
    // it is evaluated; left as it was, ~* $1 matched case-sensitively.
    ["('(?i)' || ", operand, ")"].concat()
}

/// The token span of the operand that ends at token `end` (inclusive): a name, a
/// qualified name, a literal, a parenthesized group, or a call.
pub(crate) fn operand_back(sql: &str, toks: &[CodeTok], end: usize) -> usize {
    // A cast's type: the operand is what it casts.
    let mut end = end;
    if toks[end].is_punct(sql, "]") && end >= 1 && toks[end - 1].is_punct(sql, "[") {
        end -= 2;
    }
    if toks[end].is_punct(sql, ")")
        && let Some(open) = (0..end)
            .rev()
            .find(|&k| toks[k].depth == toks[end].depth && toks[k].is_punct(sql, "("))
        && open >= 3
        && toks[open - 1].kind == CodeTokKind::Word
        && toks[open - 2].is_punct(sql, ":")
        && toks[open - 3].is_punct(sql, ":")
    {
        end = open - 1;
    }
    if end >= 3
        && toks[end].kind == CodeTokKind::Word
        && toks[end - 1].is_punct(sql, ":")
        && toks[end - 2].is_punct(sql, ":")
    {
        return operand_back(sql, toks, end - 3);
    }
    let t = &toks[end];
    let mut start = end;
    if t.is_punct(sql, ")") {
        let Some(open) = (0..end)
            .rev()
            .find(|&k| toks[k].depth == t.depth && toks[k].is_punct(sql, "("))
        else {
            return end;
        };
        start = open;
        if open > 0 && toks[open - 1].kind == CodeTokKind::Word {
            start = open - 1;
        }
    }
    while start >= 2
        && toks[start - 1].is_punct(sql, ".")
        && matches!(toks[start - 2].kind, CodeTokKind::Word | CodeTokKind::Value)
    {
        start -= 2;
    }
    start
}

/// The token span of the operand that starts at token `start`, its end inclusive:
/// as operand_back, and a `::type` after it.
pub(crate) fn operand_fwd(sql: &str, toks: &[CodeTok], start: usize) -> usize {
    let close = |open: usize| {
        (open + 1..toks.len())
            .find(|&k| toks[k].depth == toks[open].depth && toks[k].is_punct(sql, ")"))
            .unwrap_or(toks.len() - 1)
    };
    // A sign before it.
    if (toks[start].is_punct(sql, "-") || toks[start].is_punct(sql, "+")) && start + 1 < toks.len() {
        return operand_fwd(sql, toks, start + 1);
    }
    let mut end = start;
    if toks[start].is_punct(sql, "(") {
        end = close(start);
    } else {
        while end + 2 < toks.len()
            && toks[end + 1].is_punct(sql, ".")
            && matches!(toks[end + 2].kind, CodeTokKind::Word | CodeTokKind::Value)
        {
            end += 2;
        }
        if toks[end].kind == CodeTokKind::Word && toks.get(end + 1).is_some_and(|t| t.is_punct(sql, "(")) {
            end = close(end + 1);
        }
    }
    // A cast after it: ::name, ::name(n), ::name[].
    while end + 3 < toks.len()
        && toks[end + 1].is_punct(sql, ":")
        && toks[end + 2].is_punct(sql, ":")
        && toks[end + 2].span.start == toks[end + 1].span.end
        && toks[end + 3].kind == CodeTokKind::Word
    {
        end += 3;
        if toks.get(end + 1).is_some_and(|t| t.is_punct(sql, "(")) {
            end = close(end + 1);
        }
        while toks.get(end + 1).is_some_and(|t| t.is_punct(sql, "["))
            && toks.get(end + 2).is_some_and(|t| t.is_punct(sql, "]"))
        {
            end += 2;
        }
    }
    end
}

/// Postgres' math operators SQLite has not got, as calls: `a # b` (bitwise xor), `a ^
/// b` (power), and the prefix `|/ a` (square root), `||/ a` (cube root) and `@ a`
/// (absolute value). `^` binds tighter than `*`, so its operands are whole terms; a
/// chain goes left to right.
pub(crate) fn rewrite_math_operators(sql: &str) -> String {
    if !sql.contains('#') && !sql.contains('^') && !sql.contains("|/") && !sql.contains('@') {
        return sql.to_string();
    }
    let mut sql = sql.to_string();
    for _ in 0..64 {
        let toks = code_tokens(&sql);
        let mut found: Option<(usize, usize, String)> = None;
        for i in 0..toks.len() {
            let Some((op, n)) = op_at(&sql, &toks, i, &["||/", "|/", "#", "^", "@"]) else {
                continue;
            };
            if i + n >= toks.len() {
                continue;
            }
            let prefix = i == 0 || {
                let p = &toks[i - 1];
                (p.kind == CodeTokKind::Punct && !p.is_punct(&sql, ")") && !p.is_punct(&sql, "]"))
                    || (p.kind == CodeTokKind::Word && PREFIX_AFTER.iter().any(|w| p.is_word(&sql, w)))
            };
            let re = operand_fwd(&sql, &toks, i + n);
            let right = &sql[toks[i + n].span.start..toks[re].span.end];
            match (op, prefix) {
                ("|/" | "||/" | "@", true) => {
                    let f = match op {
                        "|/" => "sqrt",
                        "||/" => "cbrt",
                        _ => "abs",
                    };
                    found = Some((toks[i].span.start, toks[re].span.end, [f, "(", right, ")"].concat()));
                }
                ("#" | "^", false) => {
                    let ls = operand_back(&sql, &toks, i - 1);
                    let left = &sql[toks[ls].span.start..toks[i - 1].span.end];
                    let f = if op == "#" { "overlite_xor" } else { "power" };
                    found = Some((
                        toks[ls].span.start,
                        toks[re].span.end,
                        [f, "(", left, ", ", right, ")"].concat(),
                    ));
                }
                _ => continue,
            }
            break;
        }
        let Some((a, b, call)) = found else { break };
        sql = splice(&sql, a, b, &call);
    }
    sql
}

/// The words a prefix operator can follow.
const PREFIX_AFTER: [&str; 16] = [
    "select",
    "where",
    "and",
    "or",
    "not",
    "when",
    "then",
    "else",
    "by",
    "on",
    "having",
    "set",
    "values",
    "returning",
    "distinct",
    "return",
];

/// pg_trgm's operators, longest first.
const TRGM_OPS: [&str; 10] = ["<<<->", "<->>>", "<<->", "<->>", "<<%", "%>>", "<->", "<%", "%>", "%"];

/// The punctuation run at token i that spells an operator of ops, with its length in
/// tokens: the characters must stand side by side.
fn op_at(sql: &str, toks: &[CodeTok], i: usize, ops: &[&'static str]) -> Option<(&'static str, usize)> {
    for op in ops {
        let n = op.len();
        if i + n > toks.len() {
            continue;
        }
        let run = &toks[i..i + n];
        let adjacent = run.windows(2).all(|w| w[0].span.end == w[1].span.start);
        if adjacent
            && run.iter().all(|t| t.kind == CodeTokKind::Punct)
            && sql[run[0].span.start..run[n - 1].span.end] == **op
        {
            // Not part of a longer run of operator characters.
            let before = i > 0
                && toks[i - 1].kind == CodeTokKind::Punct
                && toks[i - 1].span.end == run[0].span.start
                && "<>%-!@#^&|~=".contains(&sql[toks[i - 1].span.clone()]);
            let after = toks.get(i + n).is_some_and(|t| {
                t.kind == CodeTokKind::Punct
                    && t.span.start == run[n - 1].span.end
                    && "<>%-!@#^&|~=".contains(&sql[t.span.clone()])
            });
            if !before && !after {
                return Some((op, n));
            }
        }
    }
    None
}

/// pg_trgm's operators as calls: `a % b` (when a side is a string, as `%` between
/// numbers is the remainder), `<%`, `%>`, `<<%` and `%>>` as overlite_trgm_match, and
/// the distances `<->`, `<<->`, `<->>`, `<<<->` and `<->>>` as overlite_trgm_dist.
pub(crate) fn rewrite_trgm_operators(sql: &str) -> String {
    if !sql.contains('%') && !sql.contains("<->") {
        return sql.to_string();
    }
    let mut sql = sql.to_string();
    for _ in 0..64 {
        let toks = code_tokens(&sql);
        let mut found = None;
        for i in 1..toks.len() {
            let Some((op, n)) = op_at(&sql, &toks, i, &TRGM_OPS) else {
                continue;
            };
            if i + n >= toks.len() {
                continue;
            }
            let ls = operand_back(&sql, &toks, i - 1);
            let re = operand_fwd(&sql, &toks, i + n);
            if op == "%" {
                let literal =
                    |k: usize| toks[k].kind == CodeTokKind::Value && sql[toks[k].span.clone()].starts_with('\'');
                if !literal(ls) && !literal(i + n) {
                    continue;
                }
            }
            found = Some((
                op,
                toks[ls].span.start,
                toks[i - 1].span.end,
                toks[i + n].span.start,
                toks[re].span.end,
                toks[i].span.start,
            ));
            break;
        }
        let Some((op, ls, le, rs, re, _)) = found else { break };
        let f = if op.contains("->") {
            "overlite_trgm_dist"
        } else {
            "overlite_trgm_match"
        };
        let call = [f, "(", &sql[ls..le], ", ", &sql[rs..re], ", '", op, "')"].concat();
        sql = splice(&sql, ls, re, &call);
    }
    sql
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn math_operators() {
        let cases = [
            (
                "SELECT 5 # 3, 2 ^ 3 ^ 2",
                "SELECT overlite_xor(5, 3), power(power(2, 3), 2)",
            ),
            (
                r#"SELECT ("t"."a" # 3) FROM t"#,
                r#"SELECT (overlite_xor("t"."a", 3)) FROM t"#,
            ),
            (
                "SELECT |/ 25, ||/ 27, @ -5, x::int ^ 2",
                "SELECT sqrt(25), cbrt(27), abs(-5), power(x::int, 2)",
            ),
            (
                "SELECT j #> '{a}', j @> '{}', a @@ b, j #- '{a}'",
                "SELECT j #> '{a}', j @> '{}', a @@ b, j #- '{a}'",
            ),
        ];
        for (sql, want) in cases {
            assert_eq!(rewrite_math_operators(sql), want, "{sql}");
        }
    }

    #[test]
    fn trgm_operators() {
        let cases = [
            (
                r#"SELECT * FROM t WHERE "t"."name" % 'x'"#,
                r#"SELECT * FROM t WHERE overlite_trgm_match("t"."name", 'x', '%')"#,
            ),
            ("SELECT a % 3 FROM t", "SELECT a % 3 FROM t"),
            (
                "SELECT name <-> 'x'::text AS d FROM t ORDER BY d",
                "SELECT overlite_trgm_dist(name, 'x'::text, '<->') AS d FROM t ORDER BY d",
            ),
            (
                "WHERE 'x' <% lower(name) AND b %>> 'y'",
                "WHERE overlite_trgm_match('x', lower(name), '<%') AND overlite_trgm_match(b, 'y', '%>>')",
            ),
        ];
        for (sql, want) in cases {
            assert_eq!(rewrite_trgm_operators(sql), want, "{sql}");
        }
    }

    use super::*;
    use crate::rewrite::{assert_rewrites, rewrite};

    #[test]
    fn rewrite_match_operators_cases() {
        assert_rewrites(
            rewrite_match_operators,
            &[
                ("a ~ 'x'", "a REGEXP 'x'"),
                ("a !~ 'x'", "NOT (a REGEXP 'x')"),
                ("nspname !~ '^pg_toast'", "NOT (nspname REGEXP '^pg_toast')"),
                ("n.nspname ~ 'pub'", "n.nspname REGEXP 'pub'"),
                ("a ~* 'x'", "a REGEXP '(?i)x'"),
                ("a !~* 'x'", "NOT (a REGEXP '(?i)x')"),
                ("WHERE a = 1", "WHERE a = 1"),
                ("a !~ 'p' AND b ~ 'q'", "NOT (a REGEXP 'p') AND b REGEXP 'q'"),
            ],
        );
    }

    #[test]
    fn rewrite_operator_syntax_and_collate() {
        assert_rewrites(
            rewrite,
            &[
                (
                    "c.relname OPERATOR(pg_catalog.~) '^(clientes)$' COLLATE pg_catalog.default",
                    "c.relname REGEXP '^(clientes)$'",
                ),
                ("a OPERATOR(pg_catalog.~) 'x'", "a REGEXP 'x'"),
            ],
        );
        assert_eq!(
            rewrite_collate(r#"SELECT a COLLATE "default" FROM t"#),
            "SELECT a FROM t"
        );
    }
}
