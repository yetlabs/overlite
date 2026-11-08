use crate::common::*;

// Migration tools write a comment above every statement: Prisma's migrations
// read `-- CreateSchema` over CREATE SCHEMA and `-- AddForeignKey` over each
// ALTER TABLE … ADD CONSTRAINT. The statements overlite handles itself are
// recognized by their first words, so a comment in front of them used to send
// them straight to SQLite, which rejects both.

#[test]
fn leading_comment_keeps_statement_routing() {
    for (name, conn) in [
        ("simple", connect(&start_server())),
        ("extended", connect_extended(&start_server())),
    ] {
        println!("{name}");

        must_exec(&conn, "-- CreateSchema\nCREATE SCHEMA IF NOT EXISTS \"public\"");
        must_exec(&conn, "/* tenants */ CREATE SCHEMA sales");
        must_exec(&conn, r#"CREATE TABLE "Parent" (id TEXT PRIMARY KEY)"#);
        must_exec(&conn, r#"CREATE TABLE "Child" (id TEXT PRIMARY KEY, "parentId" TEXT)"#);
        must_exec(
            &conn,
            concat!(
                "-- AddForeignKey\n",
                r#"ALTER TABLE "Child" ADD CONSTRAINT "Child_parentId_fkey" FOREIGN KEY ("parentId") "#,
                r#"REFERENCES "Parent"("id") ON DELETE CASCADE ON UPDATE CASCADE"#
            ),
        );

        conn.exec(r#"INSERT INTO "Child" VALUES ('c', 'missing')"#, &[])
            .expect_err("the foreign key must hold");

        must_exec(&conn, r#"INSERT INTO "Parent" VALUES ('p')"#);
        must_exec(&conn, r#"INSERT INTO "Child" VALUES ('c', 'p')"#);
        must_exec(&conn, "-- cascade\nDELETE FROM \"Parent\"");
        let n: i64 = conn.query_row(r#"SELECT count(*) FROM "Child""#, &[]).scan().unwrap();
        assert_eq!(n, 0, "ON DELETE CASCADE must remove the child");

        // Transaction control is routed the same way.
        must_exec(&conn, "-- start\nBEGIN");
        must_exec(&conn, r#"INSERT INTO "Parent" VALUES ('q')"#);
        must_exec(&conn, "-- undo\nROLLBACK");
        let n: i64 = conn.query_row(r#"SELECT count(*) FROM "Parent""#, &[]).scan().unwrap();
        assert_eq!(n, 0, "the commented ROLLBACK must undo the insert");
    }
}
