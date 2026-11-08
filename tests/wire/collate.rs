use crate::common::*;

// collate: Postgres COLLATE clauses are accepted (never a "no such
// collation" error) and mapped to SQLite — case-insensitive collations become
// NOCASE, everything else (C/POSIX/locale) falls back to the byte-order default.
#[test]
fn collate() {
    let conn = connect(&start_server());

    // A locale collation is accepted; comparison falls back to byte order.
    conn.exec(r#"CREATE TABLE loc (tag text COLLATE "en_US")"#, &[])
        .unwrap();
    conn.exec("INSERT INTO loc VALUES ('Hello')", &[]).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM loc WHERE tag = 'hello'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0); // byte order: 'Hello' != 'hello'

    // A case-insensitive collation maps to NOCASE.
    conn.exec("CREATE TABLE ci (name text COLLATE case_insensitive)", &[])
        .unwrap();
    conn.exec("INSERT INTO ci VALUES ('Hello')", &[]).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM ci WHERE name = 'hello'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1); // NOCASE: 'Hello' == 'hello'

    // COLLATE in an expression, both directions.
    let ci_eq: bool = conn
        .query_row("SELECT 'ABC' COLLATE case_insensitive = 'abc'", &[])
        .scan()
        .unwrap();
    assert!(ci_eq);
    let c_eq: bool = conn
        .query_row(r#"SELECT 'ABC' COLLATE "C" = 'abc'"#, &[])
        .scan()
        .unwrap();
    assert!(!c_eq);
}
