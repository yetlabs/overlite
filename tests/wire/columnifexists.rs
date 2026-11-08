use crate::common::*;

// SQLite has no IF [NOT] EXISTS on a column; Evolution API's migrations use it.
#[test]
fn column_if_exists() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, r#"CREATE TABLE "Setting" ("id" TEXT PRIMARY KEY)"#);

    must_exec(
        &conn,
        r#"ALTER TABLE "Setting" ADD COLUMN IF NOT EXISTS "wavoipToken" VARCHAR(100)"#,
    );
    must_exec(
        &conn,
        r#"ALTER TABLE "Setting" ADD COLUMN IF NOT EXISTS "wavoipToken" VARCHAR(100)"#,
    ); // again: no-op
    must_exec(
        &conn,
        r#"ALTER TABLE "Setting" ADD IF NOT EXISTS note text DEFAULT 'x'"#,
    );
    must_exec(
        &conn,
        r#"INSERT INTO "Setting" ("id", "wavoipToken") VALUES ('s', 'tok')"#,
    );

    let (tok, note): (String, String) = conn
        .query_row(r#"SELECT "wavoipToken", note FROM "Setting""#, &[])
        .scan()
        .unwrap();
    assert_eq!(tok, "tok");
    assert_eq!(note, "x");

    must_exec(&conn, r#"ALTER TABLE "Setting" DROP COLUMN IF EXISTS note"#);
    must_exec(&conn, r#"ALTER TABLE "Setting" DROP COLUMN IF EXISTS note"#); // already gone: no-op
    let n: i64 = conn
        .query_row("SELECT count(*) FROM pragma_table_info('Setting')", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 2, "id and wavoipToken");
}

// The lookup has to find a table in its schema, however it is spelled or reached.
#[test]
fn column_if_exists_in_a_schema() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE SCHEMA s1");
    must_exec(&conn, "CREATE TABLE s1.t (a text, b text)");
    let columns = |conn: &Conn| {
        query_column(
            conn,
            "SELECT column_name FROM information_schema.columns WHERE table_schema = 's1' AND table_name = 't' ORDER BY ordinal_position",
            0,
        )
    };

    must_exec(&conn, "ALTER TABLE s1.t ADD COLUMN IF NOT EXISTS a text"); // duplicate column
    assert_eq!(columns(&conn), ["a", "b"]);
    must_exec(&conn, "ALTER TABLE s1.t DROP COLUMN IF EXISTS b"); // silently nothing
    assert_eq!(columns(&conn), ["a"]);
    must_exec(&conn, "ALTER TABLE s1.t DROP COLUMN IF EXISTS b"); // already gone
    must_exec(&conn, r#"ALTER TABLE "s1"."t" ADD COLUMN IF NOT EXISTS c text"#);
    must_exec(&conn, r#"ALTER TABLE "s1"."t" ADD COLUMN IF NOT EXISTS c text"#);
    assert_eq!(columns(&conn), ["a", "c"]);

    must_exec(&conn, "SET search_path TO s1");
    must_exec(&conn, "ALTER TABLE t ADD COLUMN IF NOT EXISTS a text");
    must_exec(&conn, "ALTER TABLE t DROP COLUMN IF EXISTS c");
    assert_eq!(columns(&conn), ["a"]);
}

// A literal that reads like the clause is a literal.
#[test]
fn column_if_exists_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (a text, b text)");

    must_exec(&conn, "ALTER TABLE t ALTER COLUMN b SET DEFAULT 'ADD IF NOT EXISTS a'");
    must_exec(
        &conn,
        "ALTER TABLE t ADD COLUMN c text DEFAULT 'DROP COLUMN IF EXISTS a'",
    );
    must_exec(&conn, "INSERT INTO t (a) VALUES ('x')");
    let (b, c): (String, String) = conn.query_row("SELECT b, c FROM t", &[]).scan().unwrap();
    assert_eq!(b, "ADD IF NOT EXISTS a");
    assert_eq!(c, "DROP COLUMN IF EXISTS a");

    // Nor is a quoted name.
    must_exec(&conn, r#"ALTER TABLE t ADD COLUMN "DROP IF EXISTS a" text"#);
    must_exec(&conn, r#"ALTER TABLE t RENAME COLUMN "DROP IF EXISTS a" TO d"#);
    let n: i64 = conn
        .query_row("SELECT count(*) FROM pragma_table_info('t') WHERE name = 'd'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);
}

// A comment before the clause does not hide it.
#[test]
fn column_if_exists_after_a_comment() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (a text)");
    must_exec(&conn, "ALTER TABLE t /* again */ ADD COLUMN IF NOT EXISTS a text");
}
