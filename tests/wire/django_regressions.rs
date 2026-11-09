//! What Django's test suite found wrong after the SQL layer moved to a parse tree,
//! each held to what Postgres 17 answers.

use crate::common::*;

/// The type OIDs and the text cells of one simple-protocol statement.
#[track_caller]
fn texts(conn: &Conn, sql: &str) -> (Vec<u32>, Vec<Vec<String>>) {
    let mut results = conn.pg_exec(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let r = results.remove(0);
    let oids = r.fields.iter().map(|f| f.data_type_oid).collect();
    let rows = r
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| {
                    c.as_ref()
                        .map_or_else(|| "NULL".into(), |b| String::from_utf8_lossy(b).into_owned())
                })
                .collect()
        })
        .collect();
    (oids, rows)
}

/// Every row, its cells joined by `|`.
#[track_caller]
fn rows(conn: &Conn, sql: &str) -> Vec<String> {
    texts(conn, sql).1.into_iter().map(|r| r.join("|")).collect()
}

/// The SQLSTATE sql fails with; it must fail.
#[track_caller]
fn fails(conn: &Conn, sql: &str) -> String {
    match conn.exec(sql, &[]) {
        Ok(_) => panic!("{sql}: succeeded"),
        Err(e) => e.code().to_string(),
    }
}

// A row compared with a row, Django's composite primary keys and ForeignObject, was
// refused as an expression the translation does not have.
#[test]
fn rows_compare_as_postgres_compares_them() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE r (a int, b int, c text)");
    must_exec(
        &conn,
        "INSERT INTO r VALUES (1, 2, 'x'), (1, 3, NULL), (2, 1, 'y'), (NULL, 1, 'z')",
    );
    let q = |w: &str| rows(&conn, &format!("SELECT a, b FROM r WHERE {w} ORDER BY a, b"));
    assert_eq!(q("(a, b) = (1, 2)"), ["1|2"]);
    // Unequal where a pair of members is, though another is NULL.
    assert_eq!(q("(a, b) <> (1, 2)"), ["1|3", "2|1", "NULL|1"]);
    assert_eq!(q("(a, b) > (1, 2)"), ["1|3", "2|1"]);
    assert_eq!(q("(a, b) >= (1, 2)"), ["1|2", "1|3", "2|1"]);
    assert_eq!(q("(a, b) < (2, 1)"), ["1|2", "1|3"]);
    assert_eq!(q("(a, b) <= (2, 1)"), ["1|2", "1|3", "2|1"]);
    assert_eq!(q("(a, b) IN ((1, 2), (2, 1))"), ["1|2", "2|1"]);
    assert_eq!(q("(a, b) NOT IN ((1, 2), (2, 1))"), ["1|3"]);
    assert_eq!(q("(a, c) > (1, 'a')"), ["1|2", "2|1"]);
    assert_eq!(q("(a, b) >= (SELECT a, b FROM r WHERE c = 'x')"), ["1|2", "1|3", "2|1"]);
    // NULL where the pair that decides is NULL, as in Postgres.
    assert_eq!(
        rows(
            &conn,
            "SELECT (a, b) < (1, 5), (a, b) = (1, NULL), (a, b) <> (1, NULL) FROM r ORDER BY a, b"
        ),
        ["t|NULL|NULL", "t|NULL|NULL", "f|f|t", "NULL|NULL|NULL"]
    );
    // A long list nests no deeper than SQLite takes.
    let list: Vec<String> = (0..3000).map(|i| format!("({i}, {})", i + 1)).collect();
    assert_eq!(q(&format!("(a, b) IN ({})", list.join(", "))), ["1|2"]);
    must_exec(&conn, "UPDATE r SET b = 9 WHERE (a, b) = (2, 1)");
    must_exec(&conn, "DELETE FROM r WHERE (a, b) = (1, 3)");
    assert_eq!(q("true"), ["1|2", "2|9", "NULL|1"]);
    assert_eq!(fails(&conn, "SELECT 1 WHERE (1, 2) = (1, 2, 3)"), "42601");

    // Parameters meet the member they are compared with, as Django binds them.
    let conn = connect_extended(&server);
    must_exec(&conn, "CREATE TABLE rt (id int, created timestamp, code text)");
    must_exec(
        &conn,
        "INSERT INTO rt VALUES (1, '2020-01-01 10:00', 'a'), (2, '2020-01-02', 'b'), (3, '2020-01-02', 'c')",
    );
    let p = |sql: &str, args: &[&str]| -> Vec<String> {
        let r = conn
            .exec_params(sql, args, &[], &[], &[])
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        (0..r.rows.len()).map(|i| r.text(i, 0)).collect()
    };
    assert_eq!(p("SELECT id FROM rt WHERE (id, code) = ($1, $2)", &["2", "b"]), ["2"]);
    assert_eq!(
        p(
            "SELECT id FROM rt WHERE (created, id) < ($1, $2) ORDER BY id",
            &["2020-01-02 00:00:00", "3"]
        ),
        ["1", "2"]
    );
    assert_eq!(
        p(
            "SELECT id FROM rt WHERE (id, code) IN (($1, $2), ($3, $4)) ORDER BY id",
            &["1", "a", "3", "c"]
        ),
        ["1", "3"]
    );
}

