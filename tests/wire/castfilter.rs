use crate::common::*;

// A cast after an aggregate's FILTER or OVER took only the parenthesised
// clause as its operand: count(*) CAST(FILTER (...) AS int), a syntax error.
#[test]
fn cast_after_filter_and_over() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE s (id int, on_ boolean)");
    must_exec(&conn, "INSERT INTO s VALUES (1, true), (2, false), (3, true)");

    let filtered: i64 = conn
        .query_row("SELECT count(*) FILTER (WHERE on_)::int FROM s", &[])
        .scan()
        .unwrap();
    assert_eq!(filtered, 2);

    let windowed: i64 = conn
        .query_row("SELECT count(*) OVER ()::int FROM s LIMIT 1", &[])
        .scan()
        .unwrap();
    assert_eq!(windowed, 3);

    let both: i64 = conn
        .query_row("SELECT sum(id) FILTER (WHERE on_) OVER ()::int FROM s LIMIT 1", &[])
        .scan()
        .unwrap();
    assert_eq!(both, 4);
}
