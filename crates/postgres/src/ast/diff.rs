//! The differential test: every statement of the corpus (tests/fixtures/ast-corpus)
//! through the text passes and through the AST path, the two results compared.
//!
//! The comparison reads tokens, so spacing, the case of keywords, an optional AS and
//! the quotes of a name that needs none do not count. A pair that differs only in
//! parentheses is counted apart: the tree keeps none of the source's, and the printer
//! writes those SQLite's precedence needs. Every other difference must be one listed
//! in `INTENDED`, with the reason the AST path is right; anything else fails the test.

use crate::ast::try_translate;
use crate::rewrite::blank::statement_text;
use crate::rewrite::{needs_no_rewrite, rewrite_full};
use std::collections::BTreeMap;

const CORPUS: &[(&str, &str)] = &[
    ("wire", include_str!("../../../../tests/fixtures/ast-corpus/wire.hex")),
    (
        "rewrite",
        include_str!("../../../../tests/fixtures/ast-corpus/rewrite.hex"),
    ),
    (
        "django",
        include_str!("../../../../tests/fixtures/ast-corpus/django.hex"),
    ),
    (
        "wagtail",
        include_str!("../../../../tests/fixtures/ast-corpus/wagtail.hex"),
    ),
    ("gitea", include_str!("../../../../tests/fixtures/ast-corpus/gitea.hex")),
    ("owner", include_str!("../../../../tests/fixtures/ast-corpus/owner.hex")),
];

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

/// Every statement of the corpus, as the server's front door leaves it.
pub(crate) fn corpus() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (src, hex_lines) in CORPUS {
        for line in hex_lines.lines() {
            let Ok(bytes) = hex::decode(line.trim()) else { continue };
            let Ok(raw) = String::from_utf8(bytes) else { continue };
            if let Ok(sql) = statement_text(&raw)
                && !sql.trim().is_empty()
            {
                out.push((*src, sql.into_owned()));
            }
        }
    }
    out
}

