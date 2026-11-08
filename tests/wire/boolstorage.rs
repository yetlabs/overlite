use crate::common::*;

/// A database as the Go server left it: it stored a boolean bound as text as that
/// text, so a BOOLEAN column holds 'true' and 'false' beside the 1 and 0 written
/// since. Built with SQLite alone, before any server opens it, which converts it to
/// the storage format: every reading below reads what that made of it.
fn mixed_db(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("test.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        r#"
        CREATE TABLE "m" ("id" INTEGER NOT NULL, "b" BOOLEAN, "grp" INTEGER NOT NULL,
            CONSTRAINT "m_pkey" PRIMARY KEY ("id"));
        CREATE INDEX "m_b_idx" ON "m"("b");
        INSERT INTO "m" VALUES (1, 'true', 1), (2, 't', 1), (3, 1, 1), (4, 'false', 2),
            (5, 'f', 2), (6, 0, 2), (7, NULL, 3), (8, 'TRUE', 3), (9, 'yes', 3);
        CREATE TABLE "p" ("id" INTEGER PRIMARY KEY);
        INSERT INTO "p" VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10);
        "#,
    )
    .unwrap();
    path
}

const TRUE_IDS: &str = "1,2,3,8,9";
const FALSE_IDS: &str = "4,5,6";

fn ids(conn: &Conn, sql: &str) -> String {
    let rows = conn.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let v: Vec<String> = rows.iter().map(|r| r.get::<i64>(0).unwrap().to_string()).collect();
    v.join(",")
}

fn ids_with(conn: &Conn, sql: &str, param: &str) -> String {
    let r = conn
        .exec_params(sql, &[param], &[], &[], &[])
        .unwrap_or_else(|e| panic!("{sql} with {param:?}: {e}"));
    (0..r.rows.len()).map(|i| r.text(i, 0)).collect::<Vec<_>>().join(",")
}

