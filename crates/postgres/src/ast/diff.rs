//! The differential test: every statement of the corpus (tests/fixtures/ast-corpus)
//! through the text passes and through the AST path, the two results compared as
//! `compare` says. Any difference not listed in `compare::INTENDED`, with the reason
//! the AST path is right, fails the test.

use crate::ast::compare::{Verdict, compare};
use crate::ast::try_translate;
use crate::rewrite::blank::statement_text;
use crate::rewrite::{needs_no_rewrite, rewrite_full, text_tail};
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

/// A context with no tables: the corpus' statements are of applications whose schemas
/// are not here, so both routes type them with an empty catalog, the AST path from its
/// environment and the text passes' output through the engine's type passes.
pub(crate) fn empty_context() -> Option<crate::ast::env::Context> {
    thread_local! {
        static EMPTY: std::sync::Arc<overlite_engine::TableTypes> =
            std::sync::Arc::new(overlite_engine::TableTypes::of(&[]));
    }
    Some(crate::ast::env::Context::new(EMPTY.with(Clone::clone), None))
}

/// The AST path's typed translation without the engine's mark.
pub(crate) fn typed(sql: &str) -> Result<String, &'static str> {
    let out = crate::ast::env::with(empty_context(), || try_translate(sql))?;
    Ok(crate::ast::split_mark(&out).0.to_string())
}

/// The text route as `rewrite` runs it, then the engine's type passes, which finish
/// what the text passes translated.
pub(crate) fn text_route(sql: &str) -> String {
    overlite_engine::type_passes_without_tables(&text_tail(&rewrite_full(sql)))
}

/// The query a statement wraps that the session translates alone: an EXPLAIN's, a
/// DECLARE's, a `COPY (query) TO STDOUT`'s.
pub(crate) fn wrapped_query(sql: &str) -> Option<String> {
    if let Some(q) = crate::session::explain::explained(sql) {
        return Some(q.to_string());
    }
    if crate::session::cursor::is_cursor_statement(sql) {
        return crate::session::cursor::declared_query(sql);
    }
    crate::session::copy::parse_copy(sql).and_then(|c| c.query().map(str::to_string))
}

/// Whether sql is one statement the DDL printer takes (`ddl::takes`).
pub(crate) fn is_ddl(sql: &str) -> bool {
    crate::ast::parse::parse(sql).is_ok_and(|p| {
        matches!(p.stmts.as_slice(), [raw] if raw.stmt.as_deref()
            .and_then(|s| s.node.as_ref())
            .is_some_and(crate::ast::ddl::takes))
    })
}

