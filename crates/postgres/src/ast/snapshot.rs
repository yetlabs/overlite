//! The corpus (tests/fixtures/ast-corpus, docs/adr/0001-sql-ast.md) as a snapshot of
//! the printer's output: each statement's translation under an empty catalog, or why
//! it has none, in tests/fixtures/ast-corpus/printed.txt, one line per statement in the
//! corpus' order. A change to the printer that changes a line fails the test, which
//! shows the statement and both lines; once the change is reviewed,
//! `OVERLITE_SNAPSHOT=update cargo test -p overlite-postgres --lib snapshot` writes the
//! file anew, and the diff of the file is what a review reads.

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

/// Every statement of the corpus, by its source, blank ones left out.
pub(crate) fn corpus() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (src, hex_lines) in CORPUS {
        for line in hex_lines.lines() {
            let Ok(bytes) = hex::decode(line.trim()) else { continue };
            let Ok(sql) = String::from_utf8(bytes) else { continue };
            if !sql.trim().is_empty() {
                out.push((*src, sql));
            }
        }
    }
    out
}

/// A context with no tables: the corpus' statements are of applications whose schemas
/// are not here.
pub(crate) fn empty_context() -> Option<crate::ast::env::Context> {
    thread_local! {
        static EMPTY: std::sync::Arc<overlite_engine::TableTypes> =
            std::sync::Arc::new(overlite_engine::TableTypes::of(&[]));
    }
    Some(crate::ast::env::Context::new(EMPTY.with(Clone::clone), None))
}

/// The typed translation under an empty catalog, without the engine's mark.
pub(crate) fn typed(sql: &str) -> Result<String, &'static str> {
    let out = crate::ast::env::with(empty_context(), || crate::ast::translate(sql))?;
    Ok(crate::ast::split_mark(&out).0.to_string())
}

/// One line of the snapshot: the translation, or why there is none (a refusal's
/// message where Postgres refuses the statement too), on one line.
fn line(sql: &str) -> String {
    let out = match crate::ast::env::with(empty_context(), || crate::ast::translate(sql)) {
        Ok(out) => ["ok\t", &out].concat(),
        Err(crate::ast::emit::POSTGRES_REFUSAL) => {
            let r = crate::ast::emit::take_refusal();
            [
                "refused\t",
                &r.map_or_else(String::new, |r| format!("{} {}", r.state, r.message)),
            ]
            .concat()
        }
        Err(why) => ["no\t", why].concat(),
    };
    out.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "\\r")
}

const SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/ast-corpus/printed.txt"
);

#[test]
fn the_printer_writes_the_corpus_as_the_snapshot_has_it() {
    let corpus = corpus();
    let got: Vec<String> = corpus.iter().map(|(_, sql)| line(sql)).collect();
    if std::env::var("OVERLITE_SNAPSHOT").is_ok_and(|v| v == "update") {
        std::fs::write(SNAPSHOT, got.join("\n") + "\n").unwrap_or_else(|e| panic!("{SNAPSHOT}: {e}"));
        return;
    }
    let want = std::fs::read_to_string(SNAPSHOT).unwrap_or_else(|e| panic!("{SNAPSHOT}: {e}"));
    let want: Vec<&str> = want.lines().collect();
    let mut differ = Vec::new();
    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, ((src, sql), g)) in corpus.iter().zip(&got).enumerate() {
        *tally.entry(g.split('\t').next().unwrap_or("")).or_default() += 1;
        if want.get(i) != Some(&g.as_str()) {
            differ.push(format!(
                "{src} #{i}: {}\n  snapshot: {}\n  printed:  {g}",
                sql.chars().take(300).collect::<String>(),
                want.get(i).copied().unwrap_or("(none)")
            ));
        }
    }
    eprintln!("{} statements: {tally:?}", corpus.len());
    assert!(
        differ.is_empty() && want.len() == got.len(),
        "{} of {} lines differ from {SNAPSHOT} ({} lines there); review, then OVERLITE_SNAPSHOT=update:\n{}",
        differ.len(),
        got.len(),
        want.len(),
        differ.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );
}

#[test]
fn printer_writes_what_postgres_read() {
    let t = |sql: &str| typed(sql);
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
            "SELECT overlite_cast(overlite_tz_in(x), 'timestamptz(3)') AS \"x\", (overlite_cast('{}', 'jsonb') COLLATE JSONB) \
            AS \"jsonb\", hstore_in('a=>1') AS \"hstore\", y AS \"y\" FROM t"
        )
    );
    // `%` beside a string is pg_trgm's, whatever the catalog says of the column.
    assert_eq!(
        t("SELECT name FROM t WHERE name % 'x'").as_deref(),
        Ok("SELECT name FROM t WHERE overlite_trgm_match(name, 'x', '%')")
    );
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
        let out =
            crate::ast::env::with(Some(ctx), || crate::ast::translate(sql)).unwrap_or_else(|e| panic!("{sql}: {e}"));
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
        crate::ast::translate("SELECT id, count(*), n, id + 1 FROM t GROUP BY id, n")
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
        let keep = raw.len() <= 4096 && !raw.trim().is_empty();
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
        "{} statements recorded, {} new to the corpus: OVERLITE_SNAPSHOT=update to print them",
        recorded.lines().count(),
        added.len()
    );
}

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
    let at_the_door = corpus()
        .into_iter()
        .filter_map(|(src, sql)| Some((src, crate::rewrite::blank::statement_text(&sql).ok()?.into_owned())));
    for (_, sql) in at_the_door.chain(probes) {
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
