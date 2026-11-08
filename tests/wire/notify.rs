use std::time::Duration;

use crate::common::*;

// listen_notify: a session that LISTENs on a channel receives a NOTIFY sent by
// another session on the same server (real in-memory delivery).
#[test]
fn listen_notify() {
    let addr = start_server();
    let listener = connect(&addr);
    let notifier = connect(&addr);

    listener.exec("LISTEN chan1", &[]).unwrap();

    // Notify from the other connection, with a payload.
    notifier.exec("NOTIFY chan1, 'hello world'", &[]).unwrap();

    let n = listener.wait_for_notification(Duration::from_secs(3)).unwrap();
    assert_eq!(n.channel, "chan1");
    assert_eq!(n.payload, "hello world");

    // UNLISTEN stops delivery: a following NOTIFY must not arrive.
    listener.exec("UNLISTEN chan1", &[]).unwrap();
    notifier.exec("NOTIFY chan1, 'ignored'", &[]).unwrap();
    listener
        .wait_for_notification(Duration::from_millis(400))
        .expect_err("no notification after UNLISTEN");
}
