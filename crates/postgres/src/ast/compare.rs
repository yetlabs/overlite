//! The two routes' outputs compared: the text passes' and the AST path's SQLite SQL
//! for one statement, as tokens, so that spacing, the case of keywords, an optional AS
//! and the quotes of a name that needs none do not count. A pair that differs only in
//! parentheses is told apart: the tree keeps none of the source's, and the printer
//! writes those SQLite's precedence needs. Every other difference is either one listed
//! in `INTENDED`, with the reason the AST path is right, or unexplained.
//!
//! The differential test (`diff.rs`) holds the corpus to no unexplained difference, and
//! `shadow` compares a sample of the statements a server translates, logging any.

/// How the AST path's output for a statement stands to the text passes'.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Identical,
    IdenticalButParentheses,
    /// One of `INTENDED`, by its reason.
    Intended(&'static str),
    Unexplained,
}

/// Compares the text passes' output with the AST path's, each after the passes
/// `rewrite` runs on either.
pub(crate) fn compare(text: &str, ast: &str) -> Verdict {
    let old = same_cast_types(tokens(text));
    let new = same_cast_types(tokens(ast));
    if old == new {
        Verdict::Identical
    } else if without_parens(&old) == without_parens(&new) {
        Verdict::IdenticalButParentheses
    } else if let Some((why, _)) = INTENDED.iter().find(|(_, f)| f(&old, &new)) {
        Verdict::Intended(why)
    } else {
        Verdict::Unexplained
    }
}

/// The differences the AST path makes on purpose, each a test over the pair of
/// normalized token lists (text path, AST path) and the reason.
type Intended = (&'static str, fn(&[String], &[String]) -> bool);

const INTENDED: &[Intended] = &[
    (
        "label: an expression whose operand is a cast is ?column? (FigureColname); the text path \
         labeled it by the cast",
        |old, new| {
            let (o, n) = (without_parens(old), without_parens(new));
            o.len() == n.len() && o.iter().zip(&n).all(|(o, n)| o == n || *n == "\"?column?\"")
        },
    ),
    (
        "a quoted name is a name: the text path rewrote an operator inside \"a?b\"",
        |old, new| {
            old.len() == new.len()
                && old
                    .iter()
                    .zip(new)
                    .all(|(o, n)| o == n || (o.starts_with('"') && n.starts_with('"')))
        },
    ),
    (
        "NOT (x IN (subquery)) is written x NOT IN (subquery): one node in Postgres' tree",
        |old, new| {
            let strip = |t: &[String]| -> Vec<String> {
                t.iter()
                    .filter(|t| !matches!(t.as_str(), "(" | ")" | "not"))
                    .cloned()
                    .collect()
            };
            let nots = |t: &[String]| t.iter().filter(|t| *t == "not").count();
            strip(old) == strip(new) && nots(old) == nots(new)
        },
    ),
    (
        "a set member in parentheses without ORDER BY or LIMIT needs no SELECT * FROM wrapper",
        |old, new| {
            let mut o: Vec<&String> = Vec::new();
            let mut i = 0;
            while i < old.len() {
                if old[i..].starts_with(&["select".into(), "*".into(), "from".into(), "(".into()]) {
                    i += 3;
                    continue;
                }
                o.push(&old[i]);
                i += 1;
            }
            o.retain(|t| *t != "(" && *t != ")");
            o == without_parens(new)
        },
    ),
    (
        "a parenthesized column is a column: SQLite names it, the text path wrote the same name as a label",
        |old, new| {
            let o = without_parens(old);
            let n = without_parens(new);
            o.len() > n.len() && (1..o.len()).any(|i| o[i] == o[i - 1] && [&o[..i], &o[i + 1..]].concat() == n)
        },
    ),
];

/// The tokens of a SQLite statement, normalized as the module says.
fn tokens(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() || c == b';' {
            i += 1;
        } else if c == b'\'' {
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push(sql[start..i].to_string());
        } else if c == b'"' {
            let start = i + 1;
            let end = sql[start..].find('"').map_or(b.len(), |p| start + p);
            let name = &sql[start..end];
            let plain = !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c == b'_' || c.is_ascii_lowercase() || c.is_ascii_digit());
            out.push(if plain { name.to_string() } else { format!("\"{name}\"") });
            i = (end + 1).min(b.len());
        } else if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            // A decimal point inside a number stays with it.
            if c.is_ascii_digit() && b.get(i) == Some(&b'.') {
                i += 1;
                while i < b.len() && b[i].is_ascii_alphanumeric() {
                    i += 1;
                }
            }
            let w = sql[start..i].to_ascii_lowercase();
            if !matches!(w.as_str(), "as" | "inner" | "outer") {
                out.push(w);
            }
        } else {
            let two = sql.get(i..i + 2).unwrap_or("");
            if matches!(two, "<>" | "!=" | "<=" | ">=" | "||" | "::" | "->" | "==") {
                out.push(if two == "!=" { "<>".to_string() } else { two.to_string() });
                i += 2;
            } else {
                out.push((c as char).to_string());
                i += 1;
            }
        }
    }
    out
}

/// The type name of each overlite_cast, as the engine reads it: `'UUID'`, `'int'` and
/// `'integer'` are one type to it.
fn same_cast_types(mut t: Vec<String>) -> Vec<String> {
    for i in 0..t.len() {
        if t[i] != "overlite_cast" {
            continue;
        }
        let mut depth = 0;
        for j in i + 1..t.len() {
            match t[j].as_str() {
                "(" => depth += 1,
                ")" => {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(name) = t[j - 1].strip_prefix('\'').and_then(|n| n.strip_suffix('\''))
                            && let Some(ty) = overlite_engine::CastType::parse(name)
                        {
                            t[j - 1] = format!("{ty:?}");
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    t
}

fn without_parens(t: &[String]) -> Vec<&String> {
    t.iter().filter(|t| *t != "(" && *t != ")").collect()
}