// `(a, b) IN (SELECT ...)` was refused as a row IN.
#[test]
fn a_row_is_in_a_subquery() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE r2 (a int, b int)");
    must_exec(&conn, "INSERT INTO r2 VALUES (1, 2), (1, 3), (2, 2)");
    assert_eq!(
        rows(
            &conn,
            "SELECT a, b FROM r2 WHERE (a, b) IN (SELECT a, b FROM r2 WHERE b = 3)"
        ),
        ["1|3"]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT a, b FROM r2 WHERE (a, b) NOT IN (SELECT a, b FROM r2 WHERE b = 3) ORDER BY 1, 2"
        ),
        ["1|2", "2|2"]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT a, b FROM r2 x WHERE (b, a) = ANY (SELECT a + 1, b - 1 FROM r2) ORDER BY 1, 2"
        ),
        ["1|2", "1|3", "2|2"]
    );
}

// A timestamp meeting a timestamptz is a wall clock in the session's TimeZone wherever
// it meets one. In a session not in UTC, BETWEEN and IN read it as UTC, a timestamptz
// primary key (and the foreign key on it) stored it as UTC, ADD COLUMN filled the rows
// it found with the default read as UTC, a timestamp[] cast to timestamptz[] read its
// elements as UTC, a day added across a change of offset was 24 hours rather than a
// calendar day, and 9999-12-31 23:59:59 west of UTC was refused.
#[test]
fn a_timestamp_meets_a_timestamptz_in_the_session_zone() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "SET TIME ZONE 'America/Chicago'");
    must_exec(&conn, "CREATE TABLE ev (id int, d timestamptz)");
    must_exec(
        &conn,
        "INSERT INTO ev VALUES (1, '2008-01-01 00:00:00'::timestamp), (2, '2008-12-31 23:59:59.999999'::timestamp), \
         (3, '2007-12-19 22:25:00'::timestamp)",
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT id FROM ev WHERE d BETWEEN '2008-01-01 00:00:00'::timestamp AND '2008-12-31 23:59:59.999999'::timestamp ORDER BY 1"
        ),
        ["1", "2"]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT id FROM ev WHERE d IN ('2007-12-19 22:25:00'::timestamp, '2007-12-19 21:00:00'::timestamp)"
        ),
        ["3"]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT id FROM ev WHERE d NOT IN ('2007-12-19 22:25:00'::timestamp) ORDER BY 1"
        ),
        ["1", "2"]
    );

    must_exec(&conn, "CREATE TABLE pk (d timestamptz PRIMARY KEY)");
    must_exec(&conn, "CREATE TABLE fk (id int, d timestamptz REFERENCES pk (d))");
    must_exec(&conn, "INSERT INTO pk VALUES ('2006-06-16 10:42:37'::timestamp)");
    must_exec(&conn, "INSERT INTO fk VALUES (1, '2006-06-16 10:42:37'::timestamp)");
    assert_eq!(rows(&conn, "SELECT d::text FROM pk"), ["2006-06-16 10:42:37-05"]);
    assert_eq!(
        rows(&conn, "SELECT 1 FROM pk WHERE d = '2006-06-16 10:42:37'::timestamp"),
        ["1"]
    );
    assert_eq!(rows(&conn, "SELECT fk.id FROM pk JOIN fk ON pk.d = fk.d"), ["1"]);

    must_exec(&conn, "CREATE TABLE ad (id int)");
    must_exec(&conn, "INSERT INTO ad VALUES (1)");
    must_exec(
        &conn,
        "ALTER TABLE ad ADD COLUMN d timestamptz DEFAULT '2000-01-01 01:01:00'::timestamp NOT NULL",
    );
    must_exec(&conn, "INSERT INTO ad (id) VALUES (2)");
    assert_eq!(
        rows(&conn, "SELECT d::text FROM ad ORDER BY id"),
        ["2000-01-01 01:01:00-06", "2000-01-01 01:01:00-06"]
    );

    assert_eq!(
        rows(
            &conn,
            r#"SELECT x::text FROM unnest(('{"2026-11-09 16:37:53.197252",NULL}'::timestamp[])::timestamptz[]) x"#
        ),
        ["2026-11-09 16:37:53.197252-06", "NULL"]
    );

    must_exec(&conn, "CREATE TABLE dst (d timestamptz)");
    must_exec(&conn, "INSERT INTO dst VALUES ('2010-11-03 08:24:15'::timestamp)");
    must_exec(&conn, "UPDATE dst SET d = d + '90 days 0:00:00'::interval");
    assert_eq!(rows(&conn, "SELECT d::text FROM dst"), ["2011-02-01 08:24:15-06"]);
    assert_eq!(
        rows(&conn, "SELECT (d - '3 months 1 hour'::interval)::text FROM dst"),
        ["2010-11-01 07:24:15-05"]
    );

    must_exec(&conn, "CREATE TABLE far (k text, e timestamptz)");
    must_exec(&conn, "INSERT INTO far VALUES ('a', '9999-12-31 23:59:59'::timestamp)");
    assert_eq!(rows(&conn, "SELECT e::text FROM far"), ["9999-12-31 23:59:59-06"]);
    assert_eq!(texts(&conn, "SELECT e FROM far").1, [["9999-12-31 23:59:59-06"]]);
    assert_eq!(
        rows(&conn, "SELECT k FROM far WHERE e = '9999-12-31 23:59:59'::timestamp"),
        ["a"]
    );
}

