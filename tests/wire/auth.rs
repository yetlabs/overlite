use crate::common::*;

// With POSTGRES_PASSWORD set, clients must present the matching password;
// without it, connections are trusted (covered by every other test).
#[test]
fn password_auth() {
    let addr = start_server_env(Env::password("s3cr3t"));

    assert!(dial(&addr, "s3cr3t").is_ok(), "correct password should connect");
    assert!(dial(&addr, "wrong").is_err(), "wrong password should be rejected");
}

#[test]
fn scram_auth() {
    auth_method_check("scram-sha-256");
}

#[test]
fn md5_auth_method() {
    auth_method_check("md5");
}

#[test]
fn cleartext_auth_method() {
    auth_method_check("password");
}

#[test]
fn default_auth_is_scram() {
    auth_method_check("");
}