/// Runs both paths over the corpus, a statement that wraps a query (EXPLAIN, DECLARE,
/// COPY) by that query, which is what the session translates.
pub(crate) fn run() -> Tally {
    let mut t = Tally::default();
    for (src, sql) in corpus() {
        let sql = wrapped_query(&sql).unwrap_or(sql);
        let slot = t.per_source.entry(src).or_default();
        if needs_no_rewrite(&sql) {
            t.fast_path += 1;
            slot[0] += 1;
            continue;
        }
        let ast = match typed(&sql) {
            Ok(a) => a,
            Err(why) => {
                *t.unsupported.entry(why).or_default() += 1;
                slot[1] += 1;
                continue;
            }
        };
        slot[2] += 1;
        // The text route as rewrite and the engine run it; the AST path's output is final.
        // A DDL statement's is the engine's type passes' on either route, and what
        // SQLite keeps of a table's: both are compared before them.
        let old = if is_ddl(&sql) {
            text_tail(&rewrite_full(&sql))
        } else {
            text_route(&sql)
        };
        let new = ast;
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
    let shown = std::env::var("OVERLITE_DIFF_SHOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    for (sql, old, new) in t.unexplained.iter().take(shown) {
        let one = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        eprintln!("SQL  {}\nTEXT {}\nAST  {}\n", one(sql), one(old), one(new));
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
    // LIKE and ILIKE as the engine matches them, with their escape; the pattern is the
    // whole expression, a CASE included.
    assert_eq!(
        t("SELECT a FROM t WHERE a LIKE CASE WHEN b THEN 'x%' END AND c NOT ILIKE $1 ESCAPE '!'").as_deref(),
        Ok(
            "SELECT a FROM t WHERE a GLOB overlite_like_glob(CASE WHEN b THEN 'x%' END, '\\') \
            AND c NOT LIKE overlite_ilike_pattern($1, '!') ESCAPE '\\'"
        )
    );
    // Casts to the types the engine reads, jsonb with its collation, hstore by hstore_in.
    assert_eq!(
        t("SELECT x::timestamptz(3), '{}'::JSONB, 'a=>1'::hstore, y::hstore FROM t").as_deref(),
        Ok(
            "SELECT overlite_cast(x, 'timestamptz(3)') AS \"x\", (overlite_cast('{}', 'jsonb') COLLATE JSONB) \
            AS \"jsonb\", hstore_in('a=>1') AS \"hstore\", y AS \"y\" FROM t"
        )
    );
    // What only types tell apart, and groupings SQLite cannot write, go to the text passes.
    assert!(t("SELECT name FROM t WHERE name % 'x'").is_err());
    // A set operation on the right of one is read from a subquery: Postgres binds
    // INTERSECT tighter, SQLite reads a compound left to right.
    assert_eq!(
        t("SELECT 1 UNION SELECT 2 INTERSECT SELECT 3").as_deref(),
        Ok("SELECT 1 AS \"?column?\" UNION SELECT * FROM (SELECT 2 INTERSECT SELECT 3)")
    );
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

/// Parameter typing's differential: for every statement of the corpus the AST path
/// translates, the INSERT slots and row counts the tree finds against those the text
/// readers (`param_types`' regexes) find. The tree must find every one the text finds,
/// in the same slot; where it finds more, the text missed it.
#[test]
fn parameter_slots_match_the_text_readers_over_the_corpus() {
    use crate::ast::params::Slot;
    use std::collections::BTreeSet;
    let (mut with_params, mut same, mut more) = (0, 0, 0);
    let mut fewer = Vec::new();
    for (_, sql) in corpus() {
        if !sql.contains('$') || needs_no_rewrite(&sql) {
            continue;
        }
        let Some(facts) = crate::ast::params::facts(&sql) else {
            continue;
        };
        let (text_table, text_slots, text_counts) = crate::types::param_types::text_slots(&sql);
        let mut tree_slots = facts.insert.as_ref().map(|(_, s)| s.clone()).unwrap_or_default();
        tree_slots.sort_by_key(|(p, _)| *p);
        let mut tree_counts = facts.row_counts.clone();
        tree_counts.sort_unstable();
        tree_counts.dedup();
        if text_slots.is_empty() && tree_slots.is_empty() && text_counts.is_empty() && tree_counts.is_empty() {
            continue;
        }
        with_params += 1;
        let key = |s: &Slot| match s {
            Slot::Column(c) => c.trim_matches('"').to_lowercase(),
            Slot::Position(k) => format!("#{k}"),
        };
        let t: BTreeSet<_> = text_slots.iter().map(|(p, s)| (*p, key(s))).collect();
        let a: BTreeSet<_> = tree_slots.iter().map(|(p, s)| (*p, key(s))).collect();
        let tc: BTreeSet<_> = text_counts.iter().collect();
        let ac: BTreeSet<_> = tree_counts.iter().collect();
        let name = |t: &str| t.replace('"', "").to_lowercase();
        let tree_table = facts.insert.as_ref().map(|(t, _)| name(t));
        let table_ok = text_table.is_empty() || tree_table.as_deref() == Some(&name(&text_table));
        if a == t && ac == tc && table_ok {
            same += 1;
        } else if a.is_superset(&t) && ac.is_superset(&tc) && table_ok {
            more += 1;
            eprintln!("MORE {sql}\n  text {t:?} {tc:?}\n  tree {a:?} {ac:?}");
        } else {
            fewer.push(format!(
                "{sql}\n  text {text_table} {t:?} {tc:?}\n  tree {tree_table:?} {a:?} {ac:?}"
            ));
        }
    }
    eprintln!(
        "translated statements with INSERT slots or row counts {with_params}: same {same}, tree finds more {more}, fewer or other {}",
        fewer.len()
    );
    assert!(
        fewer.is_empty(),
        "the tree misses what the text finds:\n{}",
        fewer.join("\n")
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
            return text_tail(s);
        }
        text_tail(&rewrite_full(s))
    };
    let ast = |s: &str| {
        if needs_no_rewrite(s) {
            return text_tail(s);
        }
        crate::ast::env::with(empty_context(), || crate::ast::translate(s))
            .unwrap_or_else(|_| text_tail(&rewrite_full(s)))
    };
    // Warm the cache, as a client's third round would find it: a statement is kept
    // the second time it is translated.
    for _ in 0..2 {
        for s in &stmts {
            crate::ast::env::with(empty_context(), || black_box(crate::rewrite::rewrite(s)));
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
        let c = time(&|s| crate::ast::env::with(empty_context(), || crate::rewrite::rewrite(s)));
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

/// Adds to tests/fixtures/ast-corpus/wire.hex every statement recorded in the file
/// OVERLITE_RECORDED names (OVERLITE_RECORD_STATEMENTS, `record.rs`) that no source of
/// the corpus holds yet, in the order first sent, up to 4 KiB each. `make ast-corpus`
/// records the wire suite and runs this.
#[test]
#[ignore = "rewrites a fixture: make ast-corpus"]
fn grow_the_corpus() {
    use std::collections::HashSet;
    use std::io::Write;
    let Some(recorded) = std::env::var_os("OVERLITE_RECORDED") else {
        panic!("OVERLITE_RECORDED names no file");
    };
    let recorded = std::fs::read_to_string(recorded).unwrap_or_default();
    let mut known: HashSet<Vec<u8>> = CORPUS
        .iter()
        .flat_map(|(_, lines)| lines.lines())
        .filter_map(|l| hex::decode(l.trim()).ok())
        .collect();
    let mut added = Vec::new();
    for line in recorded.lines() {
        let Ok(bytes) = hex::decode(line.trim()) else { continue };
        let Ok(raw) = std::str::from_utf8(&bytes) else { continue };
        let keep = raw.len() <= 4096 && statement_text(raw).is_ok_and(|s| !s.trim().is_empty());
        if keep && known.insert(bytes.clone()) {
            added.push(hex::encode(&bytes));
        }
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/ast-corpus/wire.hex");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap_or_else(|e| panic!("{path}: {e}"));
    for line in &added {
        writeln!(f, "{line}").unwrap_or_else(|e| panic!("{path}: {e}"));
    }
    eprintln!(
        "{} statements recorded, {} new to the corpus",
        recorded.lines().count(),
        added.len()
    );
}

#[test]
fn the_typed_printer_writes_from_the_catalog() {
    let types = std::sync::Arc::new(overlite_engine::TableTypes::of(&[
        (
            "t",
            &[
                ("id", "INTEGER"),
                ("n", "DECIMALTEXT(10,2)"),
                ("ts", "timestamp"),
                ("tz", "TIMESTAMPTZ"),
                ("d", "DATE"),
                ("b", "BOOLEAN"),
                ("v", "TSVECTOR"),
                ("s", "TEXT"),
            ],
        ),
        ("s1.u", &[("ts", "timestamptz")]),
    ]));
    let t = |sql: &str, sp: Option<&[String]>| {
        let ctx = crate::ast::env::Context::new(types.clone(), sp);
        let out = crate::ast::env::with(Some(ctx), || try_translate(sql)).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let (body, mark) = crate::ast::split_mark(&out);
        assert!(!mark.is_empty(), "{out}");
        body.to_string()
    };
    for (sql, want) in [
        // Exact arithmetic beside a numeric column, in the form a numeric column has.
        (
            "SELECT id FROM t WHERE n * 2 > 1.5",
            "SELECT id FROM t WHERE (CAST(overlite_numeric(overlite_cast(overlite_numeric_op(n, 2, '*'), \
             'numeric')) AS DECIMALTEXT) COLLATE DECIMAL) > 1.5",
        ),
        // Dates and times by their columns' types, a literal read as the column's type.
        (
            "SELECT ts + interval '1 day' FROM t WHERE d < tz AND ts >= '2024-01-01'",
            "SELECT overlite_tadd(ts, overlite_cast('1 day', 'interval'), 'TIMESTAMP', 'INTERVAL') AS \"?column?\" \
             FROM t WHERE overlite_cast(overlite_tz_in(d), 'timestamptz') < tz AND ts >= \
             overlite_cast('2024-01-01', 'timestamp')",
        ),
        // A boolean column met by a string, a tsvector's length, `%` on text.
        (
            "SELECT length(v), s % 'x' FROM t WHERE b = 'f'",
            "SELECT ts_length(v) AS \"length\", overlite_trgm_match(s, 'x', '%') AS \"?column?\" FROM t WHERE b = 0",
        ),
        // `%` on integers is a remainder.
        ("SELECT id % 2 FROM t", "SELECT id % 2 AS \"?column?\" FROM t"),
        // The session's zone where Postgres reads one.
        (
            "SELECT tz::date, EXTRACT(hour FROM tz), tz AT TIME ZONE 'UTC' FROM t",
            "SELECT overlite_cast(overlite_tz_wall(tz), 'date') AS \"tz\", overlite_extract('hour', \
             overlite_tz_wall(tz)) AS \"extract\", overlite_instant_at_time_zone(tz, 'UTC') AS \"timezone\" FROM t",
        ),
        // A select-list item whose type only Postgres knows is cast to it.
        (
            "SELECT max(ts), sum(n) FROM t",
            "SELECT overlite_cast(max(ts), 'timestamp') AS \"max\", (CAST(overlite_numeric(overlite_cast(sum(n), \
             'numeric')) AS DECIMALTEXT) COLLATE DECIMAL) AS \"sum\" FROM t",
        ),
        // Through a subquery's and a CTE's columns.
        (
            "WITH c AS (SELECT ts AS k FROM t) SELECT x.k FROM (SELECT k FROM c) AS x WHERE x.k > '2024-01-01'",
            "WITH c AS (SELECT ts AS \"k\" FROM t) SELECT x.k FROM (SELECT k FROM c) AS x WHERE x.k > \
             overlite_cast('2024-01-01', 'timestamp')",
        ),
    ] {
        assert_eq!(t(sql, None), want, "{sql}");
    }
    // The result columns' integer widths ride on the mark; a rowid's is not known.
    let ctx = crate::ast::env::Context::new(types.clone(), None);
    let out = crate::ast::env::with(Some(ctx), || {
        try_translate("SELECT id, count(*), n, id + 1 FROM t GROUP BY id, n")
    })
    .unwrap_or_default();
    assert!(out.ends_with(" /*overlite typed int4,int8,,int4*/"), "{out}");
    // An unqualified table is the search_path's.
    let sp = ["s1".to_string()];
    assert_eq!(
        t("SELECT 1 FROM u WHERE ts > '2024-01-01'", Some(&sp)),
        "SELECT 1 AS \"?column?\" FROM u WHERE ts > overlite_tz_in('2024-01-01')"
    );
    // A parameter takes the type of what it meets.
    let ctx = crate::ast::env::Context::new(types.clone(), None);
    let p = crate::ast::env::with(Some(ctx), || {
        crate::ast::param_types("SELECT 1 FROM t WHERE ts > $1 AND n = $2 + 1 AND d::text = $3 LIMIT $4")
    })
    .unwrap_or_default();
    use crate::ast::types::{Base, Ty};
    assert_eq!(
        p,
        [
            Ty::of(Base::Timestamp),
            Ty::of(Base::Int4),
            Ty::TEXT,
            Ty::of(Base::Int8)
        ]
    );
}

/// Every word of the rewrite's, the session's and the engine's sources called as a
/// function: those whose call `rewrite_full` changes are in `emit::REWRITTEN`, the
/// functions the printer does not write as they came, or are no function in Postgres'
/// grammar. A pass that learns a new function's name has to be taught to the printer.
#[test]
fn the_printer_knows_every_function_the_text_passes_rewrite() {
    use std::collections::BTreeSet;
    fn collect(dir: &std::path::Path, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                collect(&path, out);
                continue;
            }
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            for w in src.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
                if !w.is_empty() && w.len() < 40 && !w.as_bytes()[0].is_ascii_digit() {
                    out.insert(w.to_ascii_lowercase());
                }
            }
        }
    }
    // Words the grammar reads as no call: keywords, a system column, the operators.
    const NO_CALL: &[&str] = &[
        "ctid",
        "current_timestamp",
        "ilike",
        "is_operator",
        "lateral",
        "like",
        "no_operator",
        "operator",
        "pg_operator",
        "raw_operator",
        "re_any_operator",
        "regoperator",
        "rewrite_any_operator",
    ];
    let mut words = BTreeSet::new();
    collect(
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")),
        &mut words,
    );
    collect(
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../engine/src")),
        &mut words,
    );
    let mut missing = Vec::new();
    for w in &words {
        let changed = ["SELECT @(a) FROM t", "SELECT @(a, b) FROM t", "SELECT @() FROM t"]
            .iter()
            .any(|t| {
                let sql = t.replace('@', w);
                let call = sql["SELECT ".len()..sql.find(" FROM").unwrap_or(sql.len())].to_string();
                !text_tail(&rewrite_full(&sql)).contains(&call)
            });
        if changed && !crate::ast::emit::REWRITTEN.contains(&w.as_str()) && !NO_CALL.contains(&w.as_str()) {
            missing.push(w.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "rewritten by the text passes, unknown to the printer: {missing:?}"
    );
}

/// Every statement of the corpus left to the text passes, one per line: its source,
/// whether it reads or writes rows, the first node the printer refused, and the
/// statement. `OVERLITE_LEFT=<file> cargo test --release -p overlite-postgres --lib
/// list_what_the_text_passes_translate -- --ignored`.
#[test]
#[ignore = "a listing, not a check"]
fn list_what_the_text_passes_translate() {
    use std::io::Write;
    let path = std::env::var_os("OVERLITE_LEFT").unwrap_or_else(|| "target/text-passes.txt".into());
    let mut f = std::fs::File::create(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    for (src, sql) in corpus() {
        let sql = wrapped_query(&sql).unwrap_or(sql);
        if needs_no_rewrite(&sql) {
            continue;
        }
        if let Err(why) = typed(&sql) {
            let rows = if crate::rewrite::cache::reads_or_writes_rows(&sql) {
                "rows"
            } else {
                "utility"
            };
            let one = sql.split_whitespace().collect::<Vec<_>>().join(" ");
            writeln!(f, "{src}\t{rows}\t{why}\t{one}").unwrap_or_else(|e| panic!("{path:?}: {e}"));
        }
    }
}

/// The kind of statement sql is, by its parse tree: the node's name, with the object
/// kind of a DROP, the first action of an ALTER TABLE and the setting of a SET; "parse
/// error" for what Postgres refuses to parse.
pub(crate) fn node_kind(sql: &str) -> String {
    use pg_query::NodeEnum;
    let Ok(parsed) = crate::ast::parse::parse(sql) else {
        return "parse error".into();
    };
    let mut kinds = Vec::new();
    for raw in &parsed.stmts {
        let Some(node) = raw.stmt.as_deref().and_then(|s| s.node.as_ref()) else {
            continue;
        };
        let name = format!("{node:?}");
        let name = name.split('(').next().unwrap_or("").to_string();
        let detail = match node {
            NodeEnum::DropStmt(d) => format!(" {:?}", d.remove_type()),
            NodeEnum::AlterTableStmt(a) => a
                .cmds
                .iter()
                .find_map(|c| {
                    if let Some(NodeEnum::AlterTableCmd(c)) = c.node.as_ref() {
                        Some(format!(" {:?}", c.subtype()))
                    } else {
                        None
                    }
                })
                .unwrap_or_default(),
            NodeEnum::VariableSetStmt(v) => format!(" {}", v.name),
            NodeEnum::RenameStmt(r) => format!(" {:?}", r.rename_type()),
            NodeEnum::GrantStmt(g) => format!(" {:?}", g.objtype()),
            _ => String::new(),
        };
        kinds.push(name + &detail);
    }
    if kinds.is_empty() {
        "empty".into()
    } else {
        kinds.join("; ")
    }
}

/// Counts, by node kind (`node_kind`), the statements recorded in the file
/// OVERLITE_RECORDED_TEXT_PASSES names (`record::text_passes`): what reached the text
/// passes in a running server. Each line: occurrences, distinct statements, kind.
/// OVERLITE_LEFT names a file to write each distinct statement to, with its kind.
#[test]
#[ignore = "a listing, not a check"]
fn count_what_reached_the_text_passes() {
    use std::collections::BTreeSet;
    use std::io::Write;
    let Some(recorded) = std::env::var_os("OVERLITE_RECORDED_TEXT_PASSES") else {
        panic!("OVERLITE_RECORDED_TEXT_PASSES names no file");
    };
    let recorded = std::fs::read_to_string(recorded).unwrap_or_default();
    let mut by_kind: BTreeMap<String, (usize, BTreeSet<String>)> = BTreeMap::new();
    for line in recorded.lines() {
        let Ok(bytes) = hex::decode(line.trim()) else { continue };
        let Ok(sql) = String::from_utf8(bytes) else { continue };
        let slot = by_kind.entry(node_kind(&sql)).or_default();
        slot.0 += 1;
        slot.1.insert(sql);
    }
    let mut rows: Vec<_> = by_kind.iter().collect();
    rows.sort_by_key(|(k, (n, d))| (std::cmp::Reverse(d.len()), std::cmp::Reverse(*n), (*k).clone()));
    let (total, distinct) = rows.iter().fold((0, 0), |(t, d), (_, (n, s))| (t + n, d + s.len()));
    eprintln!("{total} statements, {distinct} distinct");
    for (kind, (n, d)) in &rows {
        eprintln!("{n:>7} {:>6}  {kind}", d.len());
    }
    if let Some(path) = std::env::var_os("OVERLITE_LEFT") {
        let mut f = std::fs::File::create(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        for (kind, (_, d)) in &rows {
            for sql in d {
                let one = sql.split_whitespace().collect::<Vec<_>>().join(" ");
                writeln!(f, "{kind}\t{one}").unwrap_or_else(|e| panic!("{path:?}: {e}"));
            }
        }
    }
}

/// The session's classifiers on the tree against their text readings, over the
/// corpus: each statement Postgres' grammar reads is classified both ways, and every
/// difference has to be one `CLASSIFIED` lists, with the reason the tree is right.
#[test]
fn classifiers_read_the_tree_as_the_text_did() {
    use crate::ast::classify::{self as c, read};
    type Pair = (&'static str, fn(&str) -> Option<bool>, fn(&str) -> bool);
    let pairs: &[Pair] = &[
        (
            "tx_control",
            |s| read(s, c::tx_control).map(|x| x.is_some()),
            |s| crate::session::utility::TxControl::of_text(s).is_some(),
        ),
        (
            "savepoint",
            |s| read(s, c::savepoint).map(|x| x.is_some()),
            |s| crate::session::utility::SavepointKind::of_text(s).is_some(),
        ),
        (
            "cursor",
            |s| read(s, c::cursor),
            crate::session::cursor::is_cursor_statement_text,
        ),
        (
            "schema_ddl",
            |s| read(s, c::schema_ddl),
            crate::session::utility::is_schema_ddl_text,
        ),
        (
            "role_ddl",
            |s| read(s, c::role_ddl),
            crate::auth::roles::is_role_ddl_text,
        ),
        ("grant", |s| read(s, c::grant), crate::auth::grants::is_grant_text),
        ("rls_ddl", |s| read(s, c::rls_ddl), crate::auth::rls::is_rls_ddl_text),
        (
            "listen_notify",
            |s| read(s, c::listen_notify),
            crate::session::notify::is_listen_notify_text,
        ),
        (
            "sequence_ddl",
            |s| read(s, c::sequence_ddl),
            crate::ddl::sequences::is_sequence_ddl_text,
        ),
        (
            "type_ddl",
            |s| read(s, c::type_ddl),
            crate::ddl::enums::is_type_ddl_text,
        ),
        ("comment", |s| read(s, c::comment), crate::ddl::comment::is_comment_text),
        (
            "set_role",
            |s| read(s, c::set_role).map(|x| x.is_some()),
            crate::auth::session_user::is_set_role_text,
        ),
        (
            "sql_prepare",
            |s| read(s, c::sql_prepare),
            |s| {
                matches!(
                    crate::session::utility::first_word_upper(s).as_str(),
                    "PREPARE" | "EXECUTE" | "DEALLOCATE"
                )
            },
        ),
        (
            "copy",
            |s| read(s, c::copy),
            |s| crate::session::copy::parse_copy_text(s).is_some(),
        ),
        (
            "time_zone",
            |s| read(s, c::time_zone),
            |s| crate::session::time_zone::zone_change_text(s).is_some(),
        ),
        (
            "search_path",
            |s| read(s, c::search_path),
            crate::session::search_path::is_search_path_stmt_text,
        ),
        (
            "explain",
            |s| read(s, c::explain),
            crate::session::explain::is_explain_text,
        ),
        (
            "database_ddl",
            |s| read(s, c::database_ddl),
            crate::ddl::database::is_database_ddl_text,
        ),
        (
            "creates_exclusions",
            |s| read(s, c::creates_exclusions),
            crate::ddl::exclude::creates_exclusions_text,
        ),
    ];
    let mut differ: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut read_by_tree = 0;
    let probes = CLASSIFIER_PROBES.iter().map(|s| ("probe", (*s).to_string()));
    for (_, sql) in corpus().into_iter().chain(probes) {
        if crate::ast::parse::shared(&sql).is_ok() {
            read_by_tree += 1;
        }
        for (name, tree, text) in pairs {
            let Some(by_tree) = tree(&sql) else { continue };
            if by_tree != text(&sql) {
                let why = CLASSIFIED
                    .iter()
                    .find(|(n, f, _)| n == name && f(&sql))
                    .map_or("unexplained", |(_, _, why)| why);
                differ
                    .entry(format!("{name} (tree {by_tree}): {why}"))
                    .or_default()
                    .push(sql.split_whitespace().collect::<Vec<_>>().join(" "));
            }
        }
    }
    eprintln!("{read_by_tree} statements read by the tree");
    for (k, v) in &differ {
        eprintln!("{} {k}", v.len());
        for s in v.iter().take(6) {
            eprintln!("    {}", s.chars().take(200).collect::<String>());
        }
    }
    let unexplained: usize = differ
        .iter()
        .filter(|(k, _)| k.ends_with("unexplained"))
        .map(|(_, v)| v.len())
        .sum();
    assert_eq!(unexplained, 0, "classifications the tree reads otherwise, unexplained");
}

/// Where a classifier on the tree reads a statement otherwise than its text did: the
/// classifier, a test of the statement, and why the tree is right.
type Classified = (&'static str, fn(&str) -> bool, &'static str);

const CLASSIFIED: &[Classified] = &[(
    "set_role",
    |s| {
        let w: Vec<String> = s.split_whitespace().take(2).map(str::to_ascii_lowercase).collect();
        w.first().is_some_and(|w| w == "set") && w.get(1).is_some_and(|w| w == "local" || w == "session")
    },
    "SET LOCAL ROLE and SET SESSION ROLE set the role, as SET ROLE does: the text read ROLE as the second \
     word only, and SET LOCAL ROLE, with which a pool lends a connection to a role for one transaction, was a \
     SET of nothing that left the session's role in place, its privileges and row-level security unchanged",
)];

/// Statements the corpus lacks that tell the classifiers apart, beside it.
const CLASSIFIER_PROBES: &[&str] = &[
    "SET LOCAL ROLE web_anon",
    "SET SESSION ROLE alice",
    "SET LOCAL SESSION AUTHORIZATION alice",
    "SET ROLE NONE",
    "RESET ROLE",
    "CREATE TEMP SEQUENCE s",
    "ALTER TABLE t RENAME COLUMN a TO \"ENABLE ROW LEVEL SECURITY\"",
    "ALTER TABLE t ADD COLUMN c int, ENABLE ROW LEVEL SECURITY",
    "COMMIT PREPARED 'x'",
    "PREPARE TRANSACTION 'x'",
    "SET LOCAL TIME ZONE 'UTC'",
    "SHOW search_path",
    "RESET ALL",
];
