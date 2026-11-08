//! `E''` strings are read as Postgres' scanner reads them. Every expected value is what
//! Postgres 17 answered: the escapes used to reach the client as backslashes.

use crate::common::*;

fn pair() -> (Conn, Conn) {
    let server = start_server();
    (connect(&server), connect_extended(&server))
}

#[test]
fn escape_strings_are_decoded() {
    let (conn, ext) = pair();
    for (sql, want) in [
        (r"SELECT E'a\nb'", "a\nb"),
        (r"SELECT e'a\tb'", "a\tb"),
        (r"SELECT E'a\\b'", r"a\b"),
        (r"SELECT E'it\'s'", "it's"),
        (r"SELECT E'a''b'", "a'b"),
        (r"SELECT E'\b\f\r'", "\x08\x0c\r"),
        (r"SELECT E'\x41\x4a\x4A'", "AJJ"),
        (r"SELECT E'\x4'", "\x04"),
        (r"SELECT E'\101\60\7'", "A0\x07"),
        (r"SELECT E'Aé'", "Aé"),
        (r"SELECT E'\U0001F600'", "😀"),
        (r"SELECT E'😀'", "😀"),
        (r"SELECT E'\xc3\xa9'", "é"),
        (r"SELECT E'\q\z\?'", "qz?"),
        ("SELECT E'abc'\n  'def\\n'", "abcdef\n"),
        ("SELECT 'abc'\n  'def'", "abcdef"),
        (r"SELECT U&'d\0061t\+000061'", "data"),
        (r"SELECT U&'d!0061t' UESCAPE '!'", "dat"),
    ] {
        assert_eq!(scan_str(&conn, sql), want, "simple: {sql}");
        assert_eq!(scan_str(&ext, sql), want, "extended: {sql}");
    }
}

#[test]
fn bad_escapes_are_refused() {
    let (conn, ext) = pair();
    for (sql, code) in [
        (r"SELECT E'\u12'", "22025"),
        (r"SELECT E'\U0011FFFF'", "42601"),
        (r"SELECT E'\u0000'", "42601"),
        (r"SELECT E'\uD83D'", "42601"),
        (r"SELECT E'\0'", "22021"),
        (r"SELECT E'\x00'", "22021"),
        (r"SELECT E'\400'", "22021"),
        (r"SELECT E'\xff'", "22021"),
    ] {
        for c in [&conn, &ext] {
            let got = c.exec(sql, &[]);
            assert_eq!(
                got.as_ref().map_err(WireError::code).err(),
                Some(code),
                "{sql}: {got:?}"
            );
        }
    }
}

// An escape does its work wherever the string goes.
#[test]
fn escape_strings_in_statements() {
    let (conn, ext) = pair();
    must_exec(&conn, r"CREATE TABLE t (a text)");
    must_exec(&conn, r"INSERT INTO t VALUES (E'x\ty')");
    assert_eq!(scan_str(&conn, "SELECT a FROM t"), "x\ty");
    assert_eq!(scan_str(&ext, r"SELECT count(*)::text FROM t WHERE a = E'x\ty'"), "1");
    assert_eq!(
        scan_str(&conn, r"SELECT count(*)::text FROM t WHERE a LIKE E'x\t%'"),
        "1"
    );
    assert_eq!(scan_str(&conn, r"SELECT count(*)::text FROM t WHERE a ~ E'\\w'"), "1");
    // A plain string keeps its backslashes, and no prefix is read in a word.
    assert_eq!(scan_str(&conn, r"SELECT 'a\nb'"), r"a\nb");
    must_exec(&conn, r#"CREATE TABLE e (type text)"#);
    must_exec(&conn, r"INSERT INTO e VALUES ('x\y')");
    assert_eq!(scan_str(&conn, "SELECT type FROM e"), r"x\y");
    // Not in a comment or a quoted name either.
    assert_eq!(scan_str(&conn, "SELECT 'a' -- E'\\n'"), "a");
    must_exec(&conn, r#"CREATE TABLE "E'x" (a int)"#);
    must_exec(&conn, r#"SELECT * FROM "E'x""#);
}
