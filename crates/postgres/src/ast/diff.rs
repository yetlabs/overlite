//! The differential test: every statement of the corpus (tests/fixtures/ast-corpus)
//! through the text passes and through the AST path, the two results compared as
//! `compare` says. Any difference not listed in `compare::INTENDED`, with the reason
//! the AST path is right, fails the test.

use crate::ast::compare::{Verdict, compare};
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
        let (old, new) = (post(&rewrite_full(&sql)), post(&ast));
        match compare(&old, &new) {
            Verdict::Identical => t.identical += 1,
            Verdict::IdenticalButParentheses => t.identical_but_parens += 1,
            Verdict::Intended(why) => *t.intended.entry(why).or_default() += 1,
            Verdict::Unexplained => t.unexplained.push((sql.clone(), old, new)),
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
/// rounds: the text passes alone, the AST path without the translation cache (every
/// statement parsed but those whose shape it already left to the text passes), and
/// `rewrite()` with the cache warm. `cargo test --release -p overlite-postgres --lib
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
            "round {round}: {} statements, us per statement: text passes {a:.1}, AST path without the cache {b:.1}, \
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
