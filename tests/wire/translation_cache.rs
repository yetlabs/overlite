use crate::common::*;

// The translation cache (crates/postgres/src/rewrite/cache.rs) keeps what a
// statement's text became, for every connection of the server. Each test sends the
// same text before and after a change that has to show in its answer: the cache
// must answer for the text and nothing else, or be told.

// search_path is applied after the translation, on every execution: the same text
// reads another schema's table once the path moves, in this connection and in one
// that has another path.
#[test]
fn a_cached_statement_follows_search_path() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE SCHEMA s1");
    must_exec(&conn, "CREATE SCHEMA s2");
    must_exec(&conn, "CREATE TABLE s1.t (v text)");
    must_exec(&conn, "CREATE TABLE s2.t (v text)");
    must_exec(&conn, "INSERT INTO s1.t VALUES ('one')");
    must_exec(&conn, "INSERT INTO s2.t VALUES ('two')");
    // Off the fast path and inside the AST slice: COALESCE and ||.
    let q = "SELECT coalesce(v, '?') || '!' FROM t ORDER BY 1";
    must_exec(&conn, "SET search_path TO s1");
    assert_eq!(scan_str(&conn, q), "one!");
    assert_eq!(scan_str(&conn, q), "one!");
    must_exec(&conn, "SET search_path TO s2");
    assert_eq!(scan_str(&conn, q), "two!");
    let other = connect(&server);
    must_exec(&other, "SET search_path TO s1");
    assert_eq!(scan_str(&other, q), "one!");
    assert_eq!(scan_str(&conn, q), "two!");
}

// The session's TimeZone is read on every execution too: a cached statement renders
// a timestamptz, and reads one written without an offset, in the zone set now.
#[test]
fn a_cached_statement_follows_the_time_zone() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE z (id int, ts timestamptz)");
    must_exec(&conn, "INSERT INTO z VALUES (1, '2024-01-01 12:00:00+00')");
    let shown = "SELECT ts FROM z WHERE id + 0 = 1 ORDER BY 1";
    let found = "SELECT count(*) FROM z WHERE ts = '2024-01-01 12:00:00' AND id + 0 = 1";
    must_exec(&conn, "SET TimeZone TO 'UTC'");
    assert_eq!(scan_str(&conn, shown), "2024-01-01 12:00:00+00");
    assert_eq!(seq_val(&conn, found), 1);
    must_exec(&conn, "SET TimeZone TO 'America/Sao_Paulo'");
    assert_eq!(scan_str(&conn, shown), "2024-01-01 09:00:00-03");
    assert_eq!(seq_val(&conn, found), 0);
}

// What CREATE COLLATION makes is part of a translation (a COLLATE clause names it
// or is dropped), so creating or dropping one retires what the cache holds.
#[test]
fn a_cached_statement_sees_a_collation_made_after_it() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE cc (name text)");
    must_exec(&conn, "INSERT INTO cc VALUES ('ANDREW')");
    let q = "SELECT count(*) FROM cc WHERE name = 'andrew' COLLATE nd2";
    // Unknown, the clause is dropped: bytes compare.
    assert_eq!(seq_val(&conn, q), 0);
    must_exec(
        &conn,
        "CREATE COLLATION nd2 (provider = icu, locale = 'und-u-ks-level2', deterministic = false)",
    );
    assert_eq!(seq_val(&conn, q), 1);
    let other = connect(&server);
    assert_eq!(seq_val(&other, q), 1);
    must_exec(&conn, "DROP COLLATION nd2");
    assert_eq!(seq_val(&conn, q), 0);
}

// An enum's labels are written into the table's DDL by the session before the
// translation, and DDL is not cached: the same CREATE TABLE after ALTER TYPE ...
// ADD VALUE takes the new label.
#[test]
fn the_same_ddl_after_an_enum_grows_takes_the_new_label() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TYPE mood AS ENUM ('sad', 'ok')");
    let create = "CREATE TABLE m (id int, v mood)";
    must_exec(&conn, create);
    assert!(conn.exec("INSERT INTO m VALUES (1, 'happy')", &[]).is_err());
    must_exec(&conn, "DROP TABLE m");
    must_exec(&conn, "ALTER TYPE mood ADD VALUE 'happy'");
    must_exec(&conn, create);
    must_exec(&conn, "INSERT INTO m VALUES (1, 'happy')");
    assert_eq!(
        scan_str(&conn, "SELECT coalesce(v::text, '?') FROM m WHERE id = 1"),
        "happy"
    );
}

// The catalog is read after the translation as well: a cached statement over a table
// dropped and made again with another column type answers by the new type.
#[test]
fn a_cached_statement_follows_a_table_made_again() {
    let conn = connect(&start_server());
    let q = "SELECT coalesce(max(a), 0) + 1 FROM r";
    must_exec(&conn, "CREATE TABLE r (a int)");
    must_exec(&conn, "INSERT INTO r VALUES (41)");
    assert_eq!(scan_str(&conn, q), "42");
    must_exec(&conn, "DROP TABLE r");
    must_exec(&conn, "CREATE TABLE r (a numeric)");
    must_exec(&conn, "INSERT INTO r VALUES (1.5)");
    assert_eq!(scan_str(&conn, q), "2.5");
}
