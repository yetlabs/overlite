use crate::common::*;

// TestRangeOverlapAndContainment: the "&&" overlap operator and range @> range
// containment over the text-range representation.
#[test]
fn range_overlap_and_containment() {
    let conn = connect(&start_server());

    let q = |sql: &str| -> bool { conn.query_row(sql, &[]).scan().unwrap() };

    // Range overlap.
    assert!(q("SELECT int4range(1,10) && int4range(5,15)"));
    assert!(!q("SELECT int4range(1,5) && int4range(5,10)")); // [1,5) vs [5,10): touch, no overlap
    assert!(q("SELECT int4range(1,5,'[]') && int4range(5,10)")); // [1,5] includes 5
    assert!(q("SELECT numrange(NULL, 10) && numrange(3, NULL)")); // (-inf,10) & (3,+inf)

    // Range contains range.
    assert!(q("SELECT int4range(1,20) @> int4range(5,10)"));
    assert!(!q("SELECT int4range(1,10) @> int4range(5,20)"));
    assert!(q("SELECT int4range(1,10) @> 5")); // range @> element still works

    // Array overlap.
    assert!(q("SELECT ARRAY[1,2,3] && ARRAY[3,4,5]"));
    assert!(!q("SELECT ARRAY[1,2] && ARRAY[3,4]"));
}

// A range's intersection (`*`), union (`+`) and difference (`-`), for each built-in
// range type, of literals, columns and parameters: Postgres 18's answers, its 22000 for
// a union or a difference in two pieces, and 42883 for two range types.
#[test]
fn range_intersection_union_and_difference() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE rs (id int, a int4range, b int4range, d daterange, t tstzrange)",
    );
    must_exec(
        &conn,
        "INSERT INTO rs VALUES (1, '[1,5)', '[3,8)', '[2020-01-01,2020-02-01)', '[2020-01-01 10:00+00,2020-01-01 12:00+00)')",
    );
    for (q, want) in [
        ("SELECT int4range(1,5) * int4range(3,8)", "[3,5)"),
        ("SELECT int4range(1,5) + int4range(3,8)", "[1,8)"),
        ("SELECT int4range(1,5) - int4range(3,8)", "[1,3)"),
        ("SELECT '[1,5)'::int4range * '[7,9)'::int4range", "empty"),
        ("SELECT int8range(1,10) - int8range(1,5)", "[5,10)"),
        ("SELECT int8range(1,5,'[]') + int8range(6,8)", "[1,8)"),
        ("SELECT numrange(1.5,2.5) + numrange(2.5,3)", "[1.5,3)"),
        ("SELECT numrange(1.5,3) - numrange(2,3,'[]')", "[1.5,2)"),
        ("SELECT numrange(NULL, 5) * numrange(2, NULL)", "[2,5)"),
        (
            "SELECT daterange('2020-01-01','2020-02-01') * daterange('2020-01-10','2020-03-01')",
            "[2020-01-10,2020-02-01)",
        ),
        (
            "SELECT tsrange('2020-01-01 10:00','2020-01-01 12:00') - tsrange('2020-01-01 11:00','2020-01-01 13:00')",
            "[\"2020-01-01 10:00:00\",\"2020-01-01 11:00:00\")",
        ),
        ("SELECT 'empty'::int4range + int4range(1,2)", "[1,2)"),
        ("SELECT a * b FROM rs", "[3,5)"),
        ("SELECT a + b FROM rs", "[1,8)"),
        ("SELECT b - a FROM rs", "[5,8)"),
        (
            "SELECT d * '[2020-01-15,2020-03-01)' FROM rs",
            "[2020-01-15,2020-02-01)",
        ),
        (
            "SELECT t + tstzrange('2020-01-01 11:00+00','2020-01-01 13:00+00') FROM rs",
            "[\"2020-01-01 10:00:00+00\",\"2020-01-01 13:00:00+00\")",
        ),
        ("SELECT count(*) FROM rs WHERE a * b = int4range(3,5)", "1"),
        ("SELECT pg_typeof(a * b) FROM rs", "int4range"),
    ] {
        assert_eq!(scan_str(&conn, q), want, "{q}");
    }
    let r: String = conn
        .query_row("SELECT a - $1::int4range FROM rs", &[&"[4,10)"])
        .scan()
        .unwrap();
    assert_eq!(r, "[1,4)");
    for (q, code, message) in [
        (
            "SELECT int4range(1,2) + int4range(3,4)",
            "22000",
            "result of range union would not be contiguous",
        ),
        (
            "SELECT daterange('2020-01-01','2020-02-01') - daterange('2020-01-10','2020-01-20')",
            "22000",
            "result of range difference would not be contiguous",
        ),
        (
            "SELECT numrange(1,3) * int4range(1,2)",
            "42883",
            "operator does not exist: numrange * int4range",
        ),
    ] {
        let err = conn.exec(q, &[]).expect_err(q);
        assert_eq!(
            (err.code(), err.pg().map(|e| e.message.as_str())),
            (code, Some(message)),
            "{q}"
        );
    }
}
