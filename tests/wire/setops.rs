use crate::common::*;

// A member of a UNION, INTERSECT or EXCEPT in parentheses, with an ORDER BY and LIMIT of
// its own, as Postgres allows and Django writes for a union of querysets (each member
// parenthesised) and for a count over one. SQLite's grammar takes neither. Expected rows
// are Postgres 17's.

fn rows_simple(c: &Conn, sql: &str) -> Vec<String> {
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

fn rows_extended(c: &Conn, sql: &str) -> Vec<String> {
    let r = c
        .exec_params(sql, &[] as &[&str], &[], &[], &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    (0..r.rows.len())
        .map(|i| (0..r.fields.len()).map(|j| r.text(i, j)).collect::<Vec<_>>().join("|"))
        .collect()
}

#[test]
fn parenthesised_members_of_a_set_operation() {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    must_exec(
        &conn,
        "CREATE TABLE r20a (id int, ts int); CREATE TABLE r20b (id int, ts int)",
    );
    must_exec(
        &conn,
        "INSERT INTO r20a VALUES (1, 10), (4, 40); INSERT INTO r20b VALUES (2, 20), (3, 30)",
    );
    let cases: &[(&str, &[&str])] = &[
        (
            "SELECT COUNT(*) FROM ((SELECT id AS pk, ts, 0 AS idx FROM r20a) UNION ALL (SELECT id AS pk, ts, 1 AS idx FROM r20b WHERE id > 2)) subquery",
            &["3"],
        ),
        (
            "(SELECT id FROM r20a) UNION ALL (SELECT id FROM r20b) ORDER BY 1 LIMIT 2",
            &["1", "2"],
        ),
        (
            "(SELECT id FROM r20a ORDER BY id DESC LIMIT 1) UNION ALL (SELECT id FROM r20b ORDER BY id LIMIT 1) ORDER BY id",
            &["2", "4"],
        ),
        (
            "SELECT id FROM r20a UNION (SELECT id FROM r20b ORDER BY ts DESC LIMIT 1) ORDER BY id",
            &["1", "3", "4"],
        ),
        (
            "((SELECT id FROM r20a) UNION (SELECT id FROM r20b)) EXCEPT (SELECT 3) ORDER BY 1",
            &["1", "2", "4"],
        ),
        ("(SELECT id, ts FROM r20a) INTERSECT (SELECT 4, 40)", &["4|40"]),
        ("(SELECT id FROM r20a ORDER BY id LIMIT 1)", &["1"]),
        (
            "SELECT * FROM r20a WHERE id IN ((SELECT 1) UNION (SELECT 4)) ORDER BY id",
            &["1|10", "4|40"],
        ),
        (
            "WITH u AS ((SELECT id FROM r20a) UNION ALL (SELECT id FROM r20b)) SELECT sum(id) FROM u",
            &["10"],
        ),
        (
            "(SELECT id FROM r20a WHERE id = 1) UNION ALL (SELECT id FROM r20b WHERE ts > 25) ORDER BY id DESC",
            &["3", "1"],
        ),
    ];
    for (sql, want) in cases {
        assert_eq!(rows_simple(&conn, sql), *want, "simple: {sql}");
        assert_eq!(rows_extended(&ext, sql), *want, "extended: {sql}");
    }
    // The column keeps its name and type through the parentheses: those of the table's.
    let plain = ext
        .exec_params("SELECT id FROM r20a", &[] as &[&str], &[], &[], &[])
        .unwrap();
    let r = ext
        .exec_params(
            "(SELECT id FROM r20a) UNION ALL (SELECT id FROM r20b ORDER BY id LIMIT 1)",
            &[] as &[&str],
            &[],
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(r.fields[0].name, "id");
    assert_eq!(r.fields[0].data_type_oid, plain.fields[0].data_type_oid);
}
