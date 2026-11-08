use crate::common::*;

// The image's ENTRYPOINT passes `--host 0.0.0.0`, and a compose file that passes
// `--host` again was refused with "cannot be used multiple times": the Go server
// took the last of a repeated flag. Every flag here comes twice, the harness's
// own `--host`, `--port` and `--db` last, so the server answers only if the last
// one wins.
#[test]
fn a_repeated_flag_takes_the_last() {
    let env = Env::new().with_args(&[
        "--host",
        "0.0.0.0",
        "--port",
        "1",
        "--db",
        "first.db",
        "--driver",
        "postgres",
        "--driver",
        "postgres",
        "--max-open-databases",
        "1",
        "--max-open-databases",
        "8",
    ]);
    let server = start_server_env(env);
    let conn = connect(&server);
    assert_eq!(scan_str(&conn, "SELECT current_database()"), "test");
}
