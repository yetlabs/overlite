use crate::common::*;

/// The column names of a statement's result.
fn names(conn: &Conn, sql: &str) -> Vec<String> {
    let r = conn
        .exec_params(sql, &[] as &[&str], &[], &[], &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.fields.iter().map(|f| f.name.clone()).collect()
}

// A column the statement does not name: Postgres calls it after the function, the
// column, the cast's type, the keyword, or `?column?`. SQLite called it by the text
// of the expression after its rewrite: `count(*)`, `datetime('now')` for now(),
// `'postgres'` for current_user. Each want is Postgres 17's.
#[test]
fn unnamed_columns_labelled_as_postgres() {
    let server = start_server();
    let setup = connect(&server);
    must_exec(&setup, "CREATE TABLE t (id bigserial PRIMARY KEY, a int, b text)");
    must_exec(&setup, "INSERT INTO t (a, b) VALUES (1, 'x')");
    must_exec(&setup, "CREATE SEQUENCE s");
    for conn in [connect(&server), connect_extended(&server)] {
        for (sql, want) in [
            ("SELECT count(*) FROM t", vec!["count"]),
            (
                "SELECT max(a), 1 + 1, t.b, b::text, '1'::int, now(), current_user, lower(b) FROM t",
                vec!["max", "?column?", "b", "b", "int4", "now", "current_user", "lower"],
            ),
            (
                "SELECT CASE WHEN a = 1 THEN 1 END, EXISTS (SELECT 1), coalesce(a, 0), a IS NULL, \
                 (SELECT b FROM t LIMIT 1), count(*)::int, trim(b) FROM t GROUP BY a, b",
                vec!["case", "exists", "coalesce", "?column?", "b", "count", "btrim"],
            ),
            ("SELECT nextval('s')", vec!["nextval"]),
            ("SELECT a, count(*) AS n, b x FROM t GROUP BY a, b", vec!["a", "n", "x"]),
            ("SELECT DISTINCT count(*) FROM t", vec!["count"]),
            (
                "INSERT INTO t (a) VALUES (2) RETURNING id, a + 1",
                vec!["id", "?column?"],
            ),
        ] {
            assert_eq!(names(&conn, sql), want, "{sql}");
        }
    }
}
