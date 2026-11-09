//! Every route by which a statement reads or writes rows is held to the role's
//! privileges and policies, as in Postgres (17): CREATE TABLE AS, materialized views,
//! PREPARE/EXECUTE, cursors, COPY, a SQLite trigger's body, and the server's own
//! tables, which no role but a superuser reaches.

use crate::common::*;

fn setup() -> (Server, Conn, Conn) {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE secret (id int, s text)");
    must_exec(&admin, "INSERT INTO secret VALUES (1, 'topsecret')");
    must_exec(&admin, "CREATE TABLE pub (id int, v text)");
    must_exec(&admin, "GRANT ALL ON pub TO alice");
    must_exec(&admin, "CREATE TABLE r (owner text, v text)");
    must_exec(&admin, "INSERT INTO r VALUES ('alice', 'a1'), ('bob', 'b1')");
    must_exec(&admin, "ALTER TABLE r ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY own ON r USING (owner = current_user)");
    must_exec(&admin, "GRANT SELECT, INSERT ON r TO alice");
    let alice = connect_as(&addr, "alice", "a");
    (addr, admin, alice)
}

#[track_caller]
fn denied(conn: &Conn, sql: &str) {
    match conn.exec(sql, &[]) {
        Err(e) if e.code() == "42501" => {}
        other => panic!("{sql}: {other:?}"),
    }
}

fn strings(conn: &Conn, sql: &str) -> Vec<String> {
    query_column(conn, sql, 0)
}

// CREATE TABLE AS read any table into one of the role's own, and read past policies.
#[test]
fn create_table_as_reads_with_the_creators_privileges() {
    let (_addr, admin, alice) = setup();
    denied(&alice, "CREATE TABLE mine AS SELECT * FROM secret");
    denied(&alice, "CREATE TABLE mine (a, b) AS SELECT id, s FROM secret WITH DATA");
    denied(&alice, "CREATE TEMP TABLE mine AS SELECT (SELECT s FROM secret) AS s");
    assert_eq!(
        strings(&admin, "SELECT count(*)::text FROM sqlite_master WHERE name = 'mine'"),
        ["0"]
    );
    must_exec(&alice, "CREATE TABLE rr AS SELECT * FROM r");
    assert_eq!(strings(&admin, "SELECT owner FROM rr"), ["alice"]);
}

// A materialized view is filled with its creator's privileges, and refreshed by its
// owner alone, with the owner's.
#[test]
fn a_materialized_view_reads_with_its_owners_privileges() {
    let (_addr, admin, alice) = setup();
    denied(&alice, "CREATE MATERIALIZED VIEW mv AS SELECT * FROM secret");
    assert_eq!(
        strings(&admin, "SELECT count(*)::text FROM sqlite_master WHERE name LIKE '%mv'"),
        ["0"]
    );
    must_exec(
        &alice,
        "CREATE MATERIALIZED VIEW mv AS SELECT * FROM secret WITH NO DATA",
    );
    denied(&alice, "REFRESH MATERIALIZED VIEW mv");
    assert_eq!(strings(&admin, "SELECT count(*)::text FROM mv"), ["0"]);
    must_exec(&alice, "CREATE MATERIALIZED VIEW rv AS SELECT * FROM r");
    assert_eq!(strings(&alice, "SELECT owner FROM rv"), ["alice"]);
    // The hidden view holding its query is not a way around it.
    denied(&alice, "SELECT * FROM _overlite_mv_mv");
    must_exec(&admin, "CREATE MATERIALIZED VIEW amv AS SELECT * FROM secret");
    must_exec(&admin, "GRANT SELECT ON amv TO alice");
    assert_eq!(strings(&alice, "SELECT s FROM amv"), ["topsecret"]);
    let err = alice.exec("REFRESH MATERIALIZED VIEW amv", &[]).unwrap_err();
    assert_pg_error(&err, "42501", "must be owner of materialized view amv");
}

