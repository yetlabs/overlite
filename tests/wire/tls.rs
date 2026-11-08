use crate::common::*;

// With TLS enabled (self-signed), a client using sslmode=require negotiates an
// encrypted connection, while a plaintext client (sslmode=disable) still works.
#[test]
fn tls_connection() {
    let addr = start_server_env(Env::new().with("POSTGRES_SSL", "on")); // reads POSTGRES_SSL -> self-signed TLS

    // sslmode=require: encrypt, don't verify the (self-signed) cert.
    let secure = connect_config(&addr, &Config::new().user("postgres", "").tls()).unwrap();

    assert!(secure.is_tls(), "sslmode=require must negotiate TLS");

    let n: i64 = secure.query_row("SELECT 1", &[]).scan().unwrap();
    assert_eq!(n, 1);

    // A plaintext client (no SSLRequest) is still accepted.
    let plain = connect(&addr);
    let n: i64 = plain.query_row("SELECT 2", &[]).scan().unwrap();
    assert_eq!(n, 2);
}