// avg of integers over no rows, or only NULLs, divided by a count of 0: division by
// zero where Postgres answers NULL. avg and sum of a double went out as text then.
#[test]
fn avg_over_nothing_is_null() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE av (x int, s smallint, b bigint, f float8, r real, n numeric)",
    );
    let q = "SELECT avg(x), avg(s), avg(b), avg(f), avg(r), avg(n), sum(f) FROM av";
    let all_null = ["NULL|NULL|NULL|NULL|NULL|NULL|NULL"];
    let oids = [1700, 1700, 1700, 701, 701, 1700, 701];
    assert_eq!(texts(&conn, q).0, oids);
    assert_eq!(rows(&conn, q), all_null);
    assert_eq!(
        rows(
            &conn,
            "SELECT coalesce(avg(x), 0.5), avg(DISTINCT x) FROM av WHERE x > 100"
        ),
        ["0.5|NULL"]
    );
    must_exec(&conn, "INSERT INTO av VALUES (NULL, NULL, NULL, NULL, NULL, NULL)");
    assert_eq!(rows(&conn, q), all_null);
    must_exec(
        &conn,
        "INSERT INTO av VALUES (1, 1, 1, 1.5, 1.5, 2.5), (2, 2, 2, 2, 2, 3)",
    );
    assert_eq!(texts(&conn, q).0, oids);
    assert_eq!(
        rows(&conn, q),
        ["1.5000000000000000|1.5000000000000000|1.5000000000000000|1.75|1.75|2.7500000000000000|3.5"]
    );
    assert_eq!(
        rows(&conn, "SELECT x, avg(x) FROM av GROUP BY x ORDER BY x"),
        ["1|1.00000000000000000000", "2|2.0000000000000000", "NULL|NULL"]
    );
}

