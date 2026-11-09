use crate::common::*;

// current_user_simple_protocol: with no password (trust), the session reflects
// the login role (the test client logs in as "overlite").
#[test]
fn current_user_simple_protocol() {
    let conn = connect(&start_server());
    assert_eq!(scan_str(&conn, "SELECT current_user"), "overlite");
    assert_eq!(scan_str(&conn, "SELECT session_user"), "overlite");
    // current_user takes no parentheses, as Postgres' grammar has it.
    let err = conn
        .query_row("SELECT current_user()", &[])
        .scan::<String>()
        .unwrap_err();
    assert_pg_error(&err, "42601", "syntax error at or near \"(\"");
    assert_eq!(pg_error(&err).position, "20");
}

// set_role: current_user follows SET ROLE; session_user stays the login role;
// RESET ROLE restores it; a missing role errors.
#[test]
fn set_role() {
    let addr = start_server_env(Env::password("adminpw"));

    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'alicepw'");
    must_exec(&admin, "CREATE ROLE reporter");
    must_exec(&admin, "CREATE ROLE other");
    must_exec(&admin, "GRANT reporter TO alice"); // alice may become reporter

    let alice = connect_as(&addr, "alice", "alicepw");
    assert_eq!(scan_str(&alice, "SELECT current_user"), "alice");
    assert_eq!(scan_str(&alice, "SELECT session_user"), "alice");

    // SET ROLE to a role she's a member of changes current_user, not session_user.
    must_exec(&alice, "SET ROLE reporter");
    assert_eq!(scan_str(&alice, "SELECT current_user"), "reporter");
    assert_eq!(scan_str(&alice, "SELECT session_user"), "alice");

    // RESET ROLE restores.
    must_exec(&alice, "RESET ROLE");
    assert_eq!(scan_str(&alice, "SELECT current_user"), "alice");

    // SET ROLE to a missing role is an error, and identity is unchanged.
    alice.exec("SET ROLE ghost", &[]).unwrap_err();
    assert_eq!(scan_str(&alice, "SELECT current_user"), "alice");

    // SET ROLE to a role she is NOT a member of is denied.
    alice.exec("SET ROLE other", &[]).expect_err("not a member of other");
    assert_eq!(scan_str(&alice, "SELECT current_user"), "alice");
}

// set_session_authorization changes both session_user and current_user.
#[test]
fn set_session_authorization() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE svc");

    must_exec(&admin, "SET SESSION AUTHORIZATION svc");
    assert_eq!(scan_str(&admin, "SELECT session_user"), "svc");
    assert_eq!(scan_str(&admin, "SELECT current_user"), "svc");

    must_exec(&admin, "RESET SESSION AUTHORIZATION");
    assert_eq!(scan_str(&admin, "SELECT session_user"), "postgres");
}
