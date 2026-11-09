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