// -9223372036854775808, which the grammar leaves as text past int4, was written as a
// numeric negated, and SQLite made it a double: a bigint column read it back as a
// float8, and a bigserial key refused it as a datatype mismatch. A long number in a
// statement the fast path took went to SQLite as a double too.
#[test]
fn the_smallest_bigint_is_a_bigint() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE bi (id bigserial PRIMARY KEY, v bigint)");
    must_exec(&conn, "CREATE TABLE bs (v bigserial PRIMARY KEY)");
    let (oids, cells) = texts(&conn, "INSERT INTO bi (v) VALUES (-9223372036854775808) RETURNING v");
    assert_eq!(
        (oids, cells),
        (vec![20], vec![vec!["-9223372036854775808".to_string()]])
    );
    assert_eq!(
        rows(&conn, "INSERT INTO bs (v) VALUES (-9223372036854775808) RETURNING v"),
        ["-9223372036854775808"]
    );
    for w in ["v = -9223372036854775808", "v <= -9223372036854775808"] {
        let (oids, cells) = texts(&conn, &format!("SELECT v FROM bi WHERE {w}"));
        assert_eq!(
            (oids, cells),
            (vec![20], vec![vec!["-9223372036854775808".to_string()]])
        );
    }
    assert_eq!(
        rows(&conn, "SELECT v FROM bs WHERE v = -9223372036854775808"),
        ["-9223372036854775808"]
    );
    let (oids, cells) = texts(
        &conn,
        "SELECT -9223372036854775808, 9223372036854775807, 9223372036854775808, -9223372036854775809",
    );
    assert_eq!(oids, [20, 20, 1700, 1700]);
    assert_eq!(
        cells,
        [[
            "-9223372036854775808",
            "9223372036854775807",
            "9223372036854775808",
            "-9223372036854775809"
        ]]
    );
    must_exec(&conn, "CREATE TABLE nu (n numeric)");
    must_exec(
        &conn,
        "INSERT INTO nu VALUES (-9223372036854775809), (0.12345678901234567890)",
    );
    assert_eq!(
        rows(&conn, "SELECT n FROM nu WHERE n < 1 ORDER BY n"),
        ["-9223372036854775809", "0.12345678901234567890"]
    );
}

// A serial key, and an integer identity key (Django's AutoField), is SQLite's rowid,
// declared INTEGER whatever it was, and went out as an int8 where Postgres describes
// an int4: Django's introspection found it unlike the foreign keys to it.
#[test]
fn an_int4_key_is_described_as_one() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE s4 (id serial PRIMARY KEY, i integer, b bigint, bs bigserial)",
    );
    must_exec(&conn, "CREATE TABLE s8 (id bigserial PRIMARY KEY)");
    must_exec(
        &conn,
        r#"CREATE TABLE i4 ("id" integer NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY, f int REFERENCES s4 (id))"#,
    );
    must_exec(
        &conn,
        r#"CREATE TABLE i8 ("id" bigint NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY)"#,
    );
    for t in ["s4", "s8", "i4", "i8"] {
        must_exec(&conn, &format!("INSERT INTO {t} DEFAULT VALUES"));
    }
    assert_eq!(texts(&conn, "SELECT * FROM s4").0, [23, 23, 20, 20]);
    assert_eq!(texts(&conn, "SELECT id FROM s4 WHERE id = 1").0, [23]);
    assert_eq!(texts(&conn, "INSERT INTO s4 DEFAULT VALUES RETURNING id").0, [23]);
    assert_eq!(texts(&conn, "SELECT * FROM s8").0, [20]);
    assert_eq!(texts(&conn, "SELECT * FROM i4").0, [23, 23]);
    assert_eq!(texts(&conn, "SELECT id FROM i4 WHERE id = 1").0, [23]);
    assert_eq!(texts(&conn, "SELECT * FROM i8").0, [20]);
    assert_eq!(rows(&conn, "SELECT id FROM s4 ORDER BY id"), ["1", "2"]);
}

