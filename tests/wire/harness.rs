//! The client's own tests. The suite is only as strict as `Conn`, so what it must
//! refuse is pinned here too.

use crate::common::*;

#[test]
fn sanitize_quotes_every_argument() {
    let args = [
        Arg::Int(5),
        Arg::Str("it's".into()),
        Arg::Bool(true),
        Arg::Null,
        Arg::Bytes(vec![0, 255]),
    ];
    assert_eq!(
        sanitize_sql("SELECT $1, $2, $3, $4, $5", &args).unwrap(),
        r"SELECT  '5' ,  'it''s' ,  't' ,  null ,  '\x00ff' "
    );
}

#[test]
fn sanitize_leaves_strings_and_comments_alone() {
    let args = [Arg::Int(1)];
    for sql in [
        "'$1' $1",
        "\"$1\" $1",
        "E'\\'$1' $1",
        "-- $1\n$1",
        "/* $1 /* $1 */ */ $1",
    ] {
        let out = sanitize_sql(sql, &args).unwrap();
        assert_eq!(out.matches("$1").count(), sql.matches("$1").count() - 1, "{out}");
    }
}

#[test]
fn sanitize_refuses_what_pgx_refuses() {
    assert_contains!(
        sanitize_sql("SELECT $2", &[Arg::Int(1)]).unwrap_err(),
        "insufficient arguments"
    );
    assert_contains!(
        sanitize_sql("SELECT 1", &[Arg::Int(1)]).unwrap_err(),
        "unused argument: 0"
    );
}

#[test]
fn time_argument_is_written_in_utc() {
    let t = time_fixed(2026, 10, 18, 9, 30, 0, 123456000, -3 * 3600);
    assert_eq!(
        sanitize_sql("$1", &[Arg::Time(t)]).unwrap(),
        " '2026-10-18 12:30:00.123456Z' "
    );
}

fn cell(oid: u32, format: i16, bytes: &[u8]) -> Cell {
    Cell {
        oid,
        format,
        bytes: Some(bytes.to_vec()),
    }
}

// The scan plans of pgx: a value of the wrong type is an error, not a coercion.
#[test]
fn scan_refuses_the_wrong_type() {
    assert!(
        i64::from_cell(&cell(25, TEXT_FORMAT, b"1")).is_err(),
        "text into an int"
    );
    assert!(
        bool::from_cell(&cell(20, TEXT_FORMAT, b"1")).is_err(),
        "int8 into a bool"
    );
    assert!(
        i64::from_cell(&cell(16, TEXT_FORMAT, b"t")).is_err(),
        "bool into an int"
    );
    assert!(
        i64::from_cell(&cell(26, TEXT_FORMAT, b"1")).is_err(),
        "oid into an int64"
    );
    assert!(
        Time::from_cell(&cell(25, TEXT_FORMAT, b"2024-01-02")).is_err(),
        "text into a time"
    );
    assert!(
        Vec::<String>::from_cell(&cell(25, TEXT_FORMAT, b"{a}")).is_err(),
        "text into a slice"
    );
    assert!(
        i32::from_cell(&cell(20, TEXT_FORMAT, b"3000000000")).is_err(),
        "out of range"
    );
    assert!(
        i64::from_cell(&Cell {
            oid: 20,
            format: 0,
            bytes: None
        })
        .is_err(),
        "NULL into an int"
    );
    assert!(
        String::from_cell(&Cell {
            oid: 25,
            format: 0,
            bytes: None
        })
        .is_err(),
        "NULL into a string"
    );
}

