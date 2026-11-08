use crate::common::*;

// portal_partial_fetch drives the extended protocol at the wire level (the
// high-level client doesn't expose Execute's max-rows) to verify partial fetch:
// Execute with MaxRows=2 returns two rows and PortalSuspended, and the next
// Execute continues from where it left off.
#[test]
fn portal_partial_fetch() {
    let addr = start_server();
    let mut fe = RawConn::dial(&addr).unwrap();

    // Startup + trust auth (no password in tests) up to the first ReadyForQuery.
    fe.send_startup(&[("user", "postgres"), ("database", "test")]);
    fe.flush().unwrap();
    fe.wait_ready().unwrap();

    // Seed 5 rows via the simple query protocol.
    fe.send_query("CREATE TABLE n (i int); INSERT INTO n VALUES (1),(2),(3),(4),(5)");
    fe.flush().unwrap();
    fe.wait_ready().unwrap();

    // Extended: parse/bind/describe, then fetch in pages of 2.
    fe.send_parse("", "SELECT i FROM n ORDER BY i", &[]);
    fe.send_bind("", "", &[], &[], &[]);
    fe.send_describe(b'P', "");
    fe.send_execute("", 2);
    fe.send_sync();
    fe.flush().unwrap();

    let (got, suspended) = collect_execute(&mut fe);
    assert_eq!(got, ["1", "2"]);
    assert!(suspended, "first page should suspend with more rows pending");
    fe.wait_ready().unwrap();

    // Second page: 2 more rows, still suspended.
    fe.send_execute("", 2);
    fe.send_sync();
    fe.flush().unwrap();
    let (got, suspended) = collect_execute(&mut fe);
    assert_eq!(got, ["3", "4"]);
    assert!(suspended);
    fe.wait_ready().unwrap();

    // Final page: the last row, then CommandComplete (not suspended).
    fe.send_execute("", 2);
    fe.send_sync();
    fe.flush().unwrap();
    let (got, suspended) = collect_execute(&mut fe);
    assert_eq!(got, ["5"]);
    assert!(!suspended, "last page should complete, not suspend");
}
