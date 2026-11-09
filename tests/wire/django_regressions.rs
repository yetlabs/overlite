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
