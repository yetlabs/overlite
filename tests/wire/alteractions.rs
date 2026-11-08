use crate::common::*;

// An ALTER TABLE with several actions applies them all, or none when one fails.
#[test]
fn alter_table_several_actions() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE "School" ("id" TEXT NOT NULL PRIMARY KEY)"#);

    must_exec(
        &conn,
        concat!(
            "-- AlterTable\nALTER TABLE \"School\" ADD COLUMN     \"nickname\" TEXT,\n",
            "ADD COLUMN     \"rank\" INTEGER NOT NULL DEFAULT 0;"
        ),
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'School' ORDER BY ordinal_position",
            0
        ),
        ["id", "nickname", "rank"]
    );

    conn.exec(
        r#"ALTER TABLE "School" ADD COLUMN "city" TEXT, ADD COLUMN "rank" INTEGER"#,
        &[],
    )
    .unwrap_err();
    assert_eq!(
        query_column(
            &conn,
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'School' ORDER BY ordinal_position",
            0
        ),
        ["id", "nickname", "rank"],
        "the first action must be undone with the failed second"
    );
}

fn extended(conn: &Conn, sql: &str) -> Result<PgResult> {
    conn.exec_params::<&[u8]>(sql, &[], &[], &[], &[])
}

fn columns(conn: &Conn, table: &str) -> Vec<String> {
    query_column(
        conn,
        &format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"),
        0,
    )
}

// The extended protocol takes the same comma-separated actions as a Query message.
#[test]
fn alter_table_several_actions_extended() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id text PRIMARY KEY)");

    let r = extended(
        &conn,
        "ALTER TABLE t ADD COLUMN IF NOT EXISTS a text, ADD COLUMN IF NOT EXISTS id text",
    )
    .unwrap();
    assert_eq!(r.tag.as_str(), "ALTER TABLE");
    assert_eq!(columns(&conn, "t"), ["id", "a"], "the first action was lost");

    // Actions no single-action route takes over, which SQLite refuses whole.
    extended(&conn, "ALTER TABLE t ADD COLUMN b text, ADD COLUMN c int").unwrap();
    assert_eq!(columns(&conn, "t"), ["id", "a", "b", "c"]);

    must_exec(&conn, "ALTER TABLE t ADD CONSTRAINT k1 UNIQUE (a)");
    must_exec(&conn, "ALTER TABLE t ADD CONSTRAINT k2 UNIQUE (b)");
    extended(&conn, "ALTER TABLE t DROP CONSTRAINT k1, DROP CONSTRAINT k2").unwrap();
    must_exec(
        &conn,
        "INSERT INTO t (id, a, b) VALUES ('1', 'x', 'y'), ('2', 'x', 'y')",
    );
}

// A comma in a quoted name separates nothing, in either path.
#[test]
fn alter_table_comma_in_a_quoted_name() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id text)");
    must_exec(&conn, r#"ALTER TABLE t ADD COLUMN "a,b" text"#);
    extended(&conn, r#"ALTER TABLE t ADD COLUMN "c,d" text"#).unwrap();
    assert_eq!(columns(&conn, "t"), ["id", "a,b", "c,d"]);
}

// Like the simple path, they apply together or not at all.
#[test]
fn alter_table_several_actions_extended_atomic() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id text PRIMARY KEY)");

    let err = extended(&conn, "ALTER TABLE t ADD COLUMN a text, ADD COLUMN id text").unwrap_err();
    assert!(err.to_string().contains("duplicate column"), "{err}");
    assert_eq!(columns(&conn, "t"), ["id"], "undone with the failed second");

    // Inside a transaction the transaction owns the outcome.
    let tx = conn.begin().unwrap();
    tx.exec("SELECT 1", &[]).unwrap();
    extended(&conn, "ALTER TABLE t ADD COLUMN a text, ADD COLUMN b text").unwrap();
    assert_eq!(columns(&conn, "t"), ["id", "a", "b"]);
    tx.rollback().unwrap();
    assert_eq!(columns(&conn, "t"), ["id"]);
}

// Taking the statement apart does not take the ownership check with it.
#[test]
fn alter_table_several_actions_extended_needs_owner() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE t (id int)");

    let alice = connect_as(&addr, "alice", "a");
    let err = extended(&alice, "ALTER TABLE t ADD COLUMN a text, ADD COLUMN b text").unwrap_err();
    assert_sqlstate(&err, "42501");
    assert_eq!(columns(&admin, "t"), ["id"]);
}
