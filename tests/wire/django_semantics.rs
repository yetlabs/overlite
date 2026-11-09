//! Answers Django's test suite found wrong without an error, each held to what
//! Postgres 17 answers.

use crate::common::*;

/// The type OIDs and the text cells of one simple-protocol statement.
#[track_caller]
fn texts(conn: &Conn, sql: &str) -> (Vec<u32>, Vec<Vec<Option<String>>>) {
    let mut results = conn.pg_exec(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let r = results.remove(0);
    let oids = r.fields.iter().map(|f| f.data_type_oid).collect();
    let rows = r
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| c.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()))
                .collect()
        })
        .collect();
    (oids, rows)
}

/// The cells of the first row, NULL as "NULL".
#[track_caller]
fn row(conn: &Conn, sql: &str) -> Vec<String> {
    let (_, rows) = texts(conn, sql);
    rows.first()
        .unwrap_or_else(|| panic!("{sql}: no row"))
        .iter()
        .map(|c| c.clone().unwrap_or_else(|| "NULL".into()))
        .collect()
}

// A quoted column named like a type keeps its name: `"serial"` was read as the type
// and became a column called INTEGER, so Django's model with a field named serial lost
// it (and a second such column made the table fail).
#[test]
fn a_column_named_like_a_type_keeps_its_name() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(
        &conn,
        r#"CREATE TABLE rt (id serial PRIMARY KEY, "serial" integer NOT NULL, "bigserial" int, "smallserial" text)"#,
    );
    must_exec(
        &conn,
        r#"INSERT INTO rt ("serial", "bigserial", "smallserial") VALUES (7, 8, 'x')"#,
    );
    assert_eq!(
        row(&conn, r#"SELECT id, "serial", "bigserial", "smallserial" FROM rt"#),
        ["1", "7", "8", "x"]
    );
    // Unquoted, serial is a name too, and a type only where a type goes.
    must_exec(&conn, "CREATE TABLE rt2 (serial integer, n serial PRIMARY KEY)");
    must_exec(&conn, "INSERT INTO rt2 (serial) VALUES (5)");
    assert_eq!(row(&conn, "SELECT serial, n FROM rt2"), ["5", "1"]);
    must_exec(&conn, r#"ALTER TABLE rt RENAME COLUMN "serial" TO "sn""#);
    must_exec(&conn, r#"ALTER TABLE rt ADD COLUMN "serial" int"#);
    assert_eq!(row(&conn, r#"SELECT "sn", "serial" FROM rt"#), ["7", "NULL"]);
    // Every type word is a name where a name goes.
    must_exec(
        &conn,
        "CREATE TABLE rt3 (date date, timestamp timestamp, interval interval, text text, numeric numeric(4,1), decimal decimal, CHECK (numeric > 0))",
    );
    must_exec(
        &conn,
        "INSERT INTO rt3 VALUES ('2020-01-01', '2020-01-01 1:00', '1h', 't', 1.25, 2.5)",
    );
    assert_eq!(
        row(
            &conn,
            "SELECT date, timestamp, interval, text, numeric, decimal FROM rt3"
        ),
        ["2020-01-01", "2020-01-01 01:00:00", "01:00:00", "t", "1.3", "2.5"]
    );
}

// Django's bulk insert sends each column as an array literal and unnests them. A
// quoted element is read as array_in reads it: a jsonb array in an E'' string was
// split at its commas and quotes into nothing, and the INSERT stored no row at all;
// an array cast to a type of two words (`timestamp with time zone[]`, `double
// precision[]`) was taken for a cast of the whole array to that type.
#[test]
fn unnest_of_array_literals_inserts_every_row() {
    let server = start_server();
    for conn in [connect(&server), connect_extended(&server)] {
        must_exec(&conn, "DROP TABLE IF EXISTS re");
        must_exec(
            &conn,
            "CREATE TABLE re (id serial PRIMARY KEY, a text, n numeric(5,2), b boolean, u uuid, d date, ts timestamptz, j jsonb, f float8)",
        );
        let sql = r#"INSERT INTO re (a, n, b, u, d, ts, j, f) SELECT * FROM UNNEST(E'{"a,b","c\\"d",NULL,"NULL"," x "}'::text[], ('{1.005,2,NULL,-3.5,4}'::numeric[])::numeric(5,2)[], ('{t,f,NULL,yes,off}')::boolean[], ('{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11,NULL,NULL,NULL,NULL}')::uuid[], ('{2020-01-02,NULL,NULL,NULL,infinity}'::date[])::date[], ('{"2020-01-02 03:04:05.5",NULL,NULL,NULL,NULL}'::timestamp[])::timestamp with time zone[], (E'{"{\\"a\\": [1, \\"x,y\\"]}",NULL,"null","[]","\\"s\\""}'::JSONB[])::JSONB[], ('{NULL,-1.0,3,1e3,NaN}'::float8[])::double precision[]) RETURNING a, n, b, u, d, ts, j, f"#;
        // Parse, Bind and Execute, as psycopg sends a statement, take the same route.
        assert_eq!(conn.query(sql, &[]).unwrap().len(), 5);
        let (_, rows) = texts(&conn, sql);
        let got: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| c.clone().unwrap_or_else(|| "NULL".into())).collect())
            .collect();
        assert_eq!(
            got,
            [
                [
                    "a,b",
                    "1.01",
                    "t",
                    "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
                    "2020-01-02",
                    "2020-01-02 03:04:05.5+00",
                    r#"{"a": [1, "x,y"]}"#,
                    "NULL"
                ],
                ["c\"d", "2.00", "f", "NULL", "NULL", "NULL", "NULL", "-1"],
                ["NULL", "NULL", "NULL", "NULL", "NULL", "NULL", "null", "3"],
                ["NULL", "-3.50", "t", "NULL", "NULL", "NULL", "[]", "1000"],
                [" x ", "4.00", "f", "NULL", "infinity", "NULL", "\"s\"", "NaN"],
            ]
        );
        // What the element type does not read is refused, not dropped.
        let err = conn.exec("SELECT * FROM UNNEST('{1,x}'::int[])", &[]).unwrap_err();
        assert_pg_error(&err, "22P02", r#"invalid input syntax for type integer: "x""#);
        assert_eq!(
            row(&conn, r#"SELECT count(*) FROM UNNEST(E'{"a\\\\b","q\\""}'::text[])"#),
            ["2"]
        );
    }
}

// An array literal cast to an array is an array to the client, in text and binary,
// where it went out as its JSON text under text's OID.
#[test]
fn an_array_literal_goes_out_as_an_array() {
    let server = start_server();
    let conn = connect(&server);
    let (oids, rows) = texts(
        &conn,
        r#"SELECT '{awef}'::text[], '{1,2}'::int[], '{"a b",NULL}'::varchar[]"#,
    );
    assert_eq!(oids, [1009, 1016, 1009]);
    assert_eq!(
        rows[0],
        [
            Some("{awef}".into()),
            Some("{1,2}".into()),
            Some(r#"{"a b",NULL}"#.into())
        ]
    );
    let r = conn
        .exec_params("SELECT '{awef}'::text[]", &[] as &[&[u8]], &[], &[], &[1])
        .unwrap();
    assert_eq!(r.fields[0].data_type_oid, 1009);
    // array_send: one dimension, no NULL, text elements, 1 element from 1, "awef".
    let want: Vec<u8> = [
        &1u32.to_be_bytes()[..],
        &0u32.to_be_bytes(),
        &25u32.to_be_bytes(),
        &1u32.to_be_bytes(),
        &1u32.to_be_bytes(),
        &4u32.to_be_bytes(),
        b"awef",
    ]
    .concat();
    assert_eq!(r.cell(0, 0), want.as_slice());
}

// NULL is larger than every value to Postgres: last in ascending order, first in
// descending, unless NULLS FIRST or LAST says otherwise. SQLite has it the other way
// round, and Django's `order_by(F(x).desc())` met its NULLs last.
#[test]
fn nulls_sort_as_in_postgres() {
    let server = start_server();
    for conn in [connect(&server), connect_extended(&server)] {
        let col = |sql: &str| -> Vec<String> {
            texts(&conn, sql)
                .1
                .into_iter()
                .map(|r| r[0].clone().unwrap_or_else(|| "N".into()))
                .collect()
        };
        let v = "(VALUES (1), (NULL), (2)) v(x)";
        assert_eq!(col(&format!("SELECT x FROM {v} ORDER BY x")), ["1", "2", "N"]);
        assert_eq!(col(&format!("SELECT x FROM {v} ORDER BY x DESC")), ["N", "2", "1"]);
        assert_eq!(
            col(&format!("SELECT x FROM {v} ORDER BY x ASC NULLS FIRST")),
            ["N", "1", "2"]
        );
        assert_eq!(
            col(&format!("SELECT x FROM {v} ORDER BY x DESC NULLS LAST")),
            ["2", "1", "N"]
        );
        assert_eq!(
            col(&format!(
                "SELECT string_agg(coalesce(x::text, 'N'), ',' ORDER BY x DESC) FROM {v}"
            )),
            ["N,2,1"]
        );
        assert_eq!(
            col(&format!(
                "SELECT row_number() OVER (ORDER BY x) FROM {v} ORDER BY x DESC"
            )),
            ["3", "2", "1"]
        );
    }
}

// A primary key holds no NULL. SQLite fills in a rowid for the NULL of an INTEGER
// PRIMARY KEY and takes a NULL in any other key, so Django's required custom key was
// saved under an id it never gave. A serial or identity key still takes its next value
// when it is left out or DEFAULT.
#[test]
fn a_primary_key_refuses_null() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = |name: &str| format!("{name}_{i}");
        must_exec(
            &conn,
            &format!("CREATE TABLE {} (code integer NOT NULL PRIMARY KEY, n text)", t("rx")),
        );
        must_exec(
            &conn,
            &format!("CREATE TABLE {} (a int, b text, PRIMARY KEY (a, b))", t("rk")),
        );
        must_exec(
            &conn,
            &format!("CREATE TABLE {} (id serial PRIMARY KEY, n text)", t("rs")),
        );
        must_exec(
            &conn,
            &format!(
                "CREATE TABLE {} (id integer NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY, n text)",
                t("ri")
            ),
        );
        for (sql, column) in [
            (
                format!("INSERT INTO {} (code, n) VALUES (NULL, 'Tom') RETURNING code", t("rx")),
                "code",
            ),
            (format!("INSERT INTO {} (n) VALUES ('Tom')", t("rx")), "code"),
            (format!("INSERT INTO {} VALUES (1, NULL)", t("rk")), "b"),
            (format!("INSERT INTO {} (id, n) VALUES (NULL, 'a')", t("rs")), "id"),
            (format!("INSERT INTO {} (id, n) VALUES (NULL, 'a')", t("ri")), "id"),
            (format!("INSERT INTO {} (n) SELECT 'x'", t("rx")), "code"),
        ] {
            let err = conn.exec(&sql, &[]).expect_err(&sql);
            assert_pg_error(&err, "23502", &format!("null value in column \"{column}\""));
        }
        must_exec(&conn, &format!("INSERT INTO {} (code, n) VALUES (7, 'a')", t("rx")));
        let err = conn
            .exec(&format!("UPDATE {} SET code = NULL", t("rx")), &[])
            .unwrap_err();
        assert_sqlstate(&err, "23502");
        for (sql, id) in [
            (format!("INSERT INTO {} (n) VALUES ('a') RETURNING id", t("rs")), "1"),
            (
                format!("INSERT INTO {} (id, n) VALUES (DEFAULT, 'a') RETURNING id", t("rs")),
                "2",
            ),
            (format!("INSERT INTO {} (n) VALUES ('a') RETURNING id", t("ri")), "1"),
            (format!("INSERT INTO {} DEFAULT VALUES RETURNING id", t("ri")), "2"),
        ] {
            assert_eq!(row(&conn, &sql), [id], "{sql}");
        }
    }
}

// LIKE tells case apart and takes a backslash for its escape unless ESCAPE says
// otherwise, as Django's startswith, contains and their i-forms expect: SQLite's LIKE
// folded case and read `\_` as a backslash and any character.
#[test]
fn like_tells_case_apart_and_escapes_with_a_backslash() {
    let server = start_server();
    for conn in [connect(&server), connect_extended(&server)] {
        assert_eq!(
            row(
                &conn,
                r"SELECT 'Article_ x' LIKE E'Article\\_%', 'Article x' LIKE E'Article\\_%', 'a%b' LIKE E'a\\%b', 'axb' LIKE E'a\\%b'"
            ),
            ["t", "f", "t", "f"]
        );
        assert_eq!(
            row(
                &conn,
                r"SELECT 'ABC' LIKE 'abc', 'ABC' LIKE 'A%', 'a_b' ILIKE 'A\_B', 'axb' ILIKE 'A\_B', 'a*b' LIKE 'a*b', 'ab' LIKE 'a*b'"
            ),
            ["f", "t", "t", "f", "t", "f"]
        );
        assert_eq!(
            row(
                &conn,
                r"SELECT 'a%b' LIKE 'a#%b' ESCAPE '#', 'axb' LIKE 'a#%b' ESCAPE '#', 'a\b' LIKE 'a\b' ESCAPE '', 'ABC' NOT LIKE 'a%', NULL LIKE 'a' IS NULL"
            ),
            ["t", "f", "t", "t", "t"]
        );
        // Django's contains over a column: the pattern is an expression.
        assert_eq!(
            row(
                &conn,
                r"SELECT 'x_y%z' LIKE '%' || REPLACE(REPLACE(REPLACE('_y%', E'\\', E'\\\\'), E'%', E'\\%'), E'_', E'\\_') || '%', 'xAyz' LIKE '%' || REPLACE(REPLACE(REPLACE('_y%', E'\\', E'\\\\'), E'%', E'\\%'), E'_', E'\\_') || '%'"
            ),
            ["t", "f"]
        );
        let err = conn.exec(r"SELECT 'a' LIKE 'a\'", &[]).unwrap_err();
        assert_pg_error(&err, "22025", "LIKE pattern must not end with escape character");
    }
}

// jsonb as Django's JSONField queries it: `->` and `#>` give jsonb, compared and sorted
// as jsonb (`{"l":"m"}` equals `{"l": "m"}`, 14 is past 2), `->>` and `#>>` give text,
// `?|` and `?&` read an untyped array literal, and what builds jsonb nests and says so.
#[test]
fn jsonb_operators_compare_as_jsonb() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("rm_{i}");
        must_exec(&conn, &format!("CREATE TABLE {t} (id int, v jsonb)"));
        must_exec(
            &conn,
            &format!(
                r#"INSERT INTO {t} VALUES (1, '{{"k": {{"l": "m"}}, "d": ["e", {{"f": "g"}}], "c": 14, "ord": 2, "b": true, "s": "x", "f": 1.5}}'), (2, '[1, [2]]'), (3, '{{"c": 3, "s": "y", "ord": 10}}'), (4, 'null')"#
            ),
        );
        let ids = |cond: &str| -> Vec<String> {
            texts(&conn, &format!("SELECT id FROM {t} WHERE {cond} ORDER BY id"))
                .1
                .into_iter()
                .map(|r| r[0].clone().unwrap())
                .collect()
        };
        for (cond, want) in [
            (r#"(v #> '{k,l}') = '"m"'::jsonb"#, &["1"][..]),
            ("(v #> '{1,0}') = '2'::jsonb", &["2"]),
            (r#"(v -> 'k') = '{"l": "m"}'::jsonb"#, &["1"]),
            ("(v -> 'c') > '2'::jsonb", &["1", "3"]),
            ("(v -> 'c') < '14'::jsonb", &["3"]),
            ("v -> 'f' = '1.50'", &["1"]),
            ("v ?| '{c,zz}'", &["1", "3"]),
            ("v ?& '{k,c,zz}'", &[]),
            ("v ?& '{k,c}'", &["1"]),
            ("v ?| ARRAY['ord']", &["1", "3"]),
            (r#"v -> 'd' @> '["e"]'"#, &["1"]),
            ("v ->> 'b' = 'true'", &["1"]),
            ("v ->> 'c' = '14'", &["1"]),
            ("v #>> '{d,1,f}' = 'g'", &["1"]),
            (r#"v -> 's' IN ('"x"', '"y"')"#, &["1", "3"]),
        ] {
            assert_eq!(ids(cond), want, "{cond}");
        }
        let (_, rows) = texts(&conn, &format!("SELECT id FROM {t} ORDER BY v -> 'ord', id"));
        let order: Vec<String> = rows.into_iter().map(|r| r[0].clone().unwrap()).collect();
        assert_eq!(order, ["1", "3", "2", "4"]);
        let (oids, rows) = texts(
            &conn,
            r#"SELECT JSONB_BUILD_ARRAY(('Ivan')::varchar, (JSONB_BUILD_ARRAY(('iivanov')::varchar, 30))::jsonb, (JSONB_BUILD_ARRAY())::jsonb), JSONB_BUILD_OBJECT('k', ('{"a": 1}')::jsonb)"#,
        );
        assert_eq!(oids, [3802, 3802]);
        assert_eq!(rows[0][0].as_deref(), Some(r#"["Ivan",["iivanov",30],[]]"#));
        assert_eq!(rows[0][1].as_deref(), Some(r#"{"k":{"a": 1}}"#));
        let (oids, _) = texts(&conn, &format!("SELECT v -> 'k', v ->> 'k', v #> '{{d}}' FROM {t}"));
        assert_eq!(oids, [3802, 25, 3802]);
    }
}

// Dates, times, timestamps and intervals in expressions, as Django's DurationField,
// F() arithmetic, aggregates and date hierarchies use them. SQLite read a timestamp as
// the number of its year, so `s + d` was 2011 and `e <= s + interval` compared numbers;
// SUM over intervals was NULL; and an aggregate or DATE_TRUNC over a date column went
// out as text, so Django got a str where it wanted a date.
#[test]
fn date_and_interval_arithmetic_and_types() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("rg_{i}");
        must_exec(
            &conn,
            &format!(
                "CREATE TABLE {t} (id int, s timestamp, e timestamp, d interval, tm time, dt date, tz timestamptz)"
            ),
        );
        must_exec(
            &conn,
            &format!(
                "INSERT INTO {t} VALUES (1, '2010-06-25 12:15:30', '2010-06-26 12:15:31', '1 day 02:00', '12:30:15', '2010-06-25', '2010-06-25 12:15:30+00'), (2, '2010-06-25 12:15:30', '2010-06-25 12:15:30', '2 days', '11:15:00', '2010-06-27', '2010-06-25 10:00:00+00')"
            ),
        );
        let check = |sql: &str, want_oids: &[u32], want: &[&str]| {
            let (oids, rows) = texts(&conn, sql);
            let got: Vec<String> = rows[0]
                .iter()
                .map(|c| c.clone().unwrap_or_else(|| "NULL".into()))
                .collect();
            assert_eq!(got, want, "{sql}");
            assert_eq!(oids, want_oids, "{sql}");
        };
        check(
            &format!("SELECT SUM(d), AVG(d), d FROM {t} WHERE id = 1 GROUP BY d"),
            &[1186, 1186, 1186],
            &["1 day 02:00:00", "1 day 02:00:00", "1 day 02:00:00"],
        );
        check(
            &format!("SELECT SUM(d), AVG(d) FROM {t}"),
            &[1186, 1186],
            &["3 days 02:00:00", "1 day 13:00:00"],
        );
        check(
            &format!(
                "SELECT s + d, e - s, tm - '11:15:00'::time, s - d, dt + 1, dt - '2010-06-20'::date, dt + d, tz + d FROM {t} WHERE id = 1"
            ),
            &[1114, 1186, 1186, 1114, 1082, 23, 1114, 1184],
            &[
                "2010-06-26 14:15:30",
                "1 day 00:00:01",
                "01:15:15",
                "2010-06-24 10:15:30",
                "2010-06-26",
                "5",
                "2010-06-26 02:00:00",
                "2010-06-26 14:15:30+00",
            ],
        );
        check(
            &format!("SELECT count(*) FROM {t} WHERE e <= (s + '0:00:00'::interval)"),
            &[20],
            &["1"],
        );
        check(
            "SELECT STATEMENT_TIMESTAMP() + '2 days'::interval > STATEMENT_TIMESTAMP()",
            &[16],
            &["t"],
        );
        check(
            &format!("SELECT s + interval '1 hour', tm + interval '1 hour' FROM {t} WHERE id = 1"),
            &[1114, 1083],
            &["2010-06-25 13:15:30", "13:30:15"],
        );
        check(
            &format!("UPDATE {t} SET s = (s + '2:00:00'::interval) WHERE id = 1 RETURNING s"),
            &[1114],
            &["2010-06-25 14:15:30"],
        );
        check(
            &format!("SELECT MIN(dt), MAX(s), MIN(tm), MAX(tz), COALESCE(MIN(dt), '1970-01-01'::date) FROM {t}"),
            &[1082, 1114, 1083, 1184, 1082],
            &[
                "2010-06-25",
                "2010-06-25 14:15:30",
                "11:15:00",
                "2010-06-25 12:15:30+00",
                "2010-06-25",
            ],
        );
        check(
            &format!(
                "SELECT GREATEST(s, '2000-01-01'::timestamp), LEAST(dt, '2100-01-01'::date), LAST_VALUE(dt) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM {t} ORDER BY id"
            ),
            &[1114, 1082, 1082],
            &["2010-06-25 14:15:30", "2010-06-25", "2010-06-27"],
        );
        check(
            &format!(
                "SELECT DATE_TRUNC('month', s), DATE_TRUNC('year', dt), DATE_TRUNC('day', tz), DATE_TRUNC('hour', d) FROM {t} WHERE id = 1"
            ),
            &[1114, 1184, 1184, 1186],
            &[
                "2010-06-01 00:00:00",
                "2010-01-01 00:00:00+00",
                "2010-06-25 00:00:00+00",
                "1 day 02:00:00",
            ],
        );
    }
    // interval_send: microseconds, days, months.
    let conn = connect(&server);
    let r = conn
        .exec_params(
            "SELECT '1 mon 1 day 02:00:03.5'::interval",
            &[] as &[&[u8]],
            &[],
            &[],
            &[1],
        )
        .unwrap();
    assert_eq!(r.fields[0].data_type_oid, 1186);
    let want: Vec<u8> = [
        &7_203_500_000i64.to_be_bytes()[..],
        &1i32.to_be_bytes(),
        &1i32.to_be_bytes(),
    ]
    .concat();
    assert_eq!(r.cell(0, 0), want.as_slice());
}

// EXTRACT gives a numeric since Postgres 14, with a second's fraction and an interval's
// fields: it was date_part's float, an ISO year was the calendar year, and every field
// of an interval was NULL. Its value is a number to SQLite, so it goes out without the
// trailing zeros Postgres prints (57.321000); the value is the same.
#[test]
fn extract_gives_numeric_with_every_field() {
    let server = start_server();
    let conn = connect(&server);
    let (oids, rows) = texts(
        &conn,
        "SELECT EXTRACT(ISOYEAR FROM '2014-12-29'::date), EXTRACT(SECOND FROM '2015-06-15 23:30:57.321'::timestamp), EXTRACT(MILLISECONDS FROM '2015-06-15 23:30:57.321'::timestamp), EXTRACT(EPOCH FROM '-1 day -02:00:03.5'::interval), EXTRACT(HOUR FROM '30 hours'::interval), EXTRACT(SECOND FROM DATE_TRUNC('second', '1 day 00:00:02.5'::interval)), EXTRACT(DAY FROM '2015-06-15 23:30:50+00'::timestamptz AT TIME ZONE 'UTC'), date_part('second', '2015-06-15 23:30:57.321'::timestamp)",
    );
    assert_eq!(oids, [1700, 1700, 1700, 1700, 1700, 1700, 1700, 701]);
    let got: Vec<String> = rows[0].iter().map(|c| c.clone().unwrap()).collect();
    assert_eq!(got, ["2015", "57.321", "57321", "-93603.5", "30", "2", "15", "57.321"]);
    // A number to SQLite: Django filters on `EXTRACT(YEAR FROM x) = 2015`.
    assert_eq!(
        row(
            &conn,
            "SELECT count(*) FROM (SELECT 1) x WHERE EXTRACT(YEAR FROM '2015-06-15'::date) = 2015"
        ),
        ["1"]
    );
}

// now() and its kin are a timestamptz with a fraction, the same through a statement:
// they were SQLite's datetime('now'), text to the second, which Django's Now() could not
// tell apart from another call in the same second.
#[test]
fn now_is_a_timestamptz_with_a_fraction() {
    let server = start_server();
    for conn in [connect(&server), connect_extended(&server)] {
        let (oids, rows) = texts(
            &conn,
            "SELECT STATEMENT_TIMESTAMP(), now(), CURRENT_TIMESTAMP, clock_timestamp(), now() = statement_timestamp()",
        );
        assert_eq!(oids, [1184, 1184, 1184, 1184, 16]);
        let now = rows[0][0].clone().unwrap();
        // 2026-11-07 21:53:11.292+00: a date, a clock, a fraction unless it is zero, UTC.
        assert!(now.ends_with("+00") && now.len() >= 22, "{now}");
        assert_eq!(rows[0][4].as_deref(), Some("t"));
    }
}

// SET CONSTRAINTS ... IMMEDIATE checks the deferred foreign keys there, as Django's
// check_constraints() expects, and keeps checking after each statement until the
// transaction ends or they are deferred again; a violation still pending at COMMIT
// fails it. SET CONSTRAINTS was taken and did nothing.
#[test]
fn set_constraints_immediate_checks_deferred_foreign_keys() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let (p, c) = (format!("rbp_{i}"), format!("rbc_{i}"));
        must_exec(&conn, &format!("CREATE TABLE {p} (id int PRIMARY KEY)"));
        must_exec(
            &conn,
            &format!("CREATE TABLE {c} (id int, p int REFERENCES {p} (id) DEFERRABLE INITIALLY DEFERRED)"),
        );
        let fk = format!(r#"insert or update on table "{c}" violates foreign key constraint "{c}_p_fkey""#);
        must_exec(&conn, "BEGIN");
        must_exec(&conn, &format!("INSERT INTO {c} VALUES (1, 30)"));
        let err = conn.exec("SET CONSTRAINTS ALL IMMEDIATE", &[]).unwrap_err();
        assert_pg_error(&err, "23503", &fk);
        must_exec(&conn, "ROLLBACK");
        // Named, and resolved before the check.
        must_exec(&conn, "BEGIN");
        must_exec(&conn, &format!("INSERT INTO {c} VALUES (1, 30)"));
        let err = conn
            .exec(&format!("SET CONSTRAINTS {c}_p_fkey IMMEDIATE"), &[])
            .unwrap_err();
        assert_sqlstate(&err, "23503");
        must_exec(&conn, "ROLLBACK");
        must_exec(&conn, "BEGIN");
        must_exec(&conn, &format!("INSERT INTO {c} VALUES (1, 30)"));
        must_exec(&conn, &format!("INSERT INTO {p} VALUES (30)"));
        let tag = conn.exec("SET CONSTRAINTS ALL IMMEDIATE", &[]).unwrap();
        assert_eq!(tag.as_str(), "SET CONSTRAINTS");
        must_exec(&conn, "SET CONSTRAINTS ALL DEFERRED");
        must_exec(&conn, "COMMIT");
        // IMMEDIATE holds for what follows.
        must_exec(&conn, "BEGIN");
        must_exec(&conn, "SET CONSTRAINTS ALL IMMEDIATE");
        let err = conn.exec(&format!("INSERT INTO {c} VALUES (2, 31)"), &[]).unwrap_err();
        assert_pg_error(&err, "23503", &fk);
        must_exec(&conn, "ROLLBACK");
        // Pending at COMMIT.
        must_exec(&conn, "BEGIN");
        must_exec(&conn, &format!("INSERT INTO {c} VALUES (3, 32)"));
        let err = conn.exec("COMMIT", &[]).unwrap_err();
        assert_sqlstate(&err, "23503");
        assert_eq!(row(&conn, &format!("SELECT count(*) FROM {c}")), ["1"]);
    }
}

// numeric arithmetic is exact in what a select list sends: SQLite read a numeric's text
// as a double, so `m - 0.04` was 0.060000000000000005 and ABS a float. ROUND to tens
// rounds, and RANDOM() is a double in [0, 1), where SQLite's is any 64-bit integer.
// Each result has the scale Postgres gives it: a quotient's 20 places here.
#[test]
fn numeric_arithmetic_is_exact() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("rr_{i}");
        must_exec(
            &conn,
            &format!("CREATE TABLE {t} (n numeric(20, 2), m numeric(40, 30))"),
        );
        must_exec(&conn, &format!("INSERT INTO {t} VALUES (-0.80, 0.1)"));
        let (oids, rows) = texts(
            &conn,
            &format!("SELECT ABS(n), n / 10, m - 0.04, n * 3, ROUND(n, 1), SUM(n) FROM {t} GROUP BY n, m"),
        );
        assert_eq!(oids, [1700, 1700, 1700, 1700, 1700, 1700]);
        let got: Vec<String> = rows[0].iter().map(|c| c.clone().unwrap()).collect();
        assert_eq!(
            got,
            [
                "0.80",
                "-0.08000000000000000000",
                "0.060000000000000000000000000000",
                "-2.40",
                "-0.8",
                "-0.80"
            ]
        );
        assert_eq!(
            row(
                &conn,
                "SELECT ROUND(365, -1), ROUND(365.123::numeric, -1), (0.5 - 0.4)::numeric, 0.1::numeric + 0.2::numeric"
            ),
            ["370", "370", "0.1", "0.3"]
        );
        let (oids, rows) = texts(&conn, "SELECT RANDOM() < 1, RANDOM() >= 0, RANDOM()");
        assert_eq!(oids[2], 701);
        assert_eq!(rows[0][..2], [Some("t".to_string()), Some("t".to_string())]);
        // ABS and ROUND compare as numbers where they are not sent.
        assert_eq!(
            row(
                &conn,
                &format!("SELECT count(*) FROM {t} WHERE ABS(n) > 0.5 AND ROUND(n, 1) < 0")
            ),
            ["1"]
        );
        // In parentheses, as Django writes an annotation.
        assert_eq!(texts(&conn, &format!("SELECT (n * 1.1) FROM {t}")).0, [1700]);
        let err = conn.exec(&format!("SELECT n / 0.0 FROM {t}"), &[]).unwrap_err();
        assert_sqlstate(&err, "22012");
    }
}

// Django's dates() and datetimes(): DISTINCT DATE_TRUNC, over a zone's wall clock too,
// went out as text.
#[test]
fn distinct_date_trunc_says_its_type() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE ra (d date, tz timestamptz)");
    must_exec(&conn, "INSERT INTO ra VALUES ('2010-06-25', '2010-06-25 12:15:30+00')");
    let (oids, rows) = texts(
        &conn,
        r#"SELECT DISTINCT DATE_TRUNC('year', "ra"."d") AS "datefield" FROM "ra" WHERE "ra"."d" IS NOT NULL ORDER BY 1 ASC"#,
    );
    assert_eq!(
        (oids, rows[0][0].as_deref()),
        (vec![1184], Some("2010-01-01 00:00:00+00"))
    );
    let (oids, rows) = texts(
        &conn,
        r#"SELECT DISTINCT DATE_TRUNC('second', "ra"."tz" AT TIME ZONE 'America/Chicago') AS "datetimefield" FROM "ra" ORDER BY 1 ASC LIMIT 1"#,
    );
    assert_eq!((oids, rows[0][0].as_deref()), (vec![1114], Some("2010-06-25 07:15:30")));
}
