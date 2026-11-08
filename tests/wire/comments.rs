use crate::common::*;

// A comment is whitespace to Postgres. An apostrophe in a `--` comment before a
// `$n` opened a string to the scanners, so the parameter was not counted: Prisma's
// `db pull` sends exactly that, and Parse answered 42P02, missing named argument.
// Each statement here runs beside its twin without comments, down both routes,
// and must answer the same.
const PAIRS: &[(&str, &str, &[&str])] = &[
    (
        "SELECT a FROM t WHERE -- when it's a partition\n b = $1",
        "SELECT a FROM t WHERE b = $1",
        &["x"],
    ),
    (
        "SELECT a FROM t WHERE /* it's */ b = $1",
        "SELECT a FROM t WHERE b = $1",
        &["x"],
    ),
    (
        "SELECT a FROM t WHERE /* outer /* it's */ still ' in */ b = $1",
        "SELECT a FROM t WHERE b = $1",
        &["x"],
    ),
    (
        "SELECT a FROM t -- $2 is not a parameter\nWHERE b = $1",
        "SELECT a FROM t WHERE b = $1",
        &["x"],
    ),
    (
        "SELECT a /* $3 */ FROM t WHERE c = $1 -- and $2",
        "SELECT a FROM t WHERE c = $1",
        &["2"],
    ),
    (
        "SELECT a FROM t WHERE b = $1 -- trailing '",
        "SELECT a FROM t WHERE b = $1",
        &["x"],
    ),
    (
        "SELECT a FROM t WHERE b = 'it''s' -- '\n OR c = $1 ORDER BY a",
        "SELECT a FROM t WHERE b = 'it''s' OR c = $1 ORDER BY a",
        &["1"],
    ),
    (
        "SELECT a FROM t WHERE b = '-- not' OR c = $1 -- it's",
        "SELECT a FROM t WHERE b = '-- not' OR c = $1",
        &["2"],
    ),
    (
        "SELECT a FROM t WHERE b = '/* not */' /* it's */ OR c = $1 ORDER BY a",
        "SELECT a FROM t WHERE b = '/* not */' OR c = $1 ORDER BY a",
        &["1"],
    ),
    (
        "SELECT a FROM t WHERE c = $1 -- FOR UPDATE",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "SELECT a FROM t /* RETURNING a */ WHERE c = $1",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "SELECT a FROM t -- \"x\nWHERE c = $1",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "SELECT a FROM t -- $$\nWHERE c = $1",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "SELECT a FROM t WHERE b = E'it\\'s' -- x'\n OR c = $1 ORDER BY a",
        "SELECT a FROM t WHERE b = E'it\\'s' OR c = $1 ORDER BY a",
        &["2"],
    ),
    (
        "SELECT a FROM t /* -- */ WHERE c = $1",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "SELECT a FROM t -- /*\nWHERE c = $1",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "SELECT a FROM t WHERE c = $1 -- why?",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "SELECT a::text /* ::int 'x */ FROM t WHERE c = $1",
        "SELECT a::text FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "/* BEGIN; */ SELECT a FROM t WHERE c = $1",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "SELECT a FROM t WHERE b ILIKE $1 /* it's ~~* */ ORDER BY a",
        "SELECT a FROM t WHERE b ILIKE $1 ORDER BY a",
        &["%S"],
    ),
    (
        "SELECT a FROM t WHERE c = $1 /* ; DELETE FROM t; */",
        "SELECT a FROM t WHERE c = $1",
        &["1"],
    ),
    (
        "UPDATE t SET b = b /* , c = $2 */ WHERE c = $1 RETURNING a -- '",
        "UPDATE t SET b = b WHERE c = $1 RETURNING a",
        &["1"],
    ),
];

fn rows(r: &PgResult) -> Vec<Vec<Option<Vec<u8>>>> {
    r.rows.clone()
}

#[test]
fn comments_hold_nothing() {
    let server = start_server();
    let ext = connect_extended(&server);
    let simple = connect(&server);
    must_exec(&ext, "CREATE TABLE t (a int PRIMARY KEY, b text, c int)");
    must_exec(&ext, "INSERT INTO t VALUES (1, 'x', 1), (2, 'it''s', 2), (3, 'S', 3)");

    for (with, without, params) in PAIRS {
        let n = |sql: &str| ext.prepare("", sql).unwrap().param_oids.len();
        assert_eq!(n(with), n(without), "parameters of {with:?}");
        let got = ext.exec_params(with, params, &[], &[], &[]);
        let want = ext.exec_params(without, params, &[], &[], &[]).unwrap();
        let got = got.unwrap_or_else(|e| panic!("{with:?}: {e}"));
        assert_eq!(rows(&got), rows(&want), "extended {with:?}");

        // The simple route, with the parameters written in.
        let inline = |sql: &str| sql.replace("$1", &format!("'{}'", params[0]));
        let got = simple.pg_exec(inline(with)).unwrap_or_else(|e| panic!("{with:?}: {e}"));
        let want = simple.pg_exec(inline(without)).unwrap();
        assert_eq!(got.len(), want.len(), "simple {with:?}");
        assert_eq!(rows(&got[0]), rows(&want[0]), "simple {with:?}");
    }

    // What Prisma's db pull sends, over the emulated catalog.
    let pull = "SELECT tbl.relname AS table_name\nFROM pg_class AS tbl\nWHERE\n  ( -- (relkind = 'r' and relispartition = true) matches partition table \"duplicates\"\n    (tbl.relkind = 'r' AND tbl.relispartition = false)\n      OR -- when it's a partition\n    tbl.relkind = 'p'\n  )\n  AND tbl.relname = $1";
    assert_eq!(ext.prepare("", pull).unwrap().param_oids, [19]);
    let r = ext.exec_params(pull, &["t"], &[], &[], &[]).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.text(0, 0), "t");

    // A comment in a batch, and in a function's body, which is inlined in parentheses.
    let r = simple.pg_exec("SELECT 1; -- it's\nSELECT 2 /* ; */").unwrap();
    assert_eq!(r.len(), 2);
    must_exec(
        &simple,
        "CREATE FUNCTION inc(int) RETURNS int LANGUAGE sql AS $$ SELECT $1 + 1 -- it's $2\n $$",
    );
    assert_eq!(scan_str(&simple, "SELECT inc(1)::text"), "2");
    assert_eq!(scan_str(&simple, "SELECT inc(a)::text FROM t WHERE a = 2"), "3");
}
