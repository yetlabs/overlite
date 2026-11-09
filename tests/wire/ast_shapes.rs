//! Answers of statements the AST path now translates (docs/adr/0001-sql-ast.md, stage
//! 4: query shapes), and of plain statements that leave the fast path for it when a
//! column's type makes the typed translation differ: each failed before the stage,
//! held to Postgres 17's answer.

use crate::common::*;

/// Each row's columns as text joined by commas, over the simple protocol.
#[track_caller]
fn rows(conn: &Conn, sql: &str) -> Vec<String> {
    raw_rows(conn, sql)
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|c| c.map_or_else(|| "NULL".into(), |b| String::from_utf8_lossy(&b).into_owned()))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

/// The type OIDs of a statement's result columns, as the simple protocol sends them and
/// as Describe gives them.
#[track_caller]
fn oids(server: &Server, sql: &str) -> (Vec<u32>, Vec<u32>) {
    let simple = connect(server).pg_exec(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let described = connect_extended(server)
        .prepare("", sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    (
        simple[0].fields.iter().map(|f| f.data_type_oid).collect(),
        described.fields.iter().map(|f| f.data_type_oid).collect(),
    )
}

// A plain lookup the text passes leave as it is took the fast path whatever its
// columns' types, so a literal compared with a timestamptz column compared their texts
// (a date alone sorts before its midnight's text) and a narrow integer went out as an
// int8. Such a statement now takes the typed translation; one over a table without
// such columns stays on the fast path.
#[test]
fn a_plain_lookup_over_typed_columns_reads_them_by_type() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE fp (k serial PRIMARY KEY, i int, s smallint, d date, tz timestamptz, v text)",
    );
    must_exec(
        &conn,
        "INSERT INTO fp (i, s, d, tz, v) VALUES (1, 2, '2024-01-02', '2024-01-02 00:00:00+00', 'x')",
    );
    for (sql, want) in [
        ("SELECT k FROM fp WHERE tz > '2024-01-02'", Vec::<&str>::new()),
        ("SELECT k FROM fp WHERE tz = '2024-01-02'", vec!["1"]),
        ("SELECT v FROM fp WHERE d > '2024-1-1'", vec!["x"]),
    ] {
        assert_eq!(rows(&conn, sql), want, "{sql}");
    }
    for (sql, want) in [
        ("SELECT k, i, s FROM fp WHERE k = 1", vec![23, 23, 21]),
        ("SELECT * FROM fp", vec![23, 23, 21, 1082, 1184, 25]),
    ] {
        let (simple, described) = oids(&server, sql);
        assert_eq!(simple, want, "{sql}");
        assert_eq!(described, want, "{sql} described");
    }
}

fn arrays(conn: &Conn) {
    must_exec(conn, "CREATE TABLE ar (id int PRIMARY KEY, a int[], t text[])");
    must_exec(conn, "INSERT INTO ar VALUES (1, '{1,2,3}', '{x,y}'), (2, '{4}', '{z}')");
}

// Arrays in the shapes the text passes left in Postgres' syntax, which SQLite refused:
// a subscript of an expression and a slice, `<> ALL (subquery)`, ARRAY((subquery)); and
// `||` of arrays, which they wrote as a concatenation of texts.
#[test]
fn array_subscripts_slices_and_concatenation() {
    let server = start_server();
    let conn = connect(&server);
    arrays(&conn);
    assert_eq!(
        rows(
            &conn,
            "SELECT id, (a)[2], a[2:3], a || 9, a || ARRAY[7,8], 0 || a FROM ar ORDER BY id"
        ),
        [
            "1,2,{2,3},{1,2,3,9},{1,2,3,7,8},{0,1,2,3}",
            "2,NULL,{},{4,9},{4,7,8},{0,4}"
        ]
    );
    assert_eq!(
        rows(&conn, "SELECT id FROM ar WHERE id <> ALL (SELECT 2) ORDER BY id"),
        ["1"]
    );
    assert_eq!(
        rows(&conn, "SELECT ARRAY((SELECT id FROM ar ORDER BY id DESC))"),
        ["{2,1}"]
    );
    // Each goes out as the array it is: int[] as int4[].
    let (simple, described) = oids(&server, "SELECT a, a[1:2], a || 1, t || '{w}' FROM ar");
    assert_eq!(simple, [1007, 1007, 1007, 1009]);
    assert_eq!(described, simple);
}