#[test]
fn scan_accepts_what_pgx_accepts() {
    assert_eq!(
        i64::from_cell(&cell(20, BINARY_FORMAT, &7i64.to_be_bytes())).unwrap(),
        7
    );
    assert_eq!(
        i64::from_cell(&cell(23, BINARY_FORMAT, &7i32.to_be_bytes())).unwrap(),
        7
    );
    assert_eq!(
        i64::from_cell(&cell(701, BINARY_FORMAT, &7f64.to_be_bytes())).unwrap(),
        7
    );
    assert_eq!(u32::from_cell(&cell(26, TEXT_FORMAT, b"1259")).unwrap(), 1259);
    assert_eq!(f64::from_cell(&cell(20, TEXT_FORMAT, b"2")).unwrap(), 2.0);
    assert_eq!(
        String::from_cell(&cell(20, BINARY_FORMAT, &7i64.to_be_bytes())).unwrap(),
        "7"
    );
    assert_eq!(
        i64::from_cell(&cell(99999, TEXT_FORMAT, b"7")).unwrap(),
        7,
        "a type pgx does not know"
    );
    assert_eq!(
        Vec::<u8>::from_cell(&cell(17, TEXT_FORMAT, b"\\x00ff")).unwrap(),
        [0, 255]
    );
    assert!(bool::from_cell(&cell(16, TEXT_FORMAT, b"t")).unwrap());
    assert_eq!(
        Vec::<Option<String>>::from_cell(&cell(1009, TEXT_FORMAT, br#"{a,"b,c",NULL,"d\"e"}"#)).unwrap(),
        [
            Some("a".to_string()),
            Some("b,c".to_string()),
            None,
            Some("d\"e".to_string())
        ]
    );
}

#[test]
fn query_row_with_no_row_is_no_rows() {
    let conn = connect(&start_server());
    let err = conn.query_row("SELECT 1 WHERE 1 = 0", &[]).scan::<i64>().unwrap_err();
    assert!(err.is_no_rows(), "{err:?}");
}

#[test]
fn scan_counts_columns() {
    let conn = connect(&start_server());
    assert_contains!(
        conn.query_row("SELECT 1, 2", &[]).scan::<i64>().unwrap_err(),
        "number of field descriptions"
    );
}

#[test]
fn extended_query_counts_arguments() {
    let conn = connect_extended(&start_server());
    assert_contains!(
        conn.query("SELECT $1::int", &[]).unwrap_err(),
        "expected 1 arguments, got 0"
    );
}

#[test]
fn modes_choose_formats_as_pgx() {
    let addr = start_server();
    let simple = connect(&addr);
    let extended = connect_extended(&addr);
    let format = |c: &Conn, sql: &str| c.query(sql, &[]).unwrap().fields[0].format;
    assert_eq!(format(&simple, "SELECT 1"), TEXT_FORMAT);
    assert_eq!(format(&extended, "SELECT 1"), BINARY_FORMAT);
    assert_eq!(format(&extended, "SELECT 'a'"), TEXT_FORMAT);
}

// The statement cache is pgx's: a failed statement is dropped, and fails again
// by asking the server rather than from memory.
#[test]
fn failed_statement_leaves_the_cache() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int)");
    let q = "SELECT id FROM t WHERE id = $1";
    assert!(conn.query(q, &[&1]).unwrap().is_empty());
    must_exec(&conn, "DROP TABLE t");
    conn.query(q, &[&1]).unwrap_err();
    must_exec(&conn, "CREATE TABLE t (id int)");
    must_exec(&conn, "INSERT INTO t VALUES (1)");
    assert_eq!(conn.query(q, &[&1]).unwrap().len(), 1);
}

#[test]
fn quoting_round_trips() {
    let raw = "a\"b\\c\n\t\x07\x00é\u{ffff}\u{1f600}".as_bytes();
    let mut with_bad = raw.to_vec();
    with_bad.push(0xff);
    let q = quote_bytes(&with_bad);
    assert_eq!(q, "\"a\\\"b\\\\c\\n\\t\\a\\x00é\\uffff😀\\xff\"");
    assert_eq!(unquote_bytes(&q), with_bad);
}

// The environment is the one server's, not the whole process's.
#[test]
fn env_reaches_only_its_server() {
    let locked = start_server_env(Env::password("pw"));
    let open = start_server();
    assert!(dial(&locked, "wrong").is_err());
    assert!(dial(&locked, "pw").is_ok());
    assert!(dial(&open, "anything").is_ok());
}
