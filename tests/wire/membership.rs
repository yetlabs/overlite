use crate::common::*;

// role_membership: GRANT role TO role confers the role's privileges to its
// members (default INHERIT), transitively, and REVOKE takes them away.
#[test]
fn role_membership() {
    let addr = start_server_env(Env::password("adminpw"));

    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE reader");
    must_exec(&admin, "CREATE ROLE staff"); // staff will be a member of reader
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE t (id int)");
    must_exec(&admin, "INSERT INTO t VALUES (1)");
    must_exec(&admin, "GRANT SELECT ON t TO reader");

    let alice = connect_as(&addr, "alice", "a");

    // Not a member yet -> denied.
    alice
        .query_row("SELECT count(*) FROM t", &[])
        .scan::<i64>()
        .unwrap_err();

    // Direct membership: alice inherits reader's SELECT.
    must_exec(&admin, "GRANT reader TO alice");
    let n: i64 = alice.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);
    alice
        .exec("INSERT INTO t VALUES (2)", &[])
        .expect_err("reader only has SELECT");

    // REVOKE removes the inheritance.
    must_exec(&admin, "REVOKE reader FROM alice");
    alice
        .query_row("SELECT count(*) FROM t", &[])
        .scan::<i64>()
        .unwrap_err();

    // Transitive: alice -> staff -> reader.
    must_exec(&admin, "GRANT reader TO staff");
    must_exec(&admin, "GRANT staff TO alice");
    let n: i64 = alice.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

// role_membership_no_inherit: a NOINHERIT member doesn't get privileges
// automatically, but can still reach them by SET ROLE.
#[test]
fn role_membership_no_inherit() {
    let addr = start_server_env(Env::password("adminpw"));

    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE reader");
    must_exec(&admin, "CREATE ROLE bob LOGIN NOINHERIT PASSWORD 'b'");
    must_exec(&admin, "CREATE TABLE t (id int)");
    must_exec(&admin, "INSERT INTO t VALUES (1)");
    must_exec(&admin, "GRANT SELECT ON t TO reader");
    must_exec(&admin, "GRANT reader TO bob");

    let bob = connect_as(&addr, "bob", "b");

    // NOINHERIT: no automatic privilege from reader.
    bob.query_row("SELECT count(*) FROM t", &[]).scan::<i64>().unwrap_err();

    // But after SET ROLE reader, the privilege applies.
    bob.exec("SET ROLE reader", &[]).unwrap();
    let n: i64 = bob.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);
}

fn members_of(conn: &Conn, role: &str) -> Vec<String> {
    let rows = conn
        .query(
            "SELECT member, admin_option FROM _overlite_memberships WHERE roleof = $1 ORDER BY member",
            &[&role],
        )
        .unwrap();
    rows.iter()
        .map(|r| format!("{}:{}", r.cell(0).text(), r.cell(1).text()))
        .collect()
}

// A quoted role name is a name, whatever words it spells.
#[test]
fn role_names_spelling_keywords() {
    let conn = connect_extended(&start_server());
    for r in [
        "to",
        "on",
        "from",
        "with admin",
        "x granted by y",
        "x cascade",
        "admin option for",
        "staff",
    ] {
        conn.exec("INSERT INTO _overlite_roles (rolname) VALUES ($1)", &[&r])
            .unwrap();
    }

    must_exec(&conn, "GRANT \"to\" TO staff");
    assert_eq!(members_of(&conn, "to"), ["staff:0"]);

    must_exec(&conn, "GRANT \"on\" TO staff");
    assert_eq!(members_of(&conn, "on"), ["staff:0"], "no ON clause: no table in it");

    must_exec(&conn, "GRANT staff TO \"with admin\"");
    assert_eq!(members_of(&conn, "staff"), ["with admin:0"], "the name, and no option");

    must_exec(
        &conn,
        "GRANT staff TO \"x granted by y\", \"x cascade\" WITH ADMIN OPTION",
    );
    assert_eq!(
        members_of(&conn, "staff"),
        ["with admin:0", "x cascade:1", "x granted by y:1"]
    );

    must_exec(&conn, "GRANT \"from\" TO staff");
    must_exec(&conn, "REVOKE \"from\" FROM staff");
    assert_eq!(members_of(&conn, "from"), [] as [std::string::String; 0]);

    must_exec(&conn, "GRANT \"admin option for\" TO staff WITH ADMIN OPTION");
    must_exec(&conn, "REVOKE \"admin option for\" FROM staff");
    assert!(
        members_of(&conn, "admin option for").is_empty(),
        "the role, not the clause"
    );

    must_exec(&conn, "GRANT staff TO \"to\" WITH ADMIN OPTION");
    must_exec(&conn, "REVOKE ADMIN OPTION FOR staff FROM \"to\"");
    assert!(members_of(&conn, "staff").contains(&"to:0".to_string()));
}

// A backslash is no escape in a quoted name, so this statement's quotes pair up across
// its TO. Nothing is left to read as a grant, which Postgres calls a syntax error.
#[test]
fn role_grant_with_unbalanced_quoting_is_an_error() {
    let conn = connect(&start_server());
    let sql = r#"GRANT "a\"b" TO "a\"b""#;
    let err = conn.exec(sql, &[]).expect_err(sql);
    assert_sqlstate(&err, "42601");
}
