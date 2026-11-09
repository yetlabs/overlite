//! A LANGUAGE sql function is inlined into the statement that calls it, and what its
//! body reads is held to the privileges Postgres checks, whose answers (17) these tests
//! repeat: a SECURITY INVOKER body (the default) reads with the caller's privileges and
//! through the caller's policies, a SECURITY DEFINER body with its owner's, and calling
//! either needs EXECUTE, which PUBLIC holds until it is revoked.

use crate::common::*;

/// The rows of sql, or its SQLSTATE, through the simple protocol and the extended one.
fn outcomes(conn: &Conn, sql: &str) -> [(&'static str, std::result::Result<Vec<String>, String>); 2] {
    let simple = conn
        .pg_exec(sql)
        .map(|rs| {
            rs.last()
                .map(|r| {
                    r.rows
                        .iter()
                        .map(|row| {
                            row.iter()
                                .map(|c| {
                                    c.as_ref()
                                        .map_or("NULL".into(), |b| String::from_utf8_lossy(b).into_owned())
                                })
                                .collect::<Vec<_>>()
                                .join("|")
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .map_err(|e| e.code().to_string());
    let extended = conn
        .exec_params::<&[u8]>(sql, &[], &[], &[], &[])
        .map(|r| {
            r.rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|c| {
                            c.as_ref()
                                .map_or("NULL".into(), |b| String::from_utf8_lossy(b).into_owned())
                        })
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect()
        })
        .map_err(|e| e.code().to_string());
    [("simple", simple), ("extended", extended)]
}

/// Refused with 42501 in both protocols; what came back otherwise is the leak.
#[track_caller]
fn refused(conn: &Conn, sql: &str) {
    for (path, got) in outcomes(conn, sql) {
        assert_eq!(got, Err("42501".to_string()), "{path}: {sql}");
    }
}

/// The rows of sql, the same in both protocols.
#[track_caller]
fn rows(conn: &Conn, sql: &str) -> Vec<String> {
    let [(_, simple), (_, extended)] = outcomes(conn, sql);
    let simple = simple.unwrap_or_else(|e| panic!("simple: {sql}: {e}"));
    let extended = extended.unwrap_or_else(|e| panic!("extended: {sql}: {e}"));
    assert_eq!(simple, extended, "{sql}");
    simple
}

/// A server with the superuser postgres, roles alice and bob, a table secret no one
/// else may read and a table pub both may use.
fn setup() -> (Server, Conn, Conn, Conn) {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'b'");
    must_exec(&admin, "CREATE TABLE secret (id int, s text)");
    must_exec(&admin, "INSERT INTO secret VALUES (1, 'topsecret')");
    must_exec(&admin, "CREATE TABLE pub (id int, v text)");
    must_exec(&admin, "INSERT INTO pub VALUES (1, 'x')");
    must_exec(&admin, "GRANT ALL ON pub TO alice, bob");
    let alice = connect_as(&addr, "alice", "a");
    let bob = connect_as(&addr, "bob", "b");
    (addr, admin, alice, bob)
}

// The report: a role with no grant on secret read it through a function of its own.
#[test]
fn an_invoker_body_reads_with_the_callers_privileges() {
    let (_addr, admin, alice, _bob) = setup();
    refused(&alice, "SELECT s FROM secret");
    must_exec(
        &alice,
        "CREATE FUNCTION peek() RETURNS text LANGUAGE sql AS 'SELECT s FROM secret LIMIT 1'",
    );
    must_exec(
        &alice,
        "CREATE FUNCTION peek2() RETURNS text LANGUAGE sql AS 'SELECT peek()'",
    );
    must_exec(
        &alice,
        "CREATE FUNCTION peek3(x text) RETURNS text LANGUAGE sql AS $$ SELECT x || peek2() $$",
    );
    must_exec(
        &alice,
        "CREATE FUNCTION all_secret() RETURNS SETOF secret LANGUAGE sql AS $$ SELECT * FROM secret $$",
    );
    for sql in [
        "SELECT peek()",
        "SELECT peek2()",
        "SELECT peek3('>')",
        "SELECT upper(peek3(peek()))",
        "SELECT * FROM all_secret()",
        "SELECT (SELECT peek())",
        "SELECT * FROM pub WHERE v <> peek()",
        "SELECT * FROM pub WHERE peek() IS NOT NULL",
        "SELECT * FROM pub ORDER BY peek()",
        "WITH x AS (SELECT peek() AS p) SELECT p FROM x",
        "SELECT id FROM pub WHERE EXISTS (SELECT 1 FROM pub p2 WHERE p2.v <> peek2())",
        "INSERT INTO pub VALUES (2, peek())",
        "INSERT INTO pub SELECT 3, peek()",
        "UPDATE pub SET v = peek()",
        "UPDATE pub SET v = 'y' WHERE v <> peek()",
        "DELETE FROM pub WHERE v <> peek()",
        "INSERT INTO pub VALUES (4, 'z') RETURNING peek()",
    ] {
        refused(&alice, sql);
    }
    // Nothing was written on the way.
    assert_eq!(rows(&admin, "SELECT id || v FROM pub ORDER BY id"), ["1x"]);

    // The other routes to a statement.
    let mut out = Vec::new();
    let err = alice.copy_to(&mut out, "COPY (SELECT peek()) TO STDOUT").unwrap_err();
    assert_sqlstate(&err, "42501");
    assert_eq!(String::from_utf8_lossy(&out), "");
    must_exec(&alice, "PREPARE q AS SELECT peek()");
    assert_sqlstate(&alice.exec("EXECUTE q", &[]).unwrap_err(), "42501");
    must_exec(&alice, "BEGIN");
    assert_sqlstate(
        &alice.exec("DECLARE c CURSOR FOR SELECT peek()", &[]).unwrap_err(),
        "42501",
    );
    must_exec(&alice, "ROLLBACK");

    // With the grant the same calls answer.
    must_exec(&admin, "GRANT SELECT ON secret TO alice");
    assert_eq!(rows(&alice, "SELECT peek()"), ["topsecret"]);
    assert_eq!(rows(&alice, "SELECT peek3('>')"), [">topsecret"]);
    assert_eq!(rows(&alice, "SELECT id FROM all_secret()"), ["1"]);
    assert_eq!(rows(&alice, "SELECT id FROM pub WHERE v <> peek()"), ["1"]);

    // A column grant holds for the columns the body reads.
    must_exec(&admin, "REVOKE SELECT ON secret FROM alice");
    must_exec(&admin, "GRANT SELECT (id) ON secret TO alice");
    must_exec(
        &alice,
        "CREATE FUNCTION ids() RETURNS int LANGUAGE sql AS 'SELECT max(id) FROM secret'",
    );
    assert_eq!(rows(&alice, "SELECT ids()"), ["1"]);
    refused(&alice, "SELECT peek()");
}

// A body is read through the caller's policies, wherever the call is.
#[test]
fn an_invoker_body_reads_through_the_callers_policies() {
    let (_addr, admin, alice, bob) = setup();
    must_exec(&admin, "CREATE TABLE r (owner text, v text)");
    must_exec(
        &admin,
        "INSERT INTO r VALUES ('alice', 'a1'), ('bob', 'b1'), ('bob', 'b2')",
    );
    must_exec(&admin, "ALTER TABLE r ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY own ON r USING (owner = current_user)");
    must_exec(&admin, "GRANT SELECT ON r TO alice, bob");
    must_exec(
        &admin,
        "CREATE FUNCTION rc() RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM r'",
    );
    must_exec(
        &admin,
        "CREATE FUNCTION rvs() RETURNS SETOF text LANGUAGE sql AS 'SELECT v FROM r ORDER BY v'",
    );
    assert_eq!(rows(&alice, "SELECT rc()"), ["1"]);
    assert_eq!(rows(&bob, "SELECT rc()"), ["2"]);
    assert_eq!(rows(&admin, "SELECT rc()"), ["3"]);
    assert_eq!(rows(&bob, "SELECT * FROM rvs()"), ["b1", "b2"]);
    assert_eq!(rows(&alice, "SELECT id FROM pub WHERE rc() = 1"), ["1"]);
    must_exec(&alice, "UPDATE pub SET v = rc()::text");
    assert_eq!(rows(&admin, "SELECT v FROM pub"), ["1"]);
    must_exec(&bob, "DELETE FROM pub WHERE v::bigint < rc()");
    assert_eq!(rows(&admin, "SELECT count(*) FROM pub"), ["0"]);
    must_exec(&alice, "INSERT INTO pub SELECT 5, v FROM rvs()");
    assert_eq!(rows(&admin, "SELECT v FROM pub"), ["a1"]);
    // A subquery of an UPDATE or DELETE reads through the policies too.
    must_exec(&alice, "UPDATE pub SET v = (SELECT count(*) FROM r)::text");
    assert_eq!(rows(&admin, "SELECT v FROM pub"), ["1"]);
}

// SECURITY DEFINER: the body reads with its owner's privileges and policies, the
// arguments with the caller's, and EXECUTE decides who may call it.
#[test]
fn a_definer_body_reads_with_its_owners_privileges() {
    let (_addr, admin, alice, bob) = setup();
    must_exec(
        &admin,
        "CREATE FUNCTION adm() RETURNS text LANGUAGE sql SECURITY DEFINER AS 'SELECT s FROM secret LIMIT 1'",
    );
    must_exec(
        &admin,
        "CREATE FUNCTION adm_arg(x text) RETURNS text LANGUAGE sql SECURITY DEFINER AS 'SELECT s || x FROM secret LIMIT 1'",
    );
    must_exec(
        &admin,
        "CREATE FUNCTION who() RETURNS text LANGUAGE sql SECURITY DEFINER AS 'SELECT current_user'",
    );
    must_exec(
        &admin,
        "CREATE FUNCTION inv_who() RETURNS text LANGUAGE sql AS 'SELECT current_user'",
    );
    assert_eq!(rows(&alice, "SELECT adm()"), ["topsecret"]);
    assert_eq!(rows(&alice, "SELECT adm_arg('!')"), ["topsecret!"]);
    assert_eq!(rows(&alice, "SELECT id FROM pub WHERE adm() = 'topsecret'"), ["1"]);
    assert_eq!(rows(&alice, "SELECT who() || ',' || inv_who()"), ["postgres,alice"]);
    // An argument is the caller's to read.
    refused(&alice, "SELECT adm_arg((SELECT s FROM secret))");

    // Policies apply as the owner's: postgres bypasses them.
    must_exec(&admin, "CREATE TABLE r (owner text, v text)");
    must_exec(&admin, "INSERT INTO r VALUES ('alice', 'a1'), ('bob', 'b1')");
    must_exec(&admin, "ALTER TABLE r ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY own ON r USING (owner = current_user)");
    must_exec(&admin, "GRANT SELECT ON r TO alice, bob");
    must_exec(
        &admin,
        "CREATE FUNCTION rc() RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM r'",
    );
    must_exec(
        &admin,
        "CREATE FUNCTION rc_def() RETURNS bigint LANGUAGE sql SECURITY DEFINER AS 'SELECT count(*) FROM r'",
    );
    assert_eq!(rows(&alice, "SELECT rc() || ',' || rc_def()"), ["1,2"]);

    // EXECUTE, PUBLIC's until revoked.
    must_exec(&admin, "REVOKE EXECUTE ON FUNCTION adm() FROM PUBLIC");
    must_exec(&admin, "GRANT EXECUTE ON FUNCTION adm() TO bob");
    let err = alice.exec("SELECT adm()", &[]).unwrap_err();
    assert_pg_error(&err, "42501", "permission denied for function adm");
    refused(&alice, "SELECT id FROM pub WHERE adm() IS NOT NULL");
    assert_eq!(rows(&bob, "SELECT adm()"), ["topsecret"]);

    // Only the owner changes it.
    for sql in [
        "CREATE OR REPLACE FUNCTION adm() RETURNS text LANGUAGE sql SECURITY DEFINER AS 'SELECT ''pwned'''",
        "DROP FUNCTION adm()",
        "ALTER FUNCTION adm() SECURITY INVOKER",
        "ALTER FUNCTION adm() OWNER TO alice",
    ] {
        let err = alice.exec(sql, &[]).unwrap_err();
        assert_pg_error(&err, "42501", "must be owner of function adm");
    }
    assert_eq!(rows(&bob, "SELECT adm()"), ["topsecret"]);

    // Its owner's privileges are the new owner's.
    must_exec(&admin, "ALTER FUNCTION adm() OWNER TO alice");
    must_exec(&admin, "GRANT EXECUTE ON FUNCTION adm() TO alice");
    refused(&alice, "SELECT adm()");
    refused(&bob, "SELECT adm()");
    refused(&admin, "SELECT adm()");

    // A definer body of alice's reads what alice may, for anyone.
    must_exec(
        &alice,
        "CREATE FUNCTION pubv() RETURNS text LANGUAGE sql SECURITY DEFINER AS 'SELECT v FROM pub LIMIT 1'",
    );
    must_exec(&admin, "REVOKE ALL ON pub FROM bob");
    refused(&bob, "SELECT v FROM pub");
    assert_eq!(rows(&bob, "SELECT pubv()"), ["x"]);
    // And a body calling an invoker function calls it as alice.
    must_exec(
        &alice,
        "CREATE FUNCTION inner_v() RETURNS text LANGUAGE sql AS 'SELECT v FROM pub LIMIT 1'",
    );
    must_exec(
        &alice,
        "CREATE FUNCTION outer_v() RETURNS text LANGUAGE sql SECURITY DEFINER AS 'SELECT inner_v()'",
    );
    assert_eq!(rows(&bob, "SELECT outer_v()"), ["x"]);
    refused(&bob, "SELECT inner_v()");
    // alice revokes EXECUTE on her invoker function from PUBLIC: bob may no longer call it.
    must_exec(&alice, "REVOKE EXECUTE ON FUNCTION inner_v() FROM PUBLIC");
    let err = bob.exec("SELECT inner_v()", &[]).unwrap_err();
    assert_pg_error(&err, "42501", "permission denied for function inner_v");
    assert_eq!(rows(&bob, "SELECT outer_v()"), ["x"]);
}

// A body the check cannot read is refused to a caller that does not own the
// function, rather than run unchecked.
#[test]
fn a_body_that_cannot_be_checked_is_refused() {
    let (_addr, _admin, alice, bob) = setup();
    must_exec(
        &alice,
        "CREATE FUNCTION two_statements() RETURNS int LANGUAGE sql AS 'SELECT 1; SELECT 2'",
    );
    // The extended protocol's Describe meets SQLite's refusal of the body first.
    let [(_, simple), (_, extended)] = outcomes(&bob, "SELECT two_statements()");
    assert_eq!(simple, Err("0A000".to_string()));
    assert!(extended.is_err(), "{extended:?}");
}

// A function named as a built-in one is not what a call of that name runs, as in
// Postgres, where pg_catalog comes first: it once was, the server's own queries
// included, so a role could answer its own privilege checks.
#[test]
fn a_function_named_as_a_builtin_is_not_called() {
    let (_addr, admin, alice, _bob) = setup();
    must_exec(
        &alice,
        "CREATE FUNCTION lower(x text) RETURNS text LANGUAGE sql AS $$ SELECT 'public' $$",
    );
    // A keyword is never a call: `x IN (a, b)` is not a call of "in".
    must_exec(
        &alice,
        "CREATE FUNCTION \"in\"(a text, b text) RETURNS boolean LANGUAGE sql AS $$ SELECT false $$",
    );
    refused(&alice, "SELECT s FROM secret");
    assert_eq!(rows(&alice, "SELECT lower('ABC')"), ["abc"]);
    assert_eq!(rows(&admin, "SELECT lower('ABC')"), ["abc"]);
    assert_eq!(
        rows(&admin, "SELECT CASE WHEN 'a' IN ('b', 'a') THEN 'yes' ELSE 'no' END"),
        ["yes"]
    );
}

// A DEFAULT or CHECK cannot hold a call that reads a table: SQLite takes no subquery
// there, so the table is refused before any row is written.
#[test]
fn a_default_or_check_cannot_call_a_reading_function() {
    let (_addr, _admin, alice, _bob) = setup();
    must_exec(
        &alice,
        "CREATE FUNCTION peek() RETURNS text LANGUAGE sql AS 'SELECT s FROM secret LIMIT 1'",
    );
    alice
        .exec("CREATE TABLE d (a int, b text DEFAULT peek())", &[])
        .unwrap_err();
    alice
        .exec("CREATE TABLE c (a text CHECK (a <> peek()))", &[])
        .unwrap_err();
    alice
        .exec(
            "CREATE TABLE g (a text, b text GENERATED ALWAYS AS (peek()) STORED)",
            &[],
        )
        .unwrap_err();
}

// A view reads its tables with its owner's privileges, so a view of alice's over a
// table she cannot read reads nothing, for her or for whoever it is granted to.
#[test]
fn a_view_reads_with_its_owners_privileges() {
    let (_addr, admin, alice, bob) = setup();
    must_exec(
        &alice,
        "CREATE FUNCTION peek() RETURNS text LANGUAGE sql AS 'SELECT s FROM secret LIMIT 1'",
    );
    must_exec(&alice, "CREATE VIEW av AS SELECT * FROM secret");
    must_exec(&alice, "CREATE VIEW fv AS SELECT peek() AS p");
    must_exec(&alice, "CREATE VIEW nested AS SELECT * FROM av");
    refused(&alice, "SELECT * FROM av");
    refused(&alice, "SELECT * FROM fv");
    refused(&alice, "SELECT * FROM nested");
    must_exec(&alice, "GRANT SELECT ON av, fv, nested TO bob");
    let err = bob.exec("DROP VIEW av", &[]).unwrap_err();
    assert_sqlstate(&err, "42501");
    refused(&bob, "SELECT * FROM av");
    refused(&bob, "SELECT * FROM fv");
    refused(&bob, "SELECT * FROM nested");
    // A view of postgres' reads what postgres may, for whoever it is granted to.
    must_exec(&admin, "CREATE VIEW sv AS SELECT * FROM secret");
    refused(&alice, "SELECT * FROM sv");
    must_exec(&admin, "GRANT SELECT ON sv TO alice");
    assert_eq!(rows(&alice, "SELECT s FROM sv"), ["topsecret"]);
    // Once alice may read secret, so may her views.
    must_exec(&admin, "GRANT SELECT ON secret TO alice");
    assert_eq!(rows(&bob, "SELECT s FROM nested"), ["topsecret"]);
    assert_eq!(rows(&alice, "SELECT p FROM fv"), ["topsecret"]);
}
