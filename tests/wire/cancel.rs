use std::thread;
use std::time::Duration;

use crate::common::*;

// query_cancellation runs a long query on one connection and cancels it from
// a second connection via a CancelRequest carrying the first's backend key. The
// running query must abort with SQLSTATE 57014.
#[test]
fn query_cancellation() {
    let addr = start_server();

    // Connection A: start up and capture the backend key.
    let mut fe_a = RawConn::dial(&addr).unwrap();
    fe_a.send_startup(&[("user", "postgres"), ("database", "test")]);
    fe_a.flush().unwrap();

    let mut pid = 0u32;
    let mut secret = Vec::new();
    loop {
        match fe_a.receive().unwrap() {
            Backend::KeyData { process_id, secret_key } => {
                pid = process_id;
                secret = secret_key;
            }
            Backend::ReadyForQuery(_) => break,
            _ => {}
        }
    }
    assert_ne!(pid, 0);

    // A starts a long-running query.
    fe_a.send_query(
        "WITH RECURSIVE c(x) AS (
		SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 5000000000)
		SELECT count(*) FROM c",
    );
    fe_a.flush().unwrap();

    // Give the server a moment to start executing, then cancel from connection B.
    thread::sleep(Duration::from_millis(200));
    let mut fe_b = RawConn::dial(&addr).unwrap();
    fe_b.send_cancel_request(pid, &secret);
    fe_b.flush().unwrap();
    drop(fe_b);

    // A must receive an ErrorResponse (57014), not a CommandComplete.
    fe_a.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    loop {
        match fe_a
            .receive()
            .expect("expected a cancellation error, not a hang/completion")
        {
            Backend::ErrorResponse(e) => {
                assert_eq!(e.code, "57014", "query should be cancelled");
                return;
            }
            Backend::CommandComplete(_) => panic!("query completed instead of being cancelled"),
            _ => {}
        }
    }
}
