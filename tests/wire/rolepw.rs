use crate::common::*;

// per_role_password: a role created with its own password authenticates
// against that password, independent of POSTGRES_PASSWORD (the default role).
#[test]
fn per_role_password() {
    // POSTGRES_PASSWORD enables password auth (scram default)
    let addr = start_server_env(Env::password("adminpw"));

    // The default role connects with the global password and creates a role.
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'alicepw'");

    // alice authenticates with her own password.
    assert!(
        dial_user(&addr, "alice", "alicepw").is_ok(),
        "alice's password should work"
    );
    assert!(dial_user(&addr, "alice", "wrong").is_err(), "wrong password rejected");

    // The default role still uses the global password.
    assert!(dial_user(&addr, "postgres", "adminpw").is_ok());
    assert!(dial_user(&addr, "postgres", "nope").is_err());

    // ALTER ROLE changes the stored password.
    must_exec(&admin, "ALTER ROLE alice PASSWORD 'newpw'");
    assert!(
        dial_user(&addr, "alice", "alicepw").is_err(),
        "old password no longer works"
    );
    assert!(dial_user(&addr, "alice", "newpw").is_ok(), "new password works");
}

// extract_password: the role's own name can spell the word.
#[test]
fn password_after_a_role_named_so() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE password_admin LOGIN PASSWORD 'pw1'");
    assert!(dial_user(&addr, "password_admin", "pw1").is_ok());
    must_exec(&admin, r#"CREATE ROLE "password" LOGIN PASSWORD 'pw2'"#);
    assert!(dial_user(&addr, "password", "pw2").is_ok());
}
