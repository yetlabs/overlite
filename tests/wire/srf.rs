use crate::common::*;

// Set-returning calls in a select list run in lockstep, one row per element,
// over stored arrays and over the catalog's vectors alike.
#[test]
fn select_list_srfs() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int PRIMARY KEY, a text[], b text[])");
    must_exec(&conn, "INSERT INTO t VALUES (1, ARRAY['x','y'], ARRAY['p','q'])");

    assert_eq!(
        query_column(
            &conn,
            "SELECT generate_subscripts(a, 1) || ':' || unnest(a) || ':' || unnest(b) FROM t",
            0
        ),
        ["1:x:p", "2:y:q"]
    );

    // int2vector subscripts start at 0, as Postgres numbers them.
    must_exec(&conn, "CREATE TABLE parent (x int, y int, PRIMARY KEY (x, y))");
    assert_eq!(
        query_column(
            &conn,
            "SELECT i || ':' || k FROM (
		SELECT unnest(indkey) AS k, generate_subscripts(indkey, 1) AS i
		FROM pg_index WHERE indrelid = (SELECT oid FROM pg_class WHERE relname = 'parent') ORDER BY i) s",
            0
        ),
        ["0:1", "1:2"]
    );
}

// A derived table takes a column list, as a table function does.
#[test]
fn derived_table_column_lists() {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    must_exec(&conn, "CREATE TABLE t (id int, val int)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 10), (2, 20)");
    for (sql, want) in [
        (
            "SELECT b FROM (VALUES (1, 2), (3, 4)) AS v(a, b) ORDER BY b",
            vec!["2", "4"],
        ),
        ("SELECT v.a FROM (VALUES (1, 2)) v(a, b)", vec!["1"]),
        ("SELECT column2 FROM (VALUES (1, 2)) AS v(a)", vec!["2"]),
        ("SELECT a FROM (SELECT 1 AS x, 2 AS y) AS v(a, b)", vec!["1"]),
        ("SELECT y FROM (SELECT 1 AS x, 2 AS y) AS v(a)", vec!["2"]),
        (r#"SELECT "A b" FROM (SELECT 1) AS v("A b")"#, vec!["1"]),
        ("SELECT a FROM (VALUES (1), (2)) AS v(a) WHERE a > 1", vec!["2"]),
        (
            "SELECT u.b FROM (VALUES (1)) AS t(a) JOIN (VALUES (1, 2)) AS u(a, b) ON t.a = u.a",
            vec!["2"],
        ),
        (
            "SELECT a + b FROM (VALUES (1)) AS t(a), (VALUES (2)) AS u(b)",
            vec!["3"],
        ),
        (
            "WITH c AS (SELECT * FROM (VALUES (1, 2)) AS v(a, b)) SELECT b FROM c",
            vec!["2"],
        ),
        (
            "SELECT a FROM (SELECT id, val FROM t ORDER BY id LIMIT 1) AS s(a, b)",
            vec!["1"],
        ),
        (
            "SELECT m.label FROM t JOIN (VALUES (1, 'one')) AS m(k, label) ON t.id = m.k",
            vec!["one"],
        ),
        (
            "SELECT a FROM (SELECT x AS a FROM (VALUES (1)) AS i(x)) AS o(a)",
            vec!["1"],
        ),
        (
            "SELECT a FROM (SELECT val FROM t ORDER BY id) AS t(a) LIMIT 1",
            vec!["10"],
        ),
    ] {
        for c in [&conn, &ext] {
            let rows = c.query_formats(sql, &[TEXT_FORMAT], &[]).unwrap();
            let got: Vec<String> = rows.iter().map(|r| r.cell(0).text()).collect();
            assert_eq!(got, want, "{sql}");
        }
    }

    // Names: * gives the aliases, and what follows the list.
    let rows = conn
        .query("SELECT * FROM (VALUES (1, 'a')) AS v(id, name)", &[])
        .unwrap();
    let names: Vec<&str> = rows.field_descriptions().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["id", "name"]);
    let rows = conn
        .query("SELECT * FROM (SELECT 1 AS x, 2 AS y) AS v(a)", &[])
        .unwrap();
    let names: Vec<&str> = rows.field_descriptions().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["a", "y"]);

    // Writes read one as well.
    must_exec(
        &conn,
        "UPDATE t SET val = v.x FROM (VALUES (1, 100)) AS v(id, x) WHERE t.id = v.id",
    );
    assert_eq!(scan_str(&conn, "SELECT val::text FROM t WHERE id = 1"), "100");
    must_exec(&conn, "INSERT INTO t SELECT a, b FROM (VALUES (3, 30)) AS v(a, b)");
    assert_eq!(scan_str(&conn, "SELECT count(*)::text FROM t"), "3");

    // The string in a VALUES stays what it says.
    assert_eq!(
        scan_str(&conn, "SELECT a FROM (VALUES (') AS v(a)')) AS v(a)"),
        ") AS v(a)"
    );

    // Too many names is 42P10, and the first column names no other column.
    for sql in [
        "SELECT * FROM (VALUES (1, 2)) AS v(a, b, c)",
        "SELECT * FROM (SELECT 1 AS x) AS v(a, b)",
    ] {
        assert_sqlstate(&conn.exec(sql, &[]).unwrap_err(), "42P10");
    }
    assert_sqlstate(
        &conn
            .exec("SELECT x FROM (SELECT 1 AS x, 2 AS y) AS v(a)", &[])
            .unwrap_err(),
        "42703",
    );
    assert_sqlstate(
        &conn.exec("SELECT b FROM (VALUES (1, 2)) AS v(a)", &[]).unwrap_err(),
        "42703",
    );
    // A star in a source hides what a shorter list leaves: an explicit no.
    assert_sqlstate(
        &conn.exec("SELECT * FROM (SELECT * FROM t) AS v(a)", &[]).unwrap_err(),
        "0A000",
    );
}

