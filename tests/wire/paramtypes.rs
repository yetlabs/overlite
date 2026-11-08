use crate::common::*;

// tokio-postgres (Prisma's schema engine, sqlx, Diesel) sends every parameter
// in binary, by the type Describe gives it, and looks a type it does not know
// up in pg_type. Parameters went out unspecified, so the lookup's own
// parameter was unknown too, and it recursed until the client crashed. They
// are now typed by the column or catalog column they meet, and binary values
// are read back by that type.
#[test]
fn parameter_types() {
    let conn = connect_extended(&start_server());

    must_exec(
        &conn,
        "CREATE TABLE migrations (id varchar(36) PRIMARY KEY, steps integer NOT NULL DEFAULT 0, started_at timestamptz, note text)",
    );

    let param_oids = |sql: &str| -> Vec<u32> { conn.prepare("", sql).unwrap().param_oids };
    assert_eq!(
        param_oids(
            "SELECT t.typname, t.typtype, t.typelem, t.typbasetype, t.typrelid FROM pg_catalog.pg_type t WHERE t.oid = $1"
        ),
        [26]
    );
    assert_eq!(
        param_oids("SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname = $1)"),
        [19]
    );
    assert_eq!(
        param_oids("SELECT nspname FROM pg_namespace WHERE nspname = ANY ( $1 )"),
        [1003]
    );
    assert_eq!(
        param_oids(r#"INSERT INTO "migrations" ("id","started_at","note") VALUES ($1,$2,$3)"#),
        [1043, 1184, 25]
    );
    assert_eq!(
        param_oids(r#"UPDATE "migrations" SET "steps" = ("steps" + $1) WHERE "id" = $2"#),
        [20, 1043]
    );
    // A row count is a bigint: Prisma's findFirst ends in OFFSET $n, and an
    // unspecified type there sent tokio-postgres to pg_type for type 0.
    assert_eq!(
        param_oids(
            r#"SELECT "public"."migrations"."id" FROM "public"."migrations" WHERE "public"."migrations"."id" = $1 OFFSET $2"#
        ),
        [1043, 20]
    );
    assert_eq!(param_oids("SELECT id FROM migrations LIMIT $1 OFFSET $2"), [20, 20]);

    // Binary values, read by their type: a timestamptz is stored as bare UTC,
    // and a four-byte string stays a string rather than becoming an integer.
    let started = time_fixed(2026, 10, 18, 9, 30, 0, 123456000, -3 * 3600);
    conn.exec(
        r#"INSERT INTO "migrations" ("id","started_at","note") VALUES ($1,$2,$3)"#,
        &[&"abcd", &started, &"note"],
    )
    .unwrap();
    conn.exec(
        r#"UPDATE "migrations" SET "steps" = ("steps" + $1) WHERE "id" = $2"#,
        &[&2i64, &"abcd"],
    )
    .unwrap();

    let (id, steps, at): (String, i64, Time) = conn
        .query_row("SELECT id, steps, started_at FROM migrations", &[])
        .scan()
        .unwrap();
    assert_eq!(id, "abcd");
    assert_eq!(steps, 2);
    assert!(started == at, "started_at {at}");
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM migrations WHERE started_at = '2026-10-18 12:30:00.123456+00'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1, "stored as the same literal would be");

    // A type the client declares in Parse is kept, and its binary value read by it.
    let res = conn
        .exec_params("SELECT $1 || '!'", &[b"wxyz"], &[25], &[1], &[])
        .unwrap();
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.text(0, 0), "wxyz!");
}

// param_columns: the table and the columns a parameter meets are in the code, not
// in a literal.
#[test]
fn param_types_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (a int, b text)");
    must_exec(&conn, "CREATE TABLE u (a text)");
    let oids = |sql: &str| conn.prepare("", sql).unwrap().param_oids;

    assert_eq!(oids("SELECT ' FROM u ' AS l, b FROM t WHERE a = $1"), [20]);
    assert_eq!(oids("SELECT ' JOIN ' AS l, b FROM t WHERE a = $1"), [20]);
    assert_eq!(oids("UPDATE t SET b = ' a = $1' WHERE a = $1"), [20]);
    assert_eq!(oids("UPDATE t SET b = $1 WHERE b = 'x' AND a = 1"), [25]);
}

