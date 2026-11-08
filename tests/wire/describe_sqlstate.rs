use crate::common::*;

// A statement that fails to prepare reports the code it would fail with at
// execution: a missing table is 42P01 whether the client prepares first or not.
#[test]
fn describe_error_code() {
    let conn = connect_extended(&start_server());
    let err = conn.exec(r#"SELECT * FROM nope WHERE a = $1"#, &[&1]).unwrap_err();
    assert_sqlstate(&err, "42P01");
}
