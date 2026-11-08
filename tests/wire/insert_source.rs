use crate::common::*;

// The sources of an INSERT that Postgres takes and SQLite's grammar does not: one in
// parentheses, and a SELECT with a FROM before ON CONFLICT, where SQLite reads the ON
// as a join's. Django writes both: a parenthesised VALUES for Wagtail's search index
// upsert, and SELECT * FROM UNNEST(...) ON CONFLICT DO NOTHING for bulk_create with
// ignore_conflicts and for a many-to-many add. Expected rows are Postgres 17's.

/// Every row of the last result, its cells joined with `|`.
fn rows(c: &Conn, sql: &str) -> Vec<String> {
    let r = c.pg_exec(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let last = r.last().unwrap();
    (0..last.rows.len())
        .map(|i| {
            (0..last.fields.len())
                .map(|j| last.text(i, j))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[test]
fn a_parenthesised_values_source_upserts() {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    // varchar(50) makes the write go through the column fitting, int and text do not.
    must_exec(
        &conn,
        r#"CREATE TABLE r18 ("ct" integer NOT NULL, "oid" varchar(50) NOT NULL, "body" text NOT NULL, UNIQUE ("ct", "oid"))"#,
    );
    for sql in [
        "INSERT INTO r18 (ct, oid, body) (VALUES (29, '1', 'a'), (29, '2', 'b')) ON CONFLICT (ct, oid) DO UPDATE SET body = EXCLUDED.body",
        "INSERT INTO r18 (ct, oid, body) (VALUES (29, '1', 'a2')) ON CONFLICT (ct, oid) DO UPDATE SET body = EXCLUDED.body",
        "INSERT INTO r18 (ct, oid, body) VALUES (29, '3', 'c') ON CONFLICT (ct, oid) DO UPDATE SET body = EXCLUDED.body",
    ] {
        must_exec(&conn, sql);
    }
    assert_eq!(
        rows(&conn, "SELECT * FROM r18 ORDER BY oid"),
        ["29|1|a2", "29|2|b", "29|3|c"]
    );

    must_exec(&conn, "CREATE TABLE p (k int PRIMARY KEY, v text)");
    must_exec(&conn, "INSERT INTO p (k, v) (VALUES (1, 'x'), (2, 'y'))");
    must_exec(
        &conn,
        "INSERT INTO p (k, v) (VALUES (2, 'y2'), (3, 'z')) ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v || '!'",
    );
    must_exec(&conn, "INSERT INTO p (k, v) (SELECT k + 10, v FROM p WHERE k < 3)");
    // Without a column list, the parentheses hold the source.
    must_exec(&conn, "INSERT INTO p (VALUES (20, 'w'))");
    assert_eq!(
        rows(
            &conn,
            "INSERT INTO p (k, v) (VALUES (3, 'no')) ON CONFLICT DO NOTHING RETURNING k"
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        rows(
            &conn,
            "INSERT INTO p (k, v) (VALUES (4, 'r')) ON CONFLICT (k) DO UPDATE SET v = 'u' RETURNING k, v"
        ),
        ["4|r"]
    );
    // The extended protocol, with parameters.
    let r = ext
        .exec_params(
            "INSERT INTO p (k, v) (VALUES ($1, $2)) ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
            &["4", "p"],
            &[],
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(r.tag.as_str(), "INSERT 0 1");
    assert_eq!(
        rows(&conn, "SELECT * FROM p ORDER BY k"),
        ["1|x", "2|y2!", "3|z", "4|p", "11|x", "12|y2!", "20|w"]
    );
}

#[test]
fn a_select_from_source_takes_on_conflict() {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    must_exec(
        &conn,
        r#"CREATE TABLE r7 ("g" integer NOT NULL, "p" integer NOT NULL, UNIQUE (g, p))"#,
    );
    // Django's own statements.
    for sql in [
        r#"INSERT INTO "r7" ("g", "p") SELECT * FROM UNNEST(('{1,1,1}'::int2[])::integer[], ('{109,110,111}'::int2[])::integer[]) ON CONFLICT DO NOTHING"#,
        r#"INSERT INTO "r7" ("g", "p") SELECT * FROM UNNEST(('{1,2}'::int2[])::integer[], ('{109,5}'::int2[])::integer[]) ON CONFLICT DO NOTHING"#,
        "INSERT INTO r7 (g, p) SELECT g + 1, p FROM r7 ORDER BY p LIMIT 2 ON CONFLICT DO NOTHING",
        "INSERT INTO r7 (g, p) SELECT max(g), p FROM r7 GROUP BY p ON CONFLICT (g, p) DO NOTHING",
        "INSERT INTO r7 (g, p) SELECT a.g + 5, b.p FROM r7 a JOIN r7 b ON a.p = b.p WHERE a.g = 1 ON CONFLICT DO NOTHING",
    ] {
        must_exec(&conn, sql);
    }
    assert_eq!(
        rows(
            &conn,
            "INSERT INTO r7 (g, p) SELECT 9, x FROM generate_series(1, 2) AS x ON CONFLICT DO NOTHING RETURNING g, p"
        ),
        ["9|1", "9|2"]
    );
    let r = ext
        .exec_params(
            "INSERT INTO r7 (g, p) SELECT * FROM UNNEST($1::integer[], $2::integer[]) ON CONFLICT DO NOTHING",
            &["{9,10}", "{1,1}"],
            &[],
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(r.tag.as_str(), "INSERT 0 1");
    assert_eq!(
        rows(&conn, "SELECT * FROM r7 ORDER BY 1, 2"),
        [
            "1|109", "1|110", "1|111", "2|5", "2|109", "3|5", "6|109", "6|110", "6|111", "9|1", "9|2", "10|1"
        ]
    );
}