// A table outside public has column types like any other: Describe is how a client
// learns to send a timestamp in binary.
#[test]
fn parameter_types_outside_public() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE SCHEMA s1");
    must_exec(
        &conn,
        "CREATE TABLE s1.m (id varchar(36), steps integer, active boolean, at timestamptz, note text)",
    );

    let param_oids = |sql: &str| -> Vec<u32> { conn.prepare("", sql).unwrap().param_oids };
    assert_eq!(
        param_oids("INSERT INTO s1.m (id, active, at, note) VALUES ($1, $2, $3, $4)"),
        [1043, 16, 1184, 25]
    );
    assert_eq!(
        param_oids("INSERT INTO s1.m VALUES ($1, $2, $3, $4, $5)"),
        [1043, 20, 16, 1184, 25],
        "by position, which needs the column order"
    );
    assert_eq!(
        param_oids(r#"UPDATE "s1"."m" SET "active" = $1 WHERE "steps" = $2"#),
        [16, 20]
    );
    assert_eq!(param_oids("DELETE FROM s1.m WHERE at < $1"), [1184]);
    assert_eq!(param_oids("SELECT note FROM s1.m WHERE id = $1"), [1043]);

    // By the search_path, it is the table the path finds.
    must_exec(&conn, "CREATE TABLE m (id bigint, active text)");
    must_exec(&conn, "SET search_path = s1");
    assert_eq!(param_oids("UPDATE m SET active = $1"), [16]);
    must_exec(&conn, "SET search_path = public");
    assert_eq!(param_oids("UPDATE m SET active = $1"), [25]);

    // Values typed that way arrive as what they are, here a client's binary bool
    // and a timestamptz, and read back the same in both formats.
    let at = time_utc(2026, 10, 18, 9, 30, 0, 0);
    conn.exec(
        "INSERT INTO s1.m (id, steps, active, at) VALUES ($1, $2, $3, $4)",
        &[&"abcd", &7i64, &true, &at],
    )
    .unwrap();
    let (steps, active, read): (i64, bool, Time) = conn
        .query_row("SELECT steps, active, at FROM s1.m WHERE id = $1", &[&"abcd"])
        .scan()
        .unwrap();
    assert_eq!((steps, active, read), (7, true, at));
}

// Prisma's relation filters (`some: { excluded: false }`) put the boolean column
// inside a subquery, and node-postgres binds `false` as untyped text. The type was
// found for a one-table statement only, so the text reached SQLite as 'false' and
// matched no stored 0. A parameter takes the column it meets wherever it is: a
// subquery, EXISTS, IN, a join, a CTE, through an alias or a qualified name.
#[test]
fn parameter_types_in_subqueries() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE p2 (id int primary key)");
    must_exec(
        &conn,
        "CREATE TABLE t2 (id int primary key, p_id int, b boolean not null, note text)",
    );
    must_exec(&conn, "INSERT INTO p2 VALUES (1), (2)");
    must_exec(&conn, "INSERT INTO t2 VALUES (1, 1, false, 'a'), (2, 2, true, 'b')");

    for sql in [
        "SELECT id FROM t2 WHERE b = $1",
        "SELECT id FROM p2 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.p_id = p2.id AND t2.b = $1)",
        "SELECT id FROM p2 WHERE id IN (SELECT p_id FROM t2 WHERE b = $1)",
        "SELECT p2.id FROM p2 JOIN t2 ON t2.p_id = p2.id WHERE t2.b = $1",
        "SELECT x.id FROM t2 AS x WHERE x.b = $1",
        "SELECT x.id FROM t2 x WHERE $1 = x.b",
        "WITH f AS (SELECT p_id FROM t2 WHERE b = $1) SELECT id FROM p2 WHERE id IN (SELECT p_id FROM f)",
        "SELECT id FROM p2 WHERE NOT EXISTS (SELECT 1 FROM t2 WHERE t2.p_id = p2.id AND t2.b <> $1)",
        r#"SELECT "public"."p2"."id" FROM "public"."p2" WHERE ("public"."p2"."id") IN (SELECT "t1"."p_id" FROM "public"."t2" AS "t1" WHERE ("t1"."b" = $1 AND "t1"."p_id" IS NOT NULL))"#,
    ] {
        assert_eq!(conn.prepare("", sql).unwrap().param_oids, [16], "{sql}");
        // Bound as text with no type and no Describe first, as node-postgres does.
        let r = conn.exec_params(sql, &["false"], &[], &[], &[]).unwrap();
        assert_eq!(r.rows.len(), 1, "{sql}");
        assert_eq!(r.text(0, 0), "1", "{sql}");
    }

    // Two tables, two types: each parameter takes its own.
    assert_eq!(
        conn.prepare(
            "",
            "SELECT p2.id FROM p2 JOIN t2 AS x ON x.p_id = p2.id WHERE x.note = $1 AND x.b = $2 AND p2.id = $3"
        )
        .unwrap()
        .param_oids,
        [25, 16, 20]
    );
    // A bare name in a subquery belongs to the table of that subquery's FROM.
    assert_eq!(
        conn.prepare(
            "",
            "SELECT id FROM p2 WHERE id IN (SELECT p_id FROM t2 WHERE note = $1)"
        )
        .unwrap()
        .param_oids,
        [25]
    );
}
