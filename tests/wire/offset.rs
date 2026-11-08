use crate::common::*;

// Prisma ends nearly every read with a bare `OFFSET $n` (no LIMIT), which
// Postgres accepts and SQLite rejects as a syntax error.

#[test]
fn offset_without_limit() {
    for (name, conn) in [
        ("simple", connect(&start_server())),
        ("extended", connect_extended(&start_server())),
    ] {
        println!("{name}");
        must_exec(&conn, r#"CREATE TABLE "User" (id int PRIMARY KEY, role text)"#);
        must_exec(
            &conn,
            r#"INSERT INTO "User" VALUES (1, 'a'), (2, 'a'), (3, 'b'), (4, 'a')"#,
        );

        let ids = |sql: &str, args: &[&dyn ToArg]| -> Vec<i64> {
            let rows = conn.query(sql, args).expect(sql);
            rows.collect_rows::<i64>().expect(sql)
        };

        assert_eq!(
            ids(
                r#"SELECT "id" FROM "User" WHERE "role" IN ($1) ORDER BY "id" OFFSET $2"#,
                &[&"a", &1i64]
            ),
            [2, 4]
        );
        assert_eq!(
            ids(
                r#"SELECT COUNT(*) FROM (SELECT "id" FROM "User" WHERE 1=1 OFFSET $1) AS "sub""#,
                &[&1i64]
            ),
            [3]
        );
        assert_eq!(
            ids(r#"SELECT id FROM "User" ORDER BY id OFFSET 1 LIMIT 2"#, &[]),
            [2, 3]
        );
    }
}
