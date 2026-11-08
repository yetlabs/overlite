use crate::common::*;

// rls_cache_setup leaves docs with a policy but RLS off, and alice connected with
// the "no RLS here" answer already cached.
fn rls_cache_setup() -> (Conn, Conn) {
    let addr = start_server_env(Env::password("adminpw"));
    let a = connect_as(&addr, "postgres", "adminpw");
    must_exec(&a, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&a, "CREATE TABLE docs (id int, owner text)");
    must_exec(&a, "INSERT INTO docs VALUES (1,'alice'),(2,'bob'),(3,'alice')");
    must_exec(&a, "GRANT ALL ON docs TO alice");
    must_exec(&a, "CREATE POLICY own ON docs FOR ALL USING (owner = current_user)");

    let alice = connect_as(&addr, "alice", "a");
    assert_eq!(count(&alice, "SELECT count(*) FROM docs"), 3, "RLS is off");
    (a, alice)
}

fn count(conn: &Conn, sql: &str) -> i64 {
    conn.query_row(sql, &[]).scan().unwrap()
}

// Turning RLS on must apply from the next statement of every other session: the
// cached answer would otherwise serve rows the policy hides.
#[test]
fn rls_cache_sees_enable_and_disable() {
    let (admin, alice) = rls_cache_setup();

    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    assert_eq!(count(&alice, "SELECT count(*) FROM docs"), 2);

    must_exec(&admin, "ALTER TABLE docs DISABLE ROW LEVEL SECURITY");
    assert_eq!(count(&alice, "SELECT count(*) FROM docs"), 3);
}

#[test]
fn rls_cache_write_is_filtered() {
    let (admin, alice) = rls_cache_setup();

    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    must_exec(&alice, "DELETE FROM docs");
    must_exec(&admin, "ALTER TABLE docs DISABLE ROW LEVEL SECURITY");
    assert_eq!(count(&alice, "SELECT count(*) FROM docs"), 1, "bob's row survives");
}

// Until the transaction commits the change is not visible to anyone else, and
// its end is what must reach them.
#[test]
fn rls_cache_waits_for_commit() {
    let (admin, alice) = rls_cache_setup();

    must_exec(&admin, "BEGIN");
    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    assert_eq!(count(&alice, "SELECT count(*) FROM docs"), 3, "not committed yet");
    must_exec(&admin, "COMMIT");
    assert_eq!(count(&alice, "SELECT count(*) FROM docs"), 2);
}

#[test]
fn rls_cache_rollback() {
    let (admin, alice) = rls_cache_setup();

    must_exec(&admin, "BEGIN");
    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "ROLLBACK");
    assert_eq!(count(&alice, "SELECT count(*) FROM docs"), 3);
}

// The session that made the change sees it inside its own transaction, even
// though the shared answer is still the committed one.
#[test]
fn rls_cache_sees_own_transaction() {
    let (admin, alice) = rls_cache_setup();

    must_exec(&alice, "BEGIN");
    assert_eq!(count(&alice, "SELECT count(*) FROM docs"), 3);
    must_exec(&alice, "ROLLBACK");

    must_exec(&admin, "BEGIN");
    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "SET ROLE alice");
    assert_eq!(count(&admin, "SELECT count(*) FROM docs"), 2);
    must_exec(&admin, "ROLLBACK");
}

// Names compare the way SQLite's lower() does, which folds ASCII only.
#[test]
fn rls_cache_non_ascii_name() {
    let (admin, alice) = rls_cache_setup();

    must_exec(&admin, r#"CREATE TABLE "AÇÃO" (id int, owner text)"#);
    must_exec(&admin, r#"INSERT INTO "AÇÃO" VALUES (1,'alice'),(2,'bob')"#);
    must_exec(&admin, r#"GRANT ALL ON "AÇÃO" TO alice"#);
    must_exec(
        &admin,
        r#"CREATE POLICY own ON "AÇÃO" FOR ALL USING (owner = current_user)"#,
    );
    must_exec(&admin, r#"ALTER TABLE "AÇÃO" ENABLE ROW LEVEL SECURITY"#);
    assert_eq!(count(&alice, r#"SELECT count(*) FROM "AÇÃO""#), 1);
}