// lower and upper of the empty range are NULL; the overloaded lower read `empty` as
// text, so a filter on lower(r) >= 0 kept the empty range and a select sent `empty`.
// A bound of a date or timestamptz range went out as text.
#[test]
fn the_bounds_of_the_empty_range_are_null() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE rb (id int, r int4range, d daterange, t tstzrange)");
    must_exec(
        &conn,
        "INSERT INTO rb VALUES (1, '[0,10)', '[2020-01-01,2020-02-01)', '[2020-01-01 00:00+00,2020-01-02 00:00+00)'), \
         (2, '[-5,10)', NULL, NULL), (3, 'empty', 'empty', 'empty'), (4, NULL, NULL, NULL), (5, '(,5)', '[2020-01-01,)', '(,)')",
    );
    assert_eq!(rows(&conn, "SELECT id FROM rb WHERE lower(r) >= 0 ORDER BY 1"), ["1"]);
    let q = "SELECT lower(r), upper(r), lower(d), upper(d), lower(t), upper(t) FROM rb ORDER BY id";
    assert_eq!(texts(&conn, q).0, [23, 23, 1082, 1082, 1184, 1184]);
    assert_eq!(
        rows(&conn, q),
        [
            "0|10|2020-01-01|2020-02-01|2020-01-01 00:00:00+00|2020-01-02 00:00:00+00",
            "-5|10|NULL|NULL|NULL|NULL",
            "NULL|NULL|NULL|NULL|NULL|NULL",
            "NULL|NULL|NULL|NULL|NULL|NULL",
            "NULL|5|2020-01-01|NULL|NULL|NULL",
        ]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT lower('empty'::int4range) IS NULL, upper('[1,2]'::int4range), lower('ABC')"
        ),
        ["t|3|abc"]
    );
}

// A range went out as text (OID 25) and an array of them as an int8[], so psycopg
// handed Django a string, or failed to read the array. A tstzrange's bounds without
// an offset were read as UTC, and went out in UTC whatever the session's zone.
#[test]
fn a_range_is_a_range_type() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        "CREATE TABLE rt (r int4range, b int8range, n numrange, d daterange, s tsrange, t tstzrange, ra int4range[], tz timestamptz[])",
    );
    must_exec(
        &conn,
        r#"INSERT INTO rt VALUES ('[1,5)', '(,10]', '[1.5,2.25)', '[2020-01-01,)', '(2020-01-01 10:00,2020-01-02)', '["2020-01-01 00:00+00","2020-01-02 00:00+00")', '{"[1,2)",empty}', '{"2020-01-01 00:00+00"}')"#,
    );
    must_exec(&conn, "SET TIME ZONE 'America/Chicago'");
    let (oids, cells) = texts(&conn, "SELECT * FROM rt");
    assert_eq!(oids, [3904, 3926, 3906, 3912, 3908, 3910, 3905, 1185]);
    assert_eq!(
        cells,
        [[
            "[1,5)",
            "(,11)",
            "[1.5,2.25)",
            "[2020-01-01,)",
            r#"("2020-01-01 10:00:00","2020-01-02 00:00:00")"#,
            r#"["2019-12-31 18:00:00-06","2020-01-01 18:00:00-06")"#,
            r#"{"[1,2)",empty}"#,
            r#"{"2019-12-31 18:00:00-06"}"#,
        ]]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT t::text, int4range(1, 5), tstzrange('2020-01-01', '2020-01-02')::text FROM rt"
        ),
        [
            r#"["2019-12-31 18:00:00-06","2020-01-01 18:00:00-06")|[1,5)|["2020-01-01 00:00:00-06","2020-01-02 00:00:00-06")"#
        ]
    );
    assert_eq!(
        texts(&conn, "SELECT int4range(1, 5), 'empty'::daterange").0,
        [3904, 3912]
    );
    // A bound without an offset is a wall clock in the session's zone.
    must_exec(
        &conn,
        r#"INSERT INTO rt (t) VALUES ('["2016-01-01 00:00:00","2016-01-04 00:00:00")')"#,
    );
    assert_eq!(
        rows(&conn, "SELECT lower(t)::text FROM rt WHERE r IS NULL"),
        ["2016-01-01 00:00:00-06"]
    );

    // range_send: flags, then each finite bound's length and binary form.
    let r = conn
        .exec_params::<&[u8]>("SELECT r, b, t FROM rt WHERE r IS NOT NULL", &[], &[], &[], &[1])
        .unwrap();
    assert_eq!(
        r.rows[0][0].as_deref().unwrap(),
        [2, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 4, 0, 0, 0, 5]
    );
    assert_eq!(
        r.rows[0][1].as_deref().unwrap(),
        [0x08, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 11]
    );
    let t = r.rows[0][2].as_deref().unwrap();
    assert_eq!(t[0], 0x02);
    // 2020-01-01 00:00 UTC is 631152000 seconds after 2000-01-01.
    assert_eq!(
        &t[1..13],
        [0, 0, 0, 8]
            .iter()
            .chain(&(631_152_000_000_000i64).to_be_bytes())
            .copied()
            .collect::<Vec<u8>>()
    );
}

