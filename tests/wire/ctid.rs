use crate::common::*;

// Duplicates removed by ctid, as Evolution API's migrations do before adding
// a unique index; ctid did not exist ("no such column").
#[test]
fn ctid_is_the_row_identifier() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE "Contact" ("id" TEXT, "remoteJid" TEXT, "instanceId" TEXT)"#,
    );
    must_exec(
        &conn,
        r#"INSERT INTO "Contact" VALUES ('a', 'j1', 'i1'), ('b', 'j1', 'i1'), ('c', 'j2', 'i1'), ('d', 'j1', 'i2')"#,
    );

    must_exec(
        &conn,
        r#"DELETE FROM "Contact"
WHERE ctid NOT IN (
  SELECT min(ctid)
  FROM "Contact"
  GROUP BY "remoteJid", "instanceId"
)"#,
    );
    must_exec(
        &conn,
        r#"CREATE UNIQUE INDEX "Contact_remoteJid_instanceId_key" ON "Contact"("remoteJid", "instanceId")"#,
    );

    let rows = conn.query(r#"SELECT "id" FROM "Contact" ORDER BY "id""#, &[]).unwrap();
    let mut ids = Vec::new();
    for row in &rows {
        let id: String = row.scan().unwrap();
        ids.push(id);
    }
    assert_eq!(ids, ["a", "c", "d"], "the first of each duplicate stays");

    // Qualified, and a string that says ctid is still a string.
    let s: String = conn
        .query_row(
            r#"SELECT 'ctid' FROM "Contact" c WHERE c.ctid = (SELECT min(ctid) FROM "Contact")"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(s, "ctid");
}

// Only the word ctid is the system column; inside a quoted name it is that name.
#[test]
fn ctid_inside_a_quoted_name() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE "a ctid" ("my ctid" text, "x.ctid" text, "ctid b" text, "ctid" text)"#,
    );
    assert_eq!(
        query_column(&conn, r#"SELECT name FROM pragma_table_info('a ctid') ORDER BY cid"#, 0),
        ["my ctid", "x.ctid", "ctid b", "ctid"]
    );
    must_exec(&conn, r#"INSERT INTO "a ctid" VALUES ('1', '2', '3', '4')"#);
    let row: (String, String, String, String) = conn
        .query_row(r#"SELECT "my ctid", "x.ctid", t."ctid b", "ctid" FROM "a ctid" t"#, &[])
        .scan()
        .unwrap();
    assert_eq!(row, ("1".into(), "2".into(), "3".into(), "4".into()));

    // A name with an escaped quote does not end early, and the real ctid still works.
    must_exec(&conn, r#"CREATE TABLE q ("a"" ctid" text)"#);
    assert_eq!(
        query_column(&conn, "SELECT name FROM pragma_table_info('q')", 0),
        [r#"a" ctid"#]
    );
    let n: i64 = conn
        .query_row(r#"SELECT count(*) FROM "a ctid" WHERE ctid > 0"#, &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);
}

fn label(rows: &Rows) -> Vec<String> {
    rows.fields.iter().map(|f| f.name.clone()).collect()
}

// A client reads the column by the name it asked for.
#[test]
fn ctid_result_labels() {
    for conn in [connect(&start_server()), connect_extended(&start_server())] {
        must_exec(&conn, "CREATE TABLE t (a int, b text)");
        must_exec(&conn, "INSERT INTO t VALUES (2, 'x'), (1, 'y')");
        // An alias of the rowid: SQLite labels it with the column's own name.
        must_exec(&conn, "CREATE TABLE s (id serial PRIMARY KEY, a int)");
        must_exec(&conn, "INSERT INTO s (a) VALUES (1)");

        for (sql, want) in [
            ("SELECT ctid FROM t", vec!["ctid"]),
            ("SELECT CTID FROM t", vec!["ctid"]),
            ("SELECT t.ctid FROM t", vec!["ctid"]),
            ("SELECT ctid FROM s", vec!["ctid"]),
            ("SELECT DISTINCT ctid FROM t", vec!["ctid"]),
            ("SELECT a, ctid, b FROM t", vec!["a", "ctid", "b"]),
            ("SELECT a AS ctid FROM t", vec!["ctid"]),
            ("SELECT a ctid FROM t", vec!["ctid"]),
            ("SELECT a + 1 AS ctid, b FROM t", vec!["ctid", "b"]),
            ("SELECT ctid AS r FROM t", vec!["r"]),
            ("SELECT ctid FROM t UNION ALL SELECT ctid FROM s", vec!["ctid"]),
            ("WITH c AS (SELECT ctid FROM t) SELECT ctid FROM c", vec!["ctid"]),
            ("SELECT x.ctid FROM (SELECT ctid, a FROM t) x", vec!["ctid"]),
            ("SELECT x.ctid FROM (SELECT a AS ctid FROM t) x", vec!["ctid"]),
            ("UPDATE t SET a = a WHERE ctid = 1 RETURNING ctid", vec!["ctid"]),
            (
                "UPDATE t SET a = a WHERE ctid = 1 RETURNING a AS ctid, b",
                vec!["ctid", "b"],
            ),
        ] {
            assert_eq!(label(&conn.query(sql, &[]).unwrap()), want, "{sql}");
        }

        let ids = |conn: &Conn, sql: &str| -> Vec<i64> {
            let rows = conn.query(sql, &[]).unwrap();
            rows.iter().map(|r| r.scan().unwrap()).collect()
        };
        // It is still the row identifier, and what an alias says still wins in ORDER BY.
        assert_eq!(ids(&conn, "SELECT ctid FROM t ORDER BY ctid DESC"), [2, 1]);
        assert_eq!(ids(&conn, "SELECT a AS ctid FROM t ORDER BY ctid"), [1, 2]);
        assert_eq!(ids(&conn, "SELECT a AS ctid FROM t WHERE ctid = 1"), [2]);
        assert_eq!(ids(&conn, "SELECT a + ctid FROM t ORDER BY 1"), [3, 3]);
        assert_eq!(
            ids(&conn, "SELECT ctid FROM t UNION SELECT ctid FROM s ORDER BY ctid"),
            [1, 2]
        );
    }
}