// array_agg is an array: the engine's aggregate wrote Postgres' text without quoting
// an element, which array_length and a subscript did not read.
#[test]
fn array_agg_is_an_array() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE ag (g int, i int, b bigint, v text)");
    must_exec(
        &conn,
        "INSERT INTO ag VALUES (1, 1, 10, 'a b'), (1, 2, 20, 'c'), (2, 3, 30, NULL)",
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT array_agg(v ORDER BY i), array_length(array_agg(i), 1), (array_agg(i ORDER BY i))[2], \
             array_agg(i ORDER BY i DESC) FILTER (WHERE g = 1) FROM ag"
        ),
        [r#"{"a b",c,NULL},3,2,{2,1}"#]
    );
    assert_eq!(rows(&conn, "SELECT array_agg(i) FROM ag WHERE false"), ["NULL"]);
    let (simple, _) = oids(&server, "SELECT array_agg(i), array_agg(v) FROM ag");
    assert_eq!(simple, [1007, 1009]);
}

// avg of integers is a numeric and sum of bigints too, as Postgres types them, where
// they went out as a double and an int8.
#[test]
fn aggregates_of_integers_are_numerics() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE ag (g int, i int, b bigint, r real)");
    must_exec(
        &conn,
        "INSERT INTO ag VALUES (1, 1, 10, 0.5), (1, 2, 20, 1), (2, 3, 30, 2)",
    );
    assert_eq!(
        rows(&conn, "SELECT avg(i), sum(b), avg(i) FILTER (WHERE g = 1) FROM ag"),
        ["2.0000000000000000,60,1.5000000000000000"]
    );
    let (simple, described) = oids(&server, "SELECT avg(i), sum(b), sum(i), r * 1.5 FROM ag GROUP BY r");
    assert_eq!(simple, [1700, 1700, 20, 701]);
    assert_eq!(described, simple);
}

// DISTINCT ON keeps the first row of each group before LIMIT counts rows: the text pass
// limited the rows it chose among. A set operation on the right of another binds first,
// as Postgres reads INTERSECT: SQLite read the compound left to right.
#[test]
fn distinct_on_before_limit_and_set_operations_on_the_right() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE src (id int, n int)");
    must_exec(&conn, "INSERT INTO src VALUES (1, 1), (1, 2), (2, 3)");
    let mut got = rows(&conn, "SELECT DISTINCT ON (id) id FROM src LIMIT 2");
    got.sort();
    assert_eq!(got, ["1", "2"]);
    assert_eq!(rows(&conn, "SELECT 1 UNION SELECT 2 INTERSECT SELECT 3"), ["1"]);
}

// What the text passes wrote that SQLite refused: a jsonb aggregate's FILTER after the
// cast around it, a row-locking clause on lines of its own, ctid quoted.
#[test]
fn clauses_the_text_passes_misplaced() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE dz (g int, k int)");
    must_exec(&conn, "INSERT INTO dz VALUES (1, 1), (1, 2), (2, 3)");
    assert_eq!(
        rows(
            &conn,
            "SELECT jsonb_agg(k) FILTER (WHERE k > 1) = '[2, 3]'::jsonb FROM dz"
        ),
        ["t"]
    );
    must_exec(&conn, "BEGIN");
    assert_eq!(
        rows(&conn, "SELECT k FROM dz WHERE g = 1 ORDER BY k\nFOR\nUPDATE"),
        ["1", "2"]
    );
    must_exec(&conn, "COMMIT");
    assert_eq!(rows(&conn, r#"SELECT "ctid" IS NOT NULL FROM dz LIMIT 1"#), ["t"]);
}
