use crate::common::*;

#[test]
fn roles_lifecycle() {
    let conn = connect(&start_server());

    let rolenames = || query_column(&conn, "SELECT rolname FROM pg_catalog.pg_roles ORDER BY rolname", 0);

    // The default role exists.
    assert_contains!(rolenames(), "postgres");

    // CREATE ROLE / CREATE USER (with attributes) succeed and show up.
    must_exec(&conn, "CREATE ROLE app_reader");
    must_exec(&conn, "CREATE USER webuser WITH PASSWORD 'secret' CREATEDB");
    let roles = rolenames();
    assert_contains!(roles, "app_reader");
    assert_contains!(roles, "webuser");

    // Attributes are reflected: USER logs in, CREATEDB set; plain ROLE cannot log in.
    let (login, createdb): (bool, bool) = conn
        .query_row(
            "SELECT rolcanlogin, rolcreatedb FROM pg_catalog.pg_roles WHERE rolname = 'webuser'",
            &[],
        )
        .scan()
        .unwrap();
    assert!(login);
    assert!(createdb);
    let login: bool = conn
        .query_row(
            "SELECT rolcanlogin FROM pg_catalog.pg_roles WHERE rolname = 'app_reader'",
            &[],
        )
        .scan()
        .unwrap();
    assert!(!login);

    // A grant names what exists, as in Postgres.
    assert_sqlstate(
        &conn.exec("GRANT SELECT ON some_table TO app_reader", &[]).unwrap_err(),
        "42P01",
    );
    assert_sqlstate(
        &conn.exec("REVOKE ALL ON some_table FROM app_reader", &[]).unwrap_err(),
        "42P01",
    );
    conn.exec("CREATE TABLE some_table (a int)", &[]).unwrap();
    conn.exec("GRANT SELECT ON some_table TO app_reader", &[]).unwrap();
    conn.exec("REVOKE ALL ON some_table FROM app_reader", &[]).unwrap();
    assert_sqlstate(
        &conn.exec("GRANT SELECT ON some_table TO nobody", &[]).unwrap_err(),
        "42704",
    );

    // ALTER ROLE updates only the named attribute.
    must_exec(&conn, "ALTER ROLE app_reader WITH LOGIN");
    let login: bool = conn
        .query_row(
            "SELECT rolcanlogin FROM pg_catalog.pg_roles WHERE rolname = 'app_reader'",
            &[],
        )
        .scan()
        .unwrap();
    assert!(login);

    // DROP ROLE removes it.
    must_exec(&conn, "DROP ROLE app_reader");
    assert_not_contains!(rolenames(), "app_reader");

    // The internal roles table is hidden from the catalog.
    for rel in query_column(&conn, "SELECT relname FROM pg_catalog.pg_class WHERE relkind = 'r'", 0) {
        assert!(
            !rel.starts_with("_overlite"),
            "internal table {rel:?} leaked into catalog"
        );
    }
}

// A role's name is what stands between its quotes, and a bare one folds.
#[test]
fn quoted_role_names() {
    let conn = connect(&start_server());
    let exists = |name: &str| {
        let q = format!("SELECT count(*) FROM pg_roles WHERE rolname = '{name}'");
        conn.query_row(&q, &[]).scan::<i64>().unwrap() == 1
    };
    must_exec(&conn, r#"CREATE ROLE "a b""#);
    assert!(exists("a b"));
    assert!(!exists("a"));
    assert_sqlstate(&conn.exec(r#"CREATE ROLE "a b""#, &[]).unwrap_err(), "42710");

    must_exec(&conn, r#"CREATE ROLE "Mixed""#);
    must_exec(&conn, "CREATE ROLE Folded");
    assert!(exists("Mixed"));
    assert_eq!(
        scan_str(&conn, "SELECT rolname FROM pg_roles WHERE rolname = 'folded'"),
        "folded"
    );

    must_exec(&conn, r#"ALTER ROLE "a b" LOGIN"#);
    let login: bool = conn
        .query_row("SELECT rolcanlogin FROM pg_roles WHERE rolname = 'a b'", &[])
        .scan()
        .unwrap();
    assert!(login);
    must_exec(&conn, r#"ALTER ROLE "a b" RENAME TO "c d""#);
    assert!(exists("c d"));
    assert!(!exists("a b"));

    must_exec(&conn, r#"CREATE ROLE "x""y""#);
    assert!(exists(r#"x"y"#));
    must_exec(&conn, r#"DROP ROLE "x""y""#);

    must_exec(&conn, r#"CREATE ROLE "a,b""#);
    assert!(exists("a,b"));
    must_exec(&conn, r#"CREATE ROLE "login""#);
    assert!(
        !conn
            .query_row("SELECT rolcanlogin FROM pg_roles WHERE rolname = 'login'", &[])
            .scan::<bool>()
            .unwrap()
    );
    must_exec(&conn, r#"DROP ROLE "a,b", "login""#);
    assert!(!exists("a,b"));

    must_exec(&conn, r#"CREATE USER "u v" PASSWORD 'p w'"#);
    assert!(exists("u v"));
    must_exec(&conn, r#"DROP USER "u v""#);

    must_exec(&conn, "CREATE TABLE t (a int)");
    must_exec(&conn, r#"GRANT SELECT ON t TO "c d""#);
    must_exec(&conn, r#"REVOKE SELECT ON t FROM "c d""#);
    must_exec(&conn, r#"GRANT "c d" TO "Mixed""#);
    must_exec(&conn, r#"REVOKE "c d" FROM "Mixed""#);
    must_exec(&conn, r#"ALTER TABLE t OWNER TO "c d""#);
    must_exec(&conn, r#"SET ROLE "c d""#);
    assert_eq!(scan_str(&conn, "SELECT current_user"), "c d");
    must_exec(&conn, "RESET ROLE");
    must_exec(&conn, r#"DROP ROLE "c d""#);
    assert!(!exists("c d"));
}

#[test]
fn role_names_postgres_refuses() {
    let conn = connect(&start_server());
    for (sql, code) in [
        (r#"CREATE ROLE """#, "42601"),
        ("CREATE ROLE", "42601"),
        ("DROP ROLE", "42601"),
        ("CREATE ROLE public", "42939"),
        ("CREATE ROLE pg_x", "42939"),
        (r#"ALTER ROLE "nobody" LOGIN"#, "42704"),
        (r#"ALTER ROLE "nobody" RENAME TO x"#, "42704"),
        (r#"DROP ROLE "nobody here""#, "42704"),
    ] {
        let got = conn.exec(sql, &[]);
        assert_eq!(
            got.as_ref().map_err(WireError::code).err(),
            Some(code),
            "{sql}: {got:?}"
        );
    }
    must_exec(&conn, r#"DROP ROLE IF EXISTS "nobody here""#);
    must_exec(&conn, "ALTER ROLE current_user CREATEDB");
}
