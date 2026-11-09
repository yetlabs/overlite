//! The statements the session runs itself, read from their parse tree: names as
//! Postgres (17) reads them, folded unless quoted.

use std::time::Duration;

use crate::common::*;

// A channel is a name: LISTEN Foo and NOTIFY FOO are both channel foo. The text
// reader kept each as written, and the NOTIFY never arrived.
#[test]
fn a_channel_is_folded_unless_quoted() {
    let addr = start_server();
    let listener = connect(&addr);
    let notifier = connect(&addr);
    listener.exec("LISTEN Foo", &[]).unwrap();
    notifier.exec("NOTIFY FOO, 'it''s'", &[]).unwrap();
    let n = listener.wait_for_notification(Duration::from_secs(3)).unwrap();
    assert_eq!((n.channel.as_str(), n.payload.as_str()), ("foo", "it's"));
    listener.exec(r#"LISTEN "Bar Baz""#, &[]).unwrap();
    notifier.exec(r#"NOTIFY "Bar Baz""#, &[]).unwrap();
    let n = listener.wait_for_notification(Duration::from_secs(3)).unwrap();
    assert_eq!(n.channel, "Bar Baz");
    listener.exec("UNLISTEN *", &[]).unwrap();
    notifier.exec("NOTIFY foo", &[]).unwrap();
    listener
        .wait_for_notification(Duration::from_millis(300))
        .expect_err("no notification after UNLISTEN *");
}

// A prepared statement's name is folded unless quoted, as Postgres reads it: PREPARE
// byId and EXECUTE BYID name one statement, which the text reader kept apart.
#[test]
fn a_prepared_statement_name_is_folded_unless_quoted() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, name text)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 'a'), (2, 'b, c')");
    must_exec(
        &conn,
        "PREPARE byId (int, text) AS SELECT name FROM t WHERE id = $1 OR name = $2",
    );
    assert_eq!(query_column(&conn, "EXECUTE BYID (1, 'b, c')", 0), ["a", "b, c"]);
    must_exec(&conn, r#"PREPARE "Q" AS SELECT 'x'"#);
    assert_eq!(query_column(&conn, r#"EXECUTE "Q""#, 0), ["x"]);
    assert_eq!(conn.exec("EXECUTE q", &[]).unwrap_err().code(), "26000");
    must_exec(&conn, "DEALLOCATE byid");
    assert_eq!(conn.exec("EXECUTE byId (1, 'x')", &[]).unwrap_err().code(), "26000");
}