// SQL-level EXECUTE ran an INSERT past the policy's check, and every other route is
// held to the grants.
#[test]
fn prepared_cursor_and_copy_routes_are_checked() {
    let (_addr, admin, alice) = setup();
    must_exec(&alice, "PREPARE ins AS INSERT INTO r VALUES ('bob', 'forged')");
    denied(&alice, "EXECUTE ins");
    must_exec(&alice, "PREPARE ins2(text) AS INSERT INTO r VALUES ($1, 'x')");
    denied(&alice, "EXECUTE ins2('bob')");
    must_exec(&alice, "EXECUTE ins2('alice')");
    assert_eq!(
        strings(&admin, "SELECT owner || v FROM r ORDER BY 1"),
        ["alicea1", "alicex", "bobb1"]
    );
    must_exec(&alice, "PREPARE rd AS SELECT s FROM secret");
    denied(&alice, "EXECUTE rd");
    must_exec(&alice, "BEGIN");
    denied(&alice, "DECLARE c CURSOR FOR SELECT s FROM secret");
    must_exec(&alice, "ROLLBACK");
    let mut out = Vec::new();
    assert_sqlstate(
        &alice
            .copy_to(&mut out, "COPY (SELECT s FROM secret) TO STDOUT")
            .unwrap_err(),
        "42501",
    );
    assert_eq!(String::from_utf8_lossy(&out), "");
    // SQLite's INSERT OR REPLACE does not parse as Postgres, and is read from its text:
    // what it reads still needs SELECT.
    alice
        .exec("INSERT OR REPLACE INTO pub SELECT id, s FROM secret", &[])
        .unwrap_err();
    assert_eq!(strings(&admin, "SELECT count(*)::text FROM pub"), ["0"]);
}

// A SQLite trigger runs its body with the privileges of the role whose write fires it,
// as a Postgres trigger function does, and only a table's owner adds or drops one.
#[test]
fn a_trigger_body_needs_the_writers_privileges() {
    let (_addr, admin, alice) = setup();
    must_exec(&alice, "CREATE TABLE loot (x text)");
    must_exec(&alice, "CREATE TABLE trg (x text)");
    must_exec(
        &alice,
        "CREATE TRIGGER copy_secret AFTER INSERT ON trg BEGIN INSERT INTO loot SELECT s FROM secret; END",
    );
    denied(&alice, "INSERT INTO trg VALUES ('a')");
    assert_eq!(strings(&alice, "SELECT count(*)::text FROM loot"), ["0"]);
    // A trigger on a table of someone else's, waiting for its owner's writes.
    denied(
        &alice,
        "CREATE TRIGGER steal AFTER INSERT ON secret BEGIN INSERT INTO loot VALUES (NEW.s); END",
    );
    must_exec(
        &admin,
        "CREATE TRIGGER audit AFTER INSERT ON pub BEGIN INSERT INTO secret VALUES (NEW.id, NEW.v); END",
    );
    denied(&alice, "DROP TRIGGER audit");
    // The body writes secret, which alice may not.
    denied(&alice, "INSERT INTO pub VALUES (1, 'x')");
    must_exec(&admin, "GRANT INSERT ON secret TO alice");
    must_exec(&alice, "INSERT INTO pub VALUES (1, 'x')");
    assert_eq!(strings(&admin, "SELECT count(*)::text FROM secret"), ["2"]);
}

// The server's own tables: writing _overlite_roles made a role a superuser.
#[test]
fn the_servers_own_tables_are_a_superusers() {
    let (_addr, admin, alice) = setup();
    denied(
        &alice,
        "UPDATE _overlite_roles SET rolsuper = 1 WHERE rolname = 'alice'",
    );
    denied(&alice, "SELECT rolpassword FROM _overlite_roles");
    denied(
        &alice,
        "INSERT INTO _overlite_grants (grantee, tablename, privilege) VALUES ('alice', 'secret', 'SELECT')",
    );
    denied(&alice, "DELETE FROM _overlite_policies");
    denied(&alice, "SELECT * FROM main._overlite_owners");
    denied(&alice, "PRAGMA writable_schema = 1");
    denied(&alice, "ATTACH DATABASE ':memory:' AS x");
    denied(&alice, "VACUUM INTO '/nonexistent/copy.db'");
    denied(&alice, "SELECT data FROM sqlite_dbpage");
    denied(&alice, "SELECT data FROM sqlite_dbpage('main')");
    denied(&alice, "UPDATE sqlite_dbpage SET data = NULL WHERE pgno = 2");
    denied(&alice, "SELECT s FROM secret");
    assert_eq!(
        strings(
            &admin,
            "SELECT CASE WHEN rolsuper THEN 'yes' ELSE 'no' END FROM _overlite_roles WHERE rolname = 'alice'"
        ),
        ["no"]
    );
}
