//! Answers the text passes got wrong for statements the AST path now translates whole
//! (docs/adr/0001-sql-ast.md, stage 2): each failed before the stage, held to Postgres
//! 17's answer.

use crate::common::*;

/// The column names of a statement's result.
fn names(conn: &Conn, sql: &str) -> Vec<String> {
    let r = conn
        .exec_params(sql, &[] as &[&str], &[], &[], &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.fields.iter().map(|f| f.name.clone()).collect()
}

// The text passes read a LIKE's pattern up to the next word that can end one, and
// CASE has four: `LIKE CASE WHEN ...` became `GLOB overlite_like_glob(CASE, '\') WHEN
// ...`, a syntax error. The tree has the whole CASE as the pattern.
#[test]
fn like_takes_a_case_for_its_pattern() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (name text, pick boolean)");
    must_exec(
        &conn,
        "INSERT INTO t VALUES ('apple', true), ('banana', false), ('avocado', false)",
    );
    for sql in [
        "SELECT name FROM t WHERE name LIKE CASE WHEN pick THEN 'a%' ELSE 'b%' END ORDER BY name",
        "SELECT name FROM t WHERE name ILIKE CASE WHEN pick THEN 'A%' ELSE 'B%' END ORDER BY name",
    ] {
        assert_eq!(query_column(&conn, sql, 0), ["apple", "banana"], "{sql}");
    }
}

// A range literal cast twice, as Django writes a range field's value
// (`'[0,10)'::int4range::int4range`): the text passes left the second `::` in the
// SQLite SQL, or, after a NULL::int8range, wrapped the whole VALUES row in a CAST.
// Either was a syntax error, so the row could not be written.
#[test]
fn a_range_cast_twice_is_written() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE r (id int, ints int4range, big int8range)");
    must_exec(
        &conn,
        "INSERT INTO r (id, ints, big) VALUES (1, '[0,10)'::int4range::int4range, NULL::int8range)",
    );
    must_exec(
        &conn,
        "INSERT INTO r (id, big, ints) VALUES (2, NULL::int8range, '[1,5]'::int4range::int4range)",
    );
    assert_eq!(
        query_column(&conn, "SELECT ints::text FROM r ORDER BY id", 0),
        ["[0,10)", "[1,6)"]
    );
}

// jsonb compares as jsonb (a number by its value, an object by its size first)
// however the client spells the type: the text passes gave `::jsonb` its collation
// and not `::JSONB`, Django's spelling, which compared the texts.
#[test]
fn jsonb_spelled_in_capitals_compares_as_jsonb() {
    let conn = connect(&start_server());
    for ty in ["jsonb", "JSONB"] {
        for (a, op, b) in [("[3]", "<", "[10]"), (r#"{"b": 1}"#, "<", r#"{"a": 1, "b": 2}"#)] {
            let sql = format!("SELECT CASE WHEN '{a}'::{ty} {op} '{b}'::{ty} THEN 'yes' ELSE 'no' END");
            assert_eq!(scan_str(&conn, &sql), "yes", "{sql}");
        }
    }
}

// A statement the session expands before translating (`nextval('s')` into the next
// value) is labelled from the statement as sent, and
// the text passes' reading of it named `b LIKE 'x%'` after its column, and
// `0.1::numeric + 0.2::numeric` after the type. FigureColname says `?column?` for both.
#[test]
fn an_expanded_statement_is_labelled_as_postgres_labels_it() {
    let server = start_server();
    let setup = connect(&server);
    must_exec(&setup, "CREATE TABLE t (a int, b text)");
    must_exec(&setup, "INSERT INTO t VALUES (1, 'xy')");
    must_exec(&setup, "CREATE SEQUENCE s");
    for conn in [connect(&server), connect_extended(&server)] {
        for (sql, want) in [
            (
                "SELECT b LIKE 'x%', nextval('s'), a FROM t",
                vec!["?column?", "nextval", "a"],
            ),
            (
                "SELECT 0.1::numeric + 0.2::numeric, nextval('s')",
                vec!["?column?", "nextval"],
            ),
        ] {
            assert_eq!(names(&conn, sql), want, "{sql}");
        }
    }
}

// A row count is a bigint wherever the client puts it: the text reader found `LIMIT
// $1` and not `LIMIT ($1)` or `OFFSET ($2)`, which left the parameter unspecified, the
// type tokio-postgres goes to pg_type for. The tree has the count, parentheses or not.
#[test]
fn a_row_count_in_parentheses_is_a_bigint() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id text)");
    let param_oids = |sql: &str| -> Vec<u32> { conn.prepare("", sql).unwrap().param_oids };
    assert_eq!(param_oids("SELECT id FROM t LIMIT ($1) OFFSET ($2)"), [20, 20]);
    assert_eq!(param_oids("SELECT id FROM t WHERE id = $1 LIMIT ($2)"), [25, 20]);
}
