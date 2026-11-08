use crate::common::*;

// sql_functions: LANGUAGE sql functions are executed (by inlining) — scalar
// and set-returning, named and positional params, nested, replaceable, drop.
#[test]
fn sql_functions() {
    let conn = connect(&start_server());

    let exec = |sql: &str| must_exec(&conn, sql);
    let scalar = |sql: &str| -> i64 { conn.query_row(sql, &[]).scan().unwrap() };

    // Named parameters.
    exec("CREATE FUNCTION add2(a int, b int) RETURNS int AS $$ SELECT a + b $$ LANGUAGE sql");
    assert_eq!(scalar("SELECT add2(3, 4)"), 7);

    // Positional parameters ($1, $2).
    exec("CREATE FUNCTION mul(x int, y int) RETURNS int AS $$ SELECT $1 * $2 $$ LANGUAGE sql");
    assert_eq!(scalar("SELECT mul(6, 7)"), 42);

    // A function reading a table, used in an expression and a predicate.
    exec("CREATE TABLE emp (id int, sal int)");
    exec("INSERT INTO emp VALUES (1, 100), (2, 300), (3, 200)");
    exec("CREATE FUNCTION top_sal() RETURNS int AS $$ SELECT max(sal) FROM emp $$ LANGUAGE sql");
    assert_eq!(scalar("SELECT top_sal()"), 300);
    assert_eq!(scalar("SELECT id FROM emp WHERE sal = top_sal()"), 2);

    // Nested calls fully inline.
    exec("CREATE FUNCTION inc(x int) RETURNS int AS $$ SELECT x + 1 $$ LANGUAGE sql");
    exec("CREATE FUNCTION inc2(x int) RETURNS int AS $$ SELECT inc(inc(x)) $$ LANGUAGE sql");
    assert_eq!(scalar("SELECT inc2(10)"), 12);

    // CREATE OR REPLACE swaps the body.
    exec("CREATE OR REPLACE FUNCTION add2(a int, b int) RETURNS int AS $$ SELECT a * b $$ LANGUAGE sql");
    assert_eq!(scalar("SELECT add2(3, 4)"), 12);

    // Set-returning function used in FROM.
    exec(
        "CREATE FUNCTION emps_over(m int) RETURNS TABLE(id int, sal int)
	      AS $$ SELECT id, sal FROM emp WHERE sal > m ORDER BY id $$ LANGUAGE sql",
    );
    let rows = conn
        .query("SELECT id, sal FROM emps_over(150) x ORDER BY id", &[])
        .unwrap();
    let mut got = Vec::new();
    for row in &rows {
        let (id, sal): (i64, i64) = row.scan().unwrap();
        got.push([id, sal]);
    }
    assert_eq!(got, [[2, 300], [3, 200]]);

    // DROP removes it.
    conn.exec("DROP FUNCTION mul(int, int)", &[]).unwrap();
    conn.exec("SELECT mul(1, 2)", &[]).unwrap_err();

    // A non-sql language is still accepted (no-op), not executed.
    conn.exec(
        "CREATE FUNCTION pl() RETURNS int AS $$ BEGIN RETURN 1; END $$ LANGUAGE plpgsql",
        &[],
    )
    .unwrap();
}