// jsonb_agg of a jsonb column held each element as a string of its text, so `#>` into
// it found nothing; over no rows it was `[]` where Postgres' is NULL, so COALESCE's
// default never applied, and the COALESCE went out as text.
#[test]
fn jsonb_agg_aggregates_json() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE ja (g int, s int, j jsonb, b boolean, i int)");
    must_exec(
        &conn,
        r#"INSERT INTO ja VALUES (1, 1, '{"sea_view": false}', true, 1), (2, 1, '{"a": true}', NULL, NULL), (2, 2, '{"sea_view": true}', false, 3)"#,
    );
    // Compared as jsonb: a jsonb's text is not spaced as Postgres spaces it.
    assert_eq!(
        rows(
            &conn,
            r#"SELECT g, jsonb_agg(j ORDER BY s DESC) = CASE g WHEN 1 THEN '[{"sea_view": false}]'::jsonb
               ELSE '[{"sea_view": true}, {"a": true}]'::jsonb END FROM ja GROUP BY g ORDER BY g"#
        ),
        ["1|t", "2|t"]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT g FROM ja GROUP BY g HAVING (jsonb_agg(j ORDER BY s DESC) #> '{0,sea_view}') = 'true'::jsonb"
        ),
        ["2"]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT jsonb_agg(b ORDER BY s, g) = '[true, null, false]'::jsonb, jsonb_agg(i) FILTER (WHERE i > 5) FROM ja"
        ),
        ["t|NULL"]
    );
    assert_eq!(
        rows(
            &conn,
            r#"SELECT jsonb_build_object('k', j) = '{"k": {"sea_view": false}}'::jsonb FROM ja WHERE g = 1"#
        ),
        ["t"]
    );
    must_exec(&conn, "CREATE TABLE je (x int)");
    let q = r#"SELECT jsonb_agg(x), coalesce(jsonb_agg(x), '["<empty>"]'::jsonb), json_agg(x) FROM je"#;
    assert_eq!(texts(&conn, q).0, [3802, 3802, 114]);
    assert_eq!(rows(&conn, q), [r#"NULL|["<empty>"]|NULL"#]);
}

// A subscript of a subscript compared an element of an int[][] stored as strings with
// a number, so `(g)[1][1] = 1` found nothing; a subscript of string_to_array, which
// gives Postgres' array text, read no JSON and was NULL; fewer subscripts than an
// array has dimensions gave an array where Postgres gives NULL.
#[test]
fn subscripts_read_elements() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE sb (id int, f int[], g int[][])");
    must_exec(
        &conn,
        "INSERT INTO sb VALUES (1, '{1,2}', '{{1,2},{3,4}}'), (2, '{3,1}', '{{5,6},{7,8}}'), (3, NULL, NULL)",
    );
    assert_eq!(rows(&conn, "SELECT id FROM sb WHERE (g)[1][1] = 1"), ["1"]);
    assert_eq!(rows(&conn, "SELECT id FROM sb WHERE g[2][2] > 5"), ["2"]);
    assert_eq!(
        rows(
            &conn,
            "SELECT id FROM sb WHERE (f)[1] = ((string_to_array('1;2', ';'))[1])::integer"
        ),
        ["1"]
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT g[1], f[0], f[3], g[1][5], (string_to_array('x;y', ';'))[2] FROM sb WHERE id = 1"
        ),
        ["NULL|NULL|NULL|NULL|y"]
    );
    assert_eq!(
        texts(&conn, "SELECT (g)[1][1] FROM sb WHERE id = 1"),
        (vec![23], vec![vec!["1".to_string()]])
    );
}