/// The tokens of a SQLite statement, normalized as the module says.
pub(crate) fn tokens(sql: &str) -> Vec<String> {
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

#[derive(Default, Debug)]
pub(crate) struct Tally {
    pub fast_path: usize,
    pub unsupported: BTreeMap<&'static str, usize>,
    pub identical: usize,
    pub identical_but_parens: usize,
    pub intended: BTreeMap<&'static str, usize>,
    pub unexplained: Vec<(String, String, String)>,
    pub per_source: BTreeMap<&'static str, [usize; 3]>,
}

/// Runs both paths over the corpus.
pub(crate) fn run() -> Tally {
    let mut t = Tally::default();
    for (src, sql) in corpus() {
        let slot = t.per_source.entry(src).or_default();
        if needs_no_rewrite(&sql) {
            t.fast_path += 1;
            slot[0] += 1;
            continue;
        }
        let ast = match try_translate(&sql) {
            Ok(a) => a,
            Err(why) => {
                *t.unsupported.entry(why).or_default() += 1;
                slot[1] += 1;
                continue;
            }
        };
        slot[2] += 1;
        // What rewrite runs after either route.
        let post = |s: &str| crate::rewrite::like::rewrite_like(&crate::rewrite::null_order::rewrite_null_order(s));
        let old = same_cast_types(tokens(&post(&rewrite_full(&sql))));
        let new = same_cast_types(tokens(&post(&ast)));
        if old == new {
            t.identical += 1;
        } else if without_parens(&old) == without_parens(&new) {
            t.identical_but_parens += 1;
        } else if let Some((why, _)) = INTENDED.iter().find(|(_, f)| f(&old, &new)) {
            *t.intended.entry(why).or_default() += 1;
        } else {
            t.unexplained.push((sql.clone(), old.join(" "), new.join(" ")));
        }
    }
    t
}

#[test]
fn ast_path_matches_the_text_passes_over_the_corpus() {
    let t = run();
    let covered = t.identical + t.identical_but_parens + t.intended.values().sum::<usize>() + t.unexplained.len();
    eprintln!(
        "fast path {}, unsupported {}, translated {covered}: identical {}, identical but parentheses {}, \
         intended {:?}, unexplained {}",
        t.fast_path,
        t.unsupported.values().sum::<usize>(),
        t.identical,
        t.identical_but_parens,
        t.intended,
        t.unexplained.len()
    );
    eprintln!("per source [fast path, unsupported, translated]: {:?}", t.per_source);
    let mut why: Vec<_> = t.unsupported.iter().collect();
    why.sort_by(|a, b| b.1.cmp(a.1));
    eprintln!("unsupported by first node: {why:?}");
    for (sql, old, new) in t.unexplained.iter().take(40) {
        eprintln!("SQL  {sql}\nTEXT {old}\nAST  {new}\n");
    }
    assert!(
        t.unexplained.is_empty(),
        "{} unexplained differences",
        t.unexplained.len()
    );
}

#[test]
fn printer_writes_what_postgres_read() {
    let t = |sql: &str| crate::ast::try_translate(sql);
    // Postgres binds || looser than +, SQLite tighter.
    assert_eq!(
        t("SELECT a + b || c FROM t").as_deref(),
        Ok("SELECT (a + b) || c AS \"?column?\" FROM t")
    );
    // A literal stays a literal, whatever it holds; a comment is gone.
    assert_eq!(
        t("SELECT 'x::int -- /*' AS v /* ::date */ FROM t").as_deref(),
        Ok("SELECT 'x::int -- /*' AS \"v\" FROM t")
    );
    // NULLs sort as Postgres sorts them; OFFSET alone gets SQLite's LIMIT.
    assert_eq!(
        t("SELECT a FROM t ORDER BY a DESC, b OFFSET $1").as_deref(),
        Ok("SELECT a FROM t ORDER BY a DESC NULLS FIRST, b NULLS LAST LIMIT -1 OFFSET $1")
    );
    // Labels as FigureColname gives them.
    assert_eq!(
        t("SELECT count(*), $1::int + 1, x::date, CASE WHEN a THEN 1 END FROM t").as_deref(),
        Ok(
            "SELECT count(*) AS \"count\", overlite_cast($1, 'integer') + 1 AS \"?column?\", \
            overlite_cast(x, 'date') AS \"x\", CASE WHEN a THEN 1 END AS \"case\" FROM t"
        )
    );
    // What only types tell apart, and groupings SQLite cannot write, go to the text passes.
    assert!(t("SELECT name FROM t WHERE name % 'x'").is_err());
    assert!(t("SELECT 1 UNION SELECT 2 INTERSECT SELECT 3").is_err());
}

#[test]
fn a_parameter_meets_the_column_of_its_own_scope() {
    let (tables, refs) =
        crate::ast::params::param_columns("SELECT * FROM a WHERE id = $1 AND EXISTS (SELECT 1 FROM b WHERE id = $2)")
            .unwrap_or_default();
    assert_eq!(tables.len(), 2);
    let key = |p: usize| refs.iter().find(|r| r.param == p).map(|r| r.qualifier.clone());
    assert_eq!(key(1), Some(vec!["#0".to_string()]), "$1 meets a.id");
    assert_eq!(key(2), Some(vec!["#1".to_string()]), "$2 meets b.id, not a.id");
}

/// The text reader's and the tree's parameter columns over the corpus, by
/// (parameter, column, how it meets it). Informational: printed, not asserted.
#[test]
fn parameter_columns_over_the_corpus() {
    use std::collections::BTreeSet;
    let (mut with_params, mut same, mut more, mut fewer, mut other) = (0, 0, 0, 0, 0);
    let (mut text_scoped, mut ast_scoped) = (0usize, 0usize);
    for (_, sql) in corpus() {
        if !sql.contains('$') {
            continue;
        }
        let Some((_, ast)) = crate::ast::params::param_columns(&sql) else {
            continue;
        };
        let (_, text) = overlite_engine::param_columns(&sql);
        if ast.is_empty() && text.is_empty() {
            continue;
        }
        with_params += 1;
        let key =
            |r: &overlite_engine::ParamColumn| (r.param, r.column.clone(), r.meets == overlite_engine::Meets::Array);
        let a: BTreeSet<_> = ast.iter().map(key).collect();
        let t: BTreeSet<_> = text.iter().map(key).collect();
        text_scoped += text.iter().filter(|r| !r.qualifier.is_empty()).count();
        ast_scoped += ast
            .iter()
            .filter(|r| r.qualifier.first().is_some_and(|q| q.starts_with('#')))
            .count();
        match (a == t, a.is_superset(&t), a.is_subset(&t)) {
            (true, ..) => same += 1,
            (_, true, _) => more += 1,
            (_, _, true) => {
                fewer += 1;
                eprintln!("FEWER {sql}\n  text {t:?}\n  tree {a:?}");
            }
            _ => other += 1,
        }
    }
    eprintln!(
        "statements with parameter columns {with_params}: same {same}, tree finds more {more}, fewer {fewer}, \
         other {other}; references tied to a table: text {text_scoped}, tree {ast_scoped}"
    );
}

/// The statement mix of the corpus through `rewrite`'s two routes, in alternating
/// rounds: the text passes alone, the AST path without the cache (every statement
/// parsed), and the AST path with it. `cargo test --release -p overlite-postgres --lib
/// time_the_statement_mix -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, not a check"]
fn time_the_statement_mix() {
    use crate::rewrite::like::rewrite_like;
    use crate::rewrite::null_order::rewrite_null_order;
    use std::hint::black_box;
    use std::time::Instant;
    // OVERLITE_MIX_LEN takes the first n statements: a mix that fits the cache, as a
    // client's working set does, against the whole corpus, which does not.
    let take = std::env::var("OVERLITE_MIX_LEN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    // OVERLITE_MIX_DML=1 keeps the statements the cache keeps (that read or write
    // rows); DDL is translated every time.
    let dml = std::env::var_os("OVERLITE_MIX_DML").is_some();
    let stmts: Vec<String> = corpus()
        .into_iter()
        .map(|(_, s)| s)
        .filter(|s| !dml || crate::rewrite::cache::cacheable(s))
        .take(take)
        .collect();
    let text = |s: &str| {
        if needs_no_rewrite(s) {
            return rewrite_like(&rewrite_null_order(s));
        }
        rewrite_like(&rewrite_null_order(&rewrite_full(s)))
    };
    let ast = |s: &str| {
        if needs_no_rewrite(s) {
            return rewrite_like(&rewrite_null_order(s));
        }
        let full = crate::ast::translate(s).unwrap_or_else(|| rewrite_full(s));
        rewrite_like(&rewrite_null_order(&full))
    };
    // Warm the cache, as a client's third round would find it: a statement is kept
    // the second time it is translated.
    for _ in 0..2 {
        for s in &stmts {
            black_box(crate::rewrite::rewrite(s));
        }
    }
    let time = |f: &dyn Fn(&str) -> String| {
        let t0 = Instant::now();
        for s in &stmts {
            black_box(f(black_box(s)));
        }
        t0.elapsed().as_secs_f64() * 1e6 / stmts.len() as f64
    };
    for round in 1..=3 {
        let a = time(&text);
        let b = time(&ast);
        let c = time(&|s| crate::rewrite::rewrite(s));
        eprintln!(
            "round {round}: {} statements, us per statement: text passes {a:.1}, AST path uncached {b:.1}, \
             rewrite() with cache warm {c:.2}",
            stmts.len()
        );
    }
    eprintln!(
        "cache (entries, bytes, hits, misses): {:?}; fast path {}",
        crate::rewrite::cache::CACHE.usage(),
        stmts.iter().filter(|s| needs_no_rewrite(s)).count()
    );
}