// Every boolean test reads 'true', 't', 'TRUE', 'yes' and 1 alike, and 'false', 'f'
// and 0, whatever wrote them: `= true` matched only the integers, `WHERE b` and
// `NOT b` read any text as false, and a text parameter matched only the text.
#[test]
fn booleans_stored_as_text_or_integers() {
    let dir = temp_dir();
    let server = start_server_on(mixed_db(dir.path()), 0);
    for conn in [connect(&server), connect_extended(&server)] {
        for (sql, want) in [
            ("SELECT id FROM m WHERE b ORDER BY id", TRUE_IDS),
            ("SELECT id FROM m WHERE NOT b ORDER BY id", FALSE_IDS),
            ("SELECT id FROM m WHERE b = true ORDER BY id", TRUE_IDS),
            ("SELECT id FROM m WHERE b = false ORDER BY id", FALSE_IDS),
            ("SELECT id FROM m WHERE true = b ORDER BY id", TRUE_IDS),
            ("SELECT id FROM m WHERE b <> true ORDER BY id", FALSE_IDS),
            ("SELECT id FROM m WHERE b != false ORDER BY id", TRUE_IDS),
            ("SELECT id FROM m WHERE b = 'true' ORDER BY id", TRUE_IDS),
            ("SELECT id FROM m WHERE b = 'f' ORDER BY id", FALSE_IDS),
            ("SELECT id FROM m WHERE b = 'false' ORDER BY id", FALSE_IDS),
            ("SELECT id FROM m WHERE b IS TRUE ORDER BY id", TRUE_IDS),
            ("SELECT id FROM m WHERE b IS FALSE ORDER BY id", FALSE_IDS),
            ("SELECT id FROM m WHERE b IS NOT TRUE ORDER BY id", "4,5,6,7"),
            ("SELECT id FROM m WHERE b IS NULL ORDER BY id", "7"),
            ("SELECT id FROM m WHERE b AND grp = 1 ORDER BY id", "1,2,3"),
            ("SELECT id FROM m WHERE b OR id = 4 ORDER BY id", "1,2,3,4,8,9"),
            ("SELECT id FROM m WHERE NOT (b = true) ORDER BY id", FALSE_IDS),
            ("SELECT id FROM m WHERE b IN (true) ORDER BY id", TRUE_IDS),
            ("SELECT id FROM m WHERE m.b = true ORDER BY id", TRUE_IDS),
            ("SELECT x.id FROM m AS x WHERE x.b ORDER BY x.id", TRUE_IDS),
            (
                "SELECT id FROM p WHERE EXISTS (SELECT 1 FROM m WHERE m.id = p.id AND m.b) ORDER BY id",
                TRUE_IDS,
            ),
            (
                "SELECT id FROM p WHERE id IN (SELECT id FROM m WHERE b = false) ORDER BY id",
                FALSE_IDS,
            ),
            (
                "SELECT p.id FROM p JOIN m ON m.id = p.id WHERE NOT m.b ORDER BY p.id",
                FALSE_IDS,
            ),
            (
                "WITH t AS (SELECT id, b FROM m) SELECT id FROM t WHERE b ORDER BY id",
                TRUE_IDS,
            ),
            (
                "SELECT id FROM m WHERE CASE WHEN b THEN 1 ELSE 0 END = 1 ORDER BY id",
                TRUE_IDS,
            ),
        ] {
            assert_eq!(ids(&conn, sql), want, "{sql}");
        }

        // Aggregates, grouping and output.
        let agg = conn
            .query(
                "SELECT grp, bool_and(b), bool_or(b), count(*) FILTER (WHERE b) FROM m GROUP BY grp ORDER BY grp",
                &[],
            )
            .unwrap();
        let got: Vec<(i64, Option<bool>, Option<bool>, i64)> = agg.collect_rows().unwrap();
        assert_eq!(
            got,
            [
                (1, Some(true), Some(true), 3),
                (2, Some(false), Some(false), 0),
                (3, Some(true), Some(true), 2)
            ]
        );
        let groups: Vec<(bool, i64)> = conn
            .query(
                "SELECT b, count(*) FROM m WHERE b IS NOT NULL GROUP BY b ORDER BY b",
                &[],
            )
            .unwrap()
            .collect_rows()
            .unwrap();
        assert_eq!(groups, [(false, 3), (true, 5)]);
        let out: Vec<Option<bool>> = conn
            .query("SELECT b FROM m ORDER BY id", &[])
            .unwrap()
            .iter()
            .map(|r| r.get::<Option<bool>>(0).unwrap())
            .collect();
        let t = Some(true);
        let f = Some(false);
        assert_eq!(out, [t, t, t, f, f, f, None, t, t]);
    }

    // Untyped text parameters, as node-postgres binds them, and Postgres' text form
    // of each value whatever the storage.
    let conn = connect_extended(&server);
    for (sql, param, want) in [
        ("SELECT id FROM m WHERE b = $1 ORDER BY id", "true", TRUE_IDS),
        ("SELECT id FROM m WHERE b = $1 ORDER BY id", "t", TRUE_IDS),
        ("SELECT id FROM m WHERE b = $1 ORDER BY id", "false", FALSE_IDS),
        ("SELECT id FROM m WHERE b <> $1 ORDER BY id", "true", FALSE_IDS),
        (
            "SELECT id FROM p WHERE id IN (SELECT id FROM m WHERE b = $1) ORDER BY id",
            "false",
            FALSE_IDS,
        ),
        (
            "SELECT id FROM p WHERE EXISTS (SELECT 1 FROM m WHERE m.id = p.id AND m.b = $1) ORDER BY id",
            "true",
            TRUE_IDS,
        ),
    ] {
        assert_eq!(ids_with(&conn, sql, param), want, "{sql} with {param}");
    }
    let r = conn
        .exec_params("SELECT b FROM m WHERE id = $1", &["1"], &[], &[], &[])
        .unwrap();
    assert_eq!(r.text(0, 0), "t");
    let r = conn
        .exec_params("SELECT b FROM m WHERE id = $1", &["4"], &[], &[], &[1])
        .unwrap();
    assert_eq!(r.cell(0, 0), [0u8], "binary false");

    // New writes store integers, a quoted spelling too.
    must_exec(&conn, "UPDATE m SET b = NOT b WHERE id = 1");
    must_exec(&conn, "INSERT INTO m VALUES (10, true, 4), (11, 'no', 4)");
    assert_eq!(ids(&conn, "SELECT id FROM m WHERE b ORDER BY id"), "2,3,8,9,10");
    drop(conn);
    drop(server);
    // The text the old server stored was converted when the file was first opened.
    let (_, texts) = offline_check(
        dir.path().join("test.db"),
        "SELECT count(*) FROM m WHERE typeof(b) NOT IN ('integer', 'null')",
    );
    assert_eq!(texts, 0);
}

// A string compared with a boolean column is a boolean, as in Postgres, on rows
// this server wrote as integers too: `b = 'false'` found none of them.
#[test]
fn a_string_compared_with_a_boolean_is_a_boolean() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE q (id int primary key, excluded boolean not null default false)",
    );
    must_exec(&conn, "INSERT INTO q VALUES (1, false), (2, true)");
    must_exec(&conn, "INSERT INTO q (id) VALUES (3)");
    assert_eq!(
        ids(&conn, "SELECT id FROM q WHERE excluded = 'false' ORDER BY id"),
        "1,3"
    );
    assert_eq!(ids(&conn, "SELECT id FROM q WHERE excluded = 't' ORDER BY id"), "2");
    assert_eq!(ids(&conn, "SELECT id FROM q WHERE 'no' = excluded ORDER BY id"), "1,3");
    assert_eq!(
        ids(&conn, "SELECT id FROM q WHERE excluded <> 'yes' ORDER BY id"),
        "1,3"
    );
    let err = conn
        .query("SELECT id FROM q WHERE excluded = 'maybe'", &[])
        .unwrap_err();
    assert_sqlstate(&err, "22P02");
}
