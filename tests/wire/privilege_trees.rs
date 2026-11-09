//! The privilege-relevant statements the session runs itself (GRANT and REVOKE,
//! roles, row-level security, owners) read from their parse tree: each name as
//! Postgres (17) reads it, whole when quoted, folded when not, and each answer checked
//! through a second role's access.

use crate::common::*;

fn setup() -> (Server, Conn) {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "GRANT CREATE ON SCHEMA public TO alice");
    must_exec(&admin, "CREATE TABLE secret (id int, s text)");
    must_exec(&admin, "INSERT INTO secret VALUES (1, 'topsecret')");
    (addr, admin)
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

// A table named with a space was recorded under its first word: creating "secret
// copy" made its creator the owner of secret, and every grant on it moot.
#[test]
fn a_table_named_with_a_space_is_owned_by_its_whole_name() {
    let (addr, admin) = setup();
    let alice = connect_as(&addr, "alice", "a");
    must_exec(&alice, r#"CREATE TABLE "secret copy" (a int)"#);
    must_exec(&alice, r#"INSERT INTO "secret copy" VALUES (1)"#);
    assert_eq!(strings(&alice, r#"SELECT a::text FROM "secret copy""#), ["1"]);
    denied(&alice, "SELECT s FROM secret");
    denied(&alice, "DROP TABLE secret");
    // A name that is a keyword, quoted, is a name: its creator owns it.
    must_exec(&alice, r#"CREATE TABLE "order" (a int)"#);
    must_exec(&alice, r#"GRANT SELECT ON "order" TO PUBLIC"#);
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'b'");
    let bob = connect_as(&addr, "bob", "b");
    assert_eq!(strings(&bob, r#"SELECT a::text FROM "order""#), Vec::<String>::new());
}

// GRANT and REVOKE name their tables, columns and grantees as Postgres reads them.
#[test]
fn grants_name_what_postgres_reads() {
    let (addr, admin) = setup();
    must_exec(&admin, r#"CREATE SCHEMA "Sales""#);
    must_exec(
        &admin,
        r#"CREATE TABLE "Sales"."Q 1" (id int, "Amount" int, note text)"#,
    );
    must_exec(&admin, r#"INSERT INTO "Sales"."Q 1" VALUES (1, 10, 'n')"#);
    must_exec(&admin, r#"CREATE ROLE "Bob Smith" LOGIN PASSWORD 'b'"#);
    let bob = connect_as(&addr, "Bob Smith", "b");
    denied(&bob, r#"SELECT id FROM "Sales"."Q 1""#);
    must_exec(&admin, r#"GRANT USAGE ON SCHEMA "Sales" TO "Bob Smith""#);
    must_exec(
        &admin,
        r#"GRANT SELECT (id, "Amount") ON TABLE "Sales"."Q 1" TO "Bob Smith""#,
    );
    assert_eq!(strings(&bob, r#"SELECT "Amount"::text FROM "Sales"."Q 1""#), ["10"]);
    denied(&bob, r#"SELECT note FROM "Sales"."Q 1""#);
    // A column named otherwise than the table has it is no column.
    let err = admin
        .exec(r#"GRANT SELECT ("amount") ON "Sales"."Q 1" TO "Bob Smith""#, &[])
        .unwrap_err();
    assert_eq!(err.code(), "42703");
    must_exec(&admin, r#"REVOKE ALL ON "Sales"."Q 1" FROM "Bob Smith""#);
    denied(&bob, r#"SELECT id FROM "Sales"."Q 1""#);
    // GRANT ... TO CURRENT_USER grants to the role running it, not to a role of
    // that name.
    let alice = connect_as(&addr, "alice", "a");
    must_exec(&alice, "CREATE TABLE mine (a int)");
    must_exec(&alice, "INSERT INTO mine VALUES (7)");
    must_exec(&alice, r#"GRANT SELECT ON mine TO "Bob Smith", CURRENT_USER"#);
    assert_eq!(strings(&bob, "SELECT a::text FROM mine"), ["7"]);
    must_exec(&alice, r#"REVOKE SELECT ON mine FROM "Bob Smith""#);
    denied(&bob, "SELECT a FROM mine");
}

// Membership is granted and revoked by the roles' names as Postgres reads them, and
// only by who may administer the role.
#[test]
fn role_membership_reads_its_names() {
    let (addr, admin) = setup();
    must_exec(&admin, r#"CREATE ROLE "Readers""#);
    must_exec(&admin, r#"GRANT SELECT ON secret TO "Readers""#);
    let alice = connect_as(&addr, "alice", "a");
    denied(&alice, "SELECT s FROM secret");
    denied(&alice, r#"GRANT "Readers" TO alice"#);
    must_exec(&admin, r#"GRANT "Readers" TO alice WITH ADMIN OPTION"#);
    assert_eq!(strings(&alice, "SELECT s FROM secret"), ["topsecret"]);
    must_exec(&admin, r#"REVOKE ADMIN OPTION FOR "Readers" FROM alice"#);
    assert_eq!(strings(&alice, "SELECT s FROM secret"), ["topsecret"]);
    denied(&alice, r#"GRANT "Readers" TO postgres"#);
    must_exec(&admin, r#"REVOKE "Readers" FROM alice"#);
    denied(&alice, "SELECT s FROM secret");
}

// CREATE, ALTER and DROP ROLE read a quoted name whole and an option only where it is
// one.
#[test]
fn role_ddl_reads_its_names_and_options() {
    let (addr, admin) = setup();
    must_exec(&admin, r#"CREATE ROLE "login" LOGIN PASSWORD 'it''s'"#);
    connect_as(&addr, "login", "it's");
    must_exec(&admin, r#"ALTER ROLE "login" RENAME TO "Log In""#);
    must_exec(&admin, r#"ALTER ROLE "Log In" PASSWORD 'p2' NOLOGIN"#);
    assert_eq!(
        strings(&admin, r#"SELECT rolname FROM pg_roles WHERE NOT rolcanlogin"#),
        ["Log In"]
    );
    must_exec(&admin, r#"DROP ROLE "Log In""#);
    assert_eq!(
        strings(&admin, "SELECT rolname FROM pg_roles WHERE rolname LIKE 'Log%'"),
        Vec::<String>::new()
    );
    // A role without CREATEROLE manages none but its own password.
    let alice = connect_as(&addr, "alice", "a");
    denied(&alice, "CREATE ROLE eve");
    denied(&alice, "ALTER ROLE postgres PASSWORD 'x'");
    must_exec(&alice, "ALTER ROLE CURRENT_USER PASSWORD 'a2'");
    connect_as(&addr, "alice", "a2");
}

// Policies read their names, roles and actions from the tree: a quoted name with a
// space was a syntax error, a policy TO CURRENT_USER matched no one, and of two row
// level security actions in one ALTER TABLE only the last was done.
#[test]
fn policies_read_their_names_roles_and_actions() {
    let (addr, admin) = setup();
    must_exec(&admin, "CREATE TABLE docs (owner text, v text)");
    must_exec(&admin, "INSERT INTO docs VALUES ('alice', 'a1'), ('bob', 'b1')");
    must_exec(&admin, "GRANT SELECT ON docs TO alice");
    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    must_exec(
        &admin,
        r#"CREATE POLICY "Own rows" ON docs TO alice USING (owner = current_user)"#,
    );
    let alice = connect_as(&addr, "alice", "a");
    assert_eq!(strings(&alice, "SELECT v FROM docs"), ["a1"]);
    must_exec(&admin, r#"DROP POLICY "Own rows" ON docs"#);
    assert_eq!(strings(&alice, "SELECT v FROM docs"), Vec::<String>::new());

    must_exec(&alice, "CREATE TABLE notes (v text)");
    must_exec(&alice, "INSERT INTO notes VALUES ('a'), ('b')");
    must_exec(
        &alice,
        "ALTER TABLE notes ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY",
    );
    must_exec(&alice, "CREATE POLICY pa ON notes USING (v = 'a')");
    assert_eq!(strings(&alice, "SELECT v FROM notes"), ["a"]);

    must_exec(&alice, "CREATE TABLE n2 (v text)");
    must_exec(&alice, "INSERT INTO n2 VALUES ('x')");
    must_exec(&alice, "ALTER TABLE n2 ENABLE ROW LEVEL SECURITY");
    must_exec(&alice, "ALTER TABLE n2 FORCE ROW LEVEL SECURITY");
    must_exec(&alice, "CREATE POLICY mine ON n2 TO CURRENT_USER USING (true)");
    assert_eq!(strings(&alice, "SELECT v FROM n2"), ["x"]);
}