// A column-alias list on a function in FROM names the function's columns, and
// SQLite has no such syntax. Answers checked against PostgreSQL 17.
#[test]
fn generate_series_column_alias() {
    let conn = connect(&start_server());
    let ints = |sql: &str| -> Vec<String> {
        let rows = conn.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
        rows.iter().map(|r| r.cell(0).text()).collect()
    };

    must_exec(&conn, "CREATE TABLE t (i int)");
    must_exec(&conn, "INSERT INTO t SELECT i FROM generate_series(1, 100000) AS s(i)");
    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 100000);

    assert_eq!(ints("SELECT * FROM generate_series(1, 2) AS s(i)"), ["1", "2"]);
    assert_eq!(ints("SELECT * FROM generate_series(1, 2) s(i)"), ["1", "2"]);
    assert_eq!(
        ints("SELECT s.i FROM generate_series(1, 3) AS s(i) WHERE s.i > 1"),
        ["2", "3"]
    );
    assert_eq!(
        ints("SELECT s.\"I\" FROM generate_series(1, 2) AS s(\"I\")"),
        ["1", "2"]
    );
    assert_eq!(
        ints("SELECT \"S\".i FROM generate_series(1, 2) AS \"S\"(i)"),
        ["1", "2"]
    );
    assert_eq!(ints("SELECT s FROM generate_series(1, 2) AS s"), ["1", "2"]);
    assert_eq!(ints("SELECT generate_series FROM generate_series(1, 2)"), ["1", "2"]);

    let rows = conn.query("SELECT * FROM generate_series(1, 2) AS s(i)", &[]).unwrap();
    assert_eq!(rows.field_descriptions()[0].name, "i");

    // WITH ORDINALITY adds a column, which the list names too.
    let pairs = |sql: &str| -> Vec<(i64, i64)> {
        let rows = conn.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
        rows.collect_rows().unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    let want = [(10, 1), (20, 2)];
    assert_eq!(pairs("SELECT * FROM generate_series(10, 20, 10) WITH ORDINALITY"), want);
    assert_eq!(
        pairs("SELECT a, b FROM generate_series(10, 20, 10) WITH ORDINALITY AS s(a, b)"),
        want
    );
    assert_eq!(
        pairs("SELECT a, ordinality FROM generate_series(10, 20, 10) WITH ORDINALITY AS s(a)"),
        want
    );
}

#[test]
fn set_returning_column_alias() {
    let conn = connect(&start_server());
    let ints = |sql: &str| -> Vec<String> {
        let rows = conn.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
        rows.iter().map(|r| r.cell(0).text()).collect()
    };
    let pairs = |sql: &str| -> Vec<(i64, i64)> {
        let rows = conn.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
        rows.collect_rows().unwrap_or_else(|e| panic!("{sql}: {e}"))
    };

    assert_eq!(ints("SELECT x FROM unnest(ARRAY[1, 2]) AS u(x)"), ["1", "2"]);
    assert_eq!(ints("SELECT x FROM unnest('{1,2}'::int[]) AS u(x)"), ["1", "2"]);
    assert_eq!(
        ints("SELECT \"X\" FROM unnest(ARRAY[1, 2]) AS \"U\"(\"X\")"),
        ["1", "2"]
    );
    assert_eq!(ints("SELECT unnest FROM unnest(ARRAY[1, 2]) WHERE unnest > 1"), ["2"]);
    assert_eq!(
        pairs("SELECT x, o FROM unnest(ARRAY[5, 6]) WITH ORDINALITY AS u(x, o)"),
        [(5, 1), (6, 2)]
    );
    assert_eq!(
        ints("SELECT i FROM generate_subscripts(ARRAY[5, 6], 1) AS g(i)"),
        ["1", "2"]
    );

    // The value of a member is JSON, and text to a client, as Postgres gives it.
    let rows: Vec<(String, String)> = conn
        .query("SELECT k, v FROM json_each('{\"a\":1}') AS e(k, v)", &[])
        .unwrap()
        .collect_rows()
        .unwrap();
    assert_eq!(rows, [("a".to_string(), "1".to_string())]);
    let rows: Vec<(String, String)> = conn
        .query("SELECT k, v FROM jsonb_each('{\"a\":1}') e(k, v)", &[])
        .unwrap()
        .collect_rows()
        .unwrap();
    assert_eq!(rows, [("a".to_string(), "1".to_string())]);

    // No list: the function's own column names still answer.
    let value: String = conn
        .query_row("SELECT value FROM json_each('{\"a\":1}') AS e", &[])
        .scan()
        .unwrap();
    assert_eq!(value, "1");
}