fn set_setup() -> (Server, Conn, Conn) {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    must_exec(&conn, "CREATE TABLE t (id int, a int[], j jsonb, s text)");
    must_exec(
        &conn,
        r#"INSERT INTO t VALUES (1, '{1,2}', '[1,"x",[2]]', 'a,b'), (2, '{3}', '[]', 'c'), (3, NULL, NULL, NULL)"#,
    );
    must_exec(&conn, "CREATE TABLE o (id int, d jsonb)");
    must_exec(
        &conn,
        r#"INSERT INTO o VALUES (1, '{"a":1,"b":"x"}'), (2, '{"c":null}')"#,
    );
    (server, conn, ext)
}

/// Every row as its cells in text joined with `|`, a NULL empty.
fn rows_of(c: &Conn, sql: &str) -> Vec<String> {
    let rows = c
        .query_formats(sql, &[TEXT_FORMAT], &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let width = rows.field_descriptions().len();
    rows.iter()
        .map(|r| {
            let cells: Vec<String> = (0..width)
                .map(|i| {
                    if r.cell(i).is_null() {
                        String::new()
                    } else {
                        r.cell(i).text()
                    }
                })
                .collect();
            cells.join("|")
        })
        .collect()
}

// Each row is what Postgres 17 answered. These answered nothing at all, without a word:
// a set read from a column of the table to its left.
#[test]
fn sets_read_from_a_column() {
    let (_server, conn, ext) = set_setup();
    for (sql, want) in [
        (
            "SELECT id, x FROM t, unnest(t.a) AS u(x) ORDER BY id, x",
            vec!["1|1", "1|2", "2|3"],
        ),
        (
            "SELECT id, u FROM t, unnest(a) u ORDER BY id, u",
            vec!["1|1", "1|2", "2|3"],
        ),
        (
            "SELECT id, u.x FROM t CROSS JOIN LATERAL unnest(t.a) AS u(x) ORDER BY id, x",
            vec!["1|1", "1|2", "2|3"],
        ),
        (
            "SELECT id, u.x FROM t LEFT JOIN LATERAL unnest(t.a) AS u(x) ON true ORDER BY id, x",
            vec!["1|1", "1|2", "2|3", "3|"],
        ),
        (
            "SELECT id, x, o FROM t, unnest(t.a) WITH ORDINALITY AS u(x, o) ORDER BY 1, 3",
            vec!["1|1|1", "1|2|2", "2|3|1"],
        ),
        ("SELECT count(*) FROM t, unnest(a)", vec!["3"]),
        ("SELECT id, unnest(a) FROM t ORDER BY 1, 2", vec!["1|1", "1|2", "2|3"]),
        ("SELECT unnest(a) FROM t WHERE id = 1 ORDER BY 1", vec!["1", "2"]),
        (
            "SELECT id, e FROM t, jsonb_array_elements(t.j) AS e WHERE id = 1 ORDER BY e",
            vec!["1|\"x\"", "1|1", "1|[2]"],
        ),
        (
            "SELECT id, e.value FROM t, jsonb_array_elements(t.j) e WHERE id = 1 ORDER BY 2 LIMIT 1",
            vec!["1|\"x\""],
        ),
        (
            "SELECT id, e FROM t CROSS JOIN LATERAL jsonb_array_elements_text(t.j) AS e WHERE id = 1 ORDER BY 2",
            vec!["1|1", "1|[2]", "1|x"],
        ),
        (
            "SELECT id, p FROM t, regexp_split_to_table(t.s, ',') AS p ORDER BY id, p",
            vec!["1|a", "1|b", "2|c"],
        ),
        (
            "SELECT id, regexp_split_to_table(s, ',') FROM t ORDER BY 1, 2",
            vec!["1|a", "1|b", "2|c"],
        ),
        (
            "SELECT id, key, value FROM o, jsonb_each(o.d) ORDER BY id, key",
            vec!["1|a|1", "1|b|\"x\"", "2|c|null"],
        ),
        (
            "SELECT id, key, value FROM o, jsonb_each_text(o.d) ORDER BY id, key",
            vec!["1|a|1", "1|b|x", "2|c|"],
        ),
        (
            "SELECT id, k, v FROM o, jsonb_each(o.d) AS e(k, v) ORDER BY id, k",
            vec!["1|a|1", "1|b|\"x\"", "2|c|null"],
        ),
        (
            "SELECT id, e.key FROM o CROSS JOIN LATERAL json_each(o.d) e ORDER BY 1, 2",
            vec!["1|a", "1|b", "2|c"],
        ),
        (
            "SELECT id, k FROM o, jsonb_object_keys(o.d) AS k ORDER BY 1, 2",
            vec!["1|a", "1|b", "2|c"],
        ),
    ] {
        assert_eq!(rows_of(&conn, sql), want, "simple: {sql}");
        assert_eq!(rows_of(&ext, sql), want, "extended: {sql}");
    }
}

#[test]
fn sets_of_values() {
    let (_server, conn, ext) = set_setup();
    for (sql, want) in [
        ("SELECT unnest(ARRAY[1,2])", vec!["1", "2"]),
        ("SELECT unnest(ARRAY[1,2]) AS x", vec!["1", "2"]),
        ("SELECT unnest(ARRAY['a','b']), 1", vec!["a|1", "b|1"]),
        ("SELECT unnest('{1,2}'::int[])", vec!["1", "2"]),
        (
            "SELECT unnest(ARRAY[1,2]), unnest(ARRAY[3,4,5])",
            vec!["1|3", "2|4", "|5"],
        ),
        ("SELECT 1 + unnest(ARRAY[1,2])", vec!["2", "3"]),
        ("SELECT generate_series(1,3)", vec!["1", "2", "3"]),
        ("SELECT generate_series(1,2), 'x'", vec!["1|x", "2|x"]),
        ("SELECT * FROM unnest(ARRAY[1,2])", vec!["1", "2"]),
        ("SELECT * FROM unnest(ARRAY[1,2]) AS u(x)", vec!["1", "2"]),
        (
            "SELECT * FROM unnest(ARRAY[1,2]) WITH ORDINALITY AS u(x, o)",
            vec!["1|1", "2|2"],
        ),
        (
            "SELECT * FROM unnest(ARRAY[1,2], ARRAY['a','b','c']) AS u(x, y)",
            vec!["1|a", "2|b", "|c"],
        ),
        (
            r#"SELECT jsonb_array_elements('[1,"a",{"b":2},null,[3]]'::jsonb)"#,
            vec!["1", "\"a\"", r#"{"b":2}"#, "null", "[3]"],
        ),
        (
            r#"SELECT * FROM jsonb_array_elements('[1,"a"]'::jsonb)"#,
            vec!["1", "\"a\""],
        ),
        (
            r#"SELECT * FROM jsonb_array_elements('[1,"a"]'::jsonb) AS e(v)"#,
            vec!["1", "\"a\""],
        ),
        (
            r#"SELECT v FROM jsonb_array_elements('[1,"a"]'::jsonb) AS v"#,
            vec!["1", "\"a\""],
        ),
        (
            r#"SELECT * FROM jsonb_array_elements_text('[1,"a",null]'::jsonb)"#,
            vec!["1", "a", ""],
        ),
        (
            r#"SELECT * FROM json_array_elements('[1,"a"]'::json)"#,
            vec!["1", "\"a\""],
        ),
        (r#"SELECT json_array_elements_text('["x",2]'::json)"#, vec!["x", "2"]),
        (
            "SELECT * FROM regexp_split_to_table('a,b;c', '[,;]')",
            vec!["a", "b", "c"],
        ),
        ("SELECT regexp_split_to_table('a b', ' ')", vec!["a", "b"]),
        (
            "SELECT * FROM regexp_split_to_table('a,b', ',') AS p(x)",
            vec!["a", "b"],
        ),
        (
            "SELECT * FROM regexp_split_to_table('aXbxc', 'x', 'i')",
            vec!["a", "b", "c"],
        ),
        ("SELECT regexp_split_to_table('abc', '')", vec!["a", "b", "c"]),
        ("SELECT * FROM regexp_split_to_table('', ',')", vec![""]),
        (
            r#"SELECT * FROM json_each('{"a":1,"b":"x"}'::json)"#,
            vec!["a|1", "b|\"x\""],
        ),
        (r#"SELECT * FROM jsonb_each('{"a":1}'::jsonb)"#, vec!["a|1"]),
        (
            r#"SELECT * FROM json_each_text('{"a":1,"b":"x"}'::json)"#,
            vec!["a|1", "b|x"],
        ),
        (
            r#"SELECT * FROM jsonb_each_text('{"a":1,"b":null}'::jsonb)"#,
            vec!["a|1", "b|"],
        ),
        (r#"SELECT * FROM jsonb_each('{"a":1}'::jsonb) AS e(k, v)"#, vec!["a|1"]),
        (
            r#"SELECT * FROM json_object_keys('{"a":1,"b":2}'::json)"#,
            vec!["a", "b"],
        ),
        (r#"SELECT jsonb_object_keys('{"a":1}'::jsonb)"#, vec!["a"]),
    ] {
        assert_eq!(rows_of(&conn, sql), want, "simple: {sql}");
        assert_eq!(rows_of(&ext, sql), want, "extended: {sql}");
    }
}

// A bound array is an array: it answered nothing.
#[test]
fn unnest_of_a_parameter() {
    let (_server, _conn, ext) = set_setup();
    let rows = ext.query("SELECT * FROM unnest($1::int[])", &[&"{1,2}"]).unwrap();
    assert_eq!(rows.len(), 2);
    let rows = ext
        .query("SELECT * FROM unnest($1::text[]) AS u(x)", &[&"{a,b,c}"])
        .unwrap();
    assert_eq!(rows.len(), 3);
}

#[test]
fn sets_that_fail() {
    let (_server, conn, _) = set_setup();
    for (sql, code) in [
        (r#"SELECT jsonb_array_elements('{"a":1}'::jsonb)"#, "22023"),
        ("SELECT jsonb_array_elements('1'::jsonb)", "22023"),
        ("SELECT * FROM json_each('[1]'::json)", "22023"),
        ("SELECT * FROM json_object_keys('1'::json)", "22023"),
        ("SELECT * FROM regexp_split_to_table('a', '(')", "2201B"),
        (r#"SELECT json_each('{"a":1}'::json)"#, "0A000"),
    ] {
        let got = conn.exec(sql, &[]);
        assert_eq!(
            got.as_ref().map_err(WireError::code).err(),
            Some(code),
            "{sql}: {got:?}"
        );
    }
}
