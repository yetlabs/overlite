use crate::common::*;

// Prisma applies a migration file as a single Query message; this is the
// shape of one, comments over every statement included.
const MIGRATION_BATCH: &str = r#"-- CreateSchema
CREATE SCHEMA IF NOT EXISTS "public";

-- CreateTable
CREATE TABLE "School" (
    "id" TEXT NOT NULL,
    "name" TEXT NOT NULL,
    CONSTRAINT "School_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "User" (
    "id" TEXT NOT NULL,
    "schoolId" TEXT NOT NULL,
    CONSTRAINT "User_pkey" PRIMARY KEY ("id")
);

-- AddForeignKey
ALTER TABLE "User" ADD CONSTRAINT "User_schoolId_fkey" FOREIGN KEY ("schoolId") REFERENCES "School"("id") ON DELETE CASCADE ON UPDATE CASCADE;
"#;

#[test]
fn multi_statement_query_runs_every_statement() {
    let conn = connect(&start_server());

    must_exec(&conn, MIGRATION_BATCH);

    must_exec(&conn, r#"INSERT INTO "School" VALUES ('s', 'x')"#);
    must_exec(&conn, r#"INSERT INTO "User" VALUES ('u', 's')"#);
    conn.exec(r#"INSERT INTO "User" VALUES ('v', 'missing')"#, &[])
        .expect_err("the foreign key from the batch must hold");
    must_exec(&conn, r#"DELETE FROM "School""#);
    let n: i64 = conn.query_row(r#"SELECT count(*) FROM "User""#, &[]).scan().unwrap();
    assert_eq!(n, 0, "ON DELETE CASCADE from the batch must apply");
}

#[test]
fn multi_statement_query_returns_each_result() {
    let conn = connect(&start_server());

    let results = conn.pg_exec("SELECT 1 AS a; SELECT 'x;y' AS b, 2 AS c").unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].rows[0], vec![Some(b"1".to_vec())]);
    assert_eq!(results[1].rows[0], vec![Some(b"x;y".to_vec()), Some(b"2".to_vec())]);
}

#[test]
fn multi_statement_query_is_one_transaction() {
    let conn = connect(&start_server());

    conn.exec(
        "CREATE TABLE a (x int); INSERT INTO a VALUES (1); INSERT INTO missing VALUES (1); CREATE TABLE b (x int)",
        &[],
    )
    .unwrap_err();

    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM information_schema.tables WHERE table_name IN ('a', 'b')",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 0, "a failure must undo the statements before it and skip the rest");

    // The session is usable afterwards, outside any transaction.
    must_exec(&conn, "CREATE TABLE c (x int); INSERT INTO c VALUES (1), (2)");
    let n: i64 = conn.query_row("SELECT count(*) FROM c", &[]).scan().unwrap();
    assert_eq!(n, 2);

    // Explicit transaction control inside the batch is honored as written.
    must_exec(
        &conn,
        "BEGIN; INSERT INTO c VALUES (3); ROLLBACK; INSERT INTO c VALUES (4)",
    );
    let n: i64 = conn.query_row("SELECT count(*) FROM c", &[]).scan().unwrap();
    assert_eq!(n, 3);
}
