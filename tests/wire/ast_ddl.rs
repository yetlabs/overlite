//! Answers of DDL the AST path now translates (docs/adr/0001-sql-ast.md, stage 5b: the
//! utility statements on the tree): each failed before the stage, held to Postgres
//! 17's answer.

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

// `serial NOT NULL PRIMARY KEY`, as Django and Wagtail's first migrations write a
// key, is the rowid with AUTOINCREMENT as `serial PRIMARY KEY` is: the text pass gave
// it neither when NOT NULL stood between, and an INSERT without the key failed on it.
// Its values are never used again, as a sequence's are not.
#[test]
fn a_serial_key_with_constraints_between_numbers_its_rows() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE s1 (id serial NOT NULL PRIMARY KEY, v text)");
    must_exec(&conn, "INSERT INTO s1 (v) VALUES ('a'), ('b'), ('c')");
    must_exec(&conn, "DELETE FROM s1 WHERE id = 3");
    assert_eq!(seq_val(&conn, "INSERT INTO s1 (v) VALUES ('d') RETURNING id"), 4);
    assert_eq!(
        rows(&conn, "SELECT pg_get_serial_sequence('s1', 'id')"),
        ["public.s1_id_seq"]
    );
}

// A default that is an expression but no call is in the parentheses SQLite requires;
// the text pass wrapped calls alone, and `DEFAULT 1 + 2` was a syntax error.
#[test]
fn a_default_of_any_expression() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE d1 (a int DEFAULT 1 + 2, b text DEFAULT 'x' || 'y')",
    );
    must_exec(&conn, "INSERT INTO d1 DEFAULT VALUES");
    assert_eq!(rows(&conn, "SELECT a, b FROM d1"), ["3,xy"]);
}

// A column may be called tablespace: the text pass took the word for the clause and
// cut it and the type after it out of the statement.
#[test]
fn a_column_called_tablespace() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE ts1 (tablespace text, x int) TABLESPACE pg_default");
    must_exec(&conn, "INSERT INTO ts1 VALUES ('a', 1)");
    assert_eq!(rows(&conn, "SELECT * FROM ts1"), ["a,1"]);
}

// A TEMP table's columns are typed as any table's: the text passes looked for TABLE as
// the second word, and a numeric lost its scale (1.10 read back 1.1) and a citext its
// case-insensitive comparison. UNLOGGED is a table too: it reached SQLite, a syntax
// error.
#[test]
fn temp_and_unlogged_tables_type_their_columns() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE EXTENSION IF NOT EXISTS citext");
    must_exec(
        &conn,
        "CREATE TEMP TABLE tt (id serial PRIMARY KEY, n numeric(10,2), c citext)",
    );
    must_exec(&conn, "INSERT INTO tt (n, c) VALUES (1.10, 'Abc')");
    assert_eq!(rows(&conn, "SELECT n FROM tt"), ["1.10"]);
    assert_eq!(rows(&conn, "SELECT count(*) FROM tt WHERE c = 'abc'"), ["1"]);
    must_exec(
        &conn,
        "CREATE UNLOGGED TABLE ut (id serial PRIMARY KEY, n numeric(10,2))",
    );
    must_exec(&conn, "INSERT INTO ut (n) VALUES (1.10)");
    assert_eq!(rows(&conn, "SELECT id, n FROM ut"), ["1,1.10"]);
}

// A column ADD COLUMN brings in a rebuild of the table (a UNIQUE one, here) is typed
// as CREATE TABLE types it: the column's text was translated alone, which no pass
// keyed on CREATE or ALTER read, and a numeric lost its scale.
#[test]
fn a_column_added_by_a_rebuild_is_typed() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE n1 (id int PRIMARY KEY)");
    must_exec(&conn, "INSERT INTO n1 VALUES (1), (2)");
    must_exec(&conn, "ALTER TABLE n1 ADD COLUMN n numeric(10,2) UNIQUE");
    must_exec(
        &conn,
        "ALTER TABLE n1 ADD COLUMN m numeric(10,2) DEFAULT round(1.234, 2)",
    );
    must_exec(&conn, "UPDATE n1 SET n = 1.10 WHERE id = 1");
    must_exec(&conn, "UPDATE n1 SET n = 10 WHERE id = 2");
    assert_eq!(
        rows(&conn, "SELECT id, n, m FROM n1 ORDER BY n"),
        ["1,1.10,1.23", "2,10.00,1.23"]
    );
}
