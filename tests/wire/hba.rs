use crate::common::*;

// hba_trust_and_reject: loopback is trusted, everything else rejected. The test
// client connects from 127.0.0.1, so it is trusted.
#[test]
fn hba_trust_and_reject() {
    let mut env = Env::new();
    write_hba(
        &mut env,
        "pg_hba.conf",
        "host all all 127.0.0.1/32 trust\nhost all all 0.0.0.0/0 reject\n",
    );
    let conn = connect(&start_server_env(env));
    let n: i64 = conn.query_row("SELECT 1", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

// hba_reject_loopback: an explicit reject for loopback refuses the connection.
#[test]
fn hba_reject_loopback() {
    let mut env = Env::new();
    write_hba(&mut env, "pg_hba.conf", "host all all 127.0.0.1/32 reject\n");
    assert!(
        dial(&start_server_env(env), "").is_err(),
        "pg_hba reject must refuse the connection"
    );
}

// hba_selects_method: a per-connection rule picks the auth method (md5 here);
// the matching password connects, a wrong one is rejected.
#[test]
fn hba_selects_method() {
    let mut env = Env::password("pw");
    write_hba(&mut env, "pg_hba.conf", "host all all 127.0.0.1/32 md5\n");
    let addr = start_server_env(env);
    assert!(dial(&addr, "pw").is_ok(), "correct password over md5 connects");
    assert!(dial(&addr, "wrong").is_err(), "wrong password is rejected");
}

// hba_from_yaml: the YAML policy drives auth just like the conf one.
#[test]
fn hba_from_yaml() {
    let mut env = Env::new();
    write_hba(
        &mut env,
        "pg_hba.yaml",
        "hba:\n  - {type: host, database: all, user: all, address: 127.0.0.1/32, method: trust}\n  - {type: host, database: all, user: all, address: 0.0.0.0/0, method: reject}\n",
    );
    let conn = connect(&start_server_env(env));
    let n: i64 = conn.query_row("SELECT 1", &[]).scan().unwrap();
    assert_eq!(n, 1);
}