// `->` of a key holding a backslash or a quote, or opening with `$`, was SQLite's label,
// which matches no such key (or is a bad path); `?&` read `{nested,123,000}` as
// numbers, `000` no JSON at all, so every row had all the keys; DISTINCT ON over `#>`
// sent its item as text, in no order.
#[test]
fn jsonb_keys_are_any_text() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE jk (id int, v jsonb)");
    must_exec(
        &conn,
        r#"INSERT INTO jk VALUES (1, '{"back\\slash": "some value", "double\"": 1, "$d": 2}'),
           (2, '{"nested": {"a": 1}, "123": 1, "array": [{"789": 1}, 2], "000": 3}'),
           (3, '{"k": {"l": "m"}}'), (4, '{"k": {"l": null}}'), (5, NULL)"#,
    );
    let ids = |w: &str| rows(&conn, &format!("SELECT id FROM jk WHERE {w} ORDER BY id"));
    assert_eq!(ids(r#"(v -> E'back\\slash') = '"some value"'::jsonb"#), ["1"]);
    assert_eq!(ids(r#"(v ->> 'double"') = '1'"#), ["1"]);
    assert_eq!(ids("(v -> '$d') = '2'::jsonb"), ["1"]);
    assert_eq!(ids(r#"v ?| E'{"back\\\\slash",does_not_exist}'"#), ["1"]);
    assert_eq!(ids("v ?& '{nested,123,array,000}'"), ["2"]);
    assert_eq!(ids("(v #> '{array,0}') ? '789'"), ["2"]);
    let q = "SELECT DISTINCT ON ((v #> '{k,l}')) (v #> '{k,l}') AS x FROM jk";
    assert_eq!(
        texts(&conn, q),
        (
            vec![3802],
            vec![
                vec!["null".to_string()],
                vec![r#""m""#.to_string()],
                vec!["NULL".to_string()]
            ]
        )
    );
}

// A Describe of a portal bound for binary results said text for every column, so a
// client reading the description (psycopg's binary cursor) read binary rows as text.
#[test]
fn a_portal_is_described_in_its_formats() {
    let server = start_server();
    let conn = connect(&server);
    let r = conn
        .exec_params::<&[u8]>("SELECT 1::int4, 'x'::text", &[], &[], &[], &[1])
        .unwrap();
    assert_eq!(r.fields.iter().map(|f| f.format).collect::<Vec<_>>(), [1, 1]);
    assert_eq!(r.rows[0][0].as_deref().unwrap(), [0, 0, 0, 1]);
    let r = conn
        .exec_params::<&[u8]>("SELECT 1::int4, 'x'::text", &[], &[], &[], &[0, 1])
        .unwrap();
    assert_eq!(r.fields.iter().map(|f| f.format).collect::<Vec<_>>(), [0, 1]);
    let r = conn.exec_params::<&[u8]>("SELECT 1::int4", &[], &[], &[], &[]).unwrap();
    assert_eq!(r.fields[0].format, 0);
}
