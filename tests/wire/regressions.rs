//! Bugs found while writing this suite, which had no test before.

use crate::common::*;

// COPY TO escaped the NULL marker along with the data, so `\N` went out as `\\N`
// and a NULL came back from a restore as the text \N.
#[test]
fn copy_to_keeps_null_a_null() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE src (id INTEGER, v TEXT)");
    must_exec(&conn, r"INSERT INTO src VALUES (1, NULL), (2, 'a\b')");

    let mut dump = Vec::new();
    conn.copy_to(&mut dump, "COPY src TO STDOUT").unwrap();
    assert_eq!(dump, b"1\t\\N\n2\ta\\\\b\n");

    must_exec(&conn, "CREATE TABLE dst (id INTEGER, v TEXT)");
    conn.copy_from(dump.as_slice(), "COPY dst FROM STDIN").unwrap();
    let v: Option<String> = conn.query_row("SELECT v FROM dst WHERE id = 1", &[]).scan().unwrap();
    assert_eq!(v, None);
    let v: Option<String> = conn.query_row("SELECT v FROM dst WHERE id = 2", &[]).scan().unwrap();
    assert_eq!(v.as_deref(), Some(r"a\b"));
}
