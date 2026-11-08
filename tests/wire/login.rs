use crate::common::*;

// no_login_role: a role marked NOLOGIN can't open a session even with the
// right password; LOGIN restores it, and ALTER ROLE flips it either way.
#[test]
fn no_login_role() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");

    must_exec(&admin, "CREATE ROLE svc NOLOGIN PASSWORD 'svcpw'");
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'bobpw'");

    // NOLOGIN role is rejected despite the correct password.
    let err = dial_user(&addr, "svc", "svcpw").unwrap_err();
    assert_contains!(err, "not permitted to log in");

    // LOGIN role connects.
    dial_user(&addr, "bob", "bobpw").unwrap();

    // ALTER ROLE flips the flag both ways.
    must_exec(&admin, "ALTER ROLE bob NOLOGIN");
    dial_user(&addr, "bob", "bobpw").expect_err("bob is now NOLOGIN");
    must_exec(&admin, "ALTER ROLE svc LOGIN");
    dial_user(&addr, "svc", "svcpw").expect("svc is now LOGIN");

    // The default superuser is unaffected.
    dial_user(&addr, "postgres", "adminpw").unwrap();
}
