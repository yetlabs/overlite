use std::thread;
use std::time::Duration;

use crate::common::*;

// A DROP DATABASE waits for a client that is still leaving the database, as
// Postgres waits for its backend to exit. Prisma drops its shadow database the
// moment it lets go of it.
#[test]
fn drop_database_waits_for_leaving_client() {
    let addr = start_cluster(8);
    let admin = dial_db(&addr, ENTRY_DATABASE).expect("admin connection");
    let shadow = must_create_database(&addr, &admin, "shadow");

    thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let _ = shadow.close();
    });
    must_exec(&admin, r#"DROP DATABASE IF EXISTS "shadow" WITH (FORCE)"#);
    assert!(dial_db(&addr, "shadow").is_none());
}

// Each way a CREATE/DROP DATABASE fails has its own SQLSTATE.
#[test]
fn database_ddl_error_codes() {
    let addr = start_cluster(8);
    let admin = dial_db(&addr, ENTRY_DATABASE).expect("admin connection");
    must_exec(&admin, "CREATE DATABASE shop");

    let code = |sql: &str| -> String {
        let res = admin.exec(sql, &[]);
        match res.as_ref().err().and_then(WireError::pg) {
            Some(pe) => pe.code.clone(),
            None => panic!("{sql}: want a PgError, got {res:?}"),
        }
    };
    assert_eq!(code("CREATE DATABASE shop"), "42P04");
    assert_eq!(code("DROP DATABASE nope"), "3D000");
    assert_eq!(code(&format!("DROP DATABASE {ENTRY_DATABASE}")), "55006");
}

fn quoted_names_setup() -> (Server, Conn) {
    let addr = start_cluster(8);
    let admin = dial_db(&addr, ENTRY_DATABASE).expect("admin connection");
    (addr, admin)
}

fn listed(admin: &Conn) -> Vec<String> {
    query_column(admin, "SELECT datname FROM pg_database ORDER BY datname", 0)
}

// The name is what stands between the quotes: `DROP DATABASE "a b"` used to act on `a`.
#[test]
fn drop_database_quoted_name_is_not_cut_at_the_space() {
    let (addr, admin) = quoted_names_setup();
    must_exec(&admin, "CREATE DATABASE a");
    assert_sqlstate(&admin.exec(r#"DROP DATABASE "a b""#, &[]).unwrap_err(), "3D000");
    assert!(listed(&admin).contains(&"a".to_string()));

    must_exec(&admin, r#"CREATE DATABASE "a b""#);
    must_exec(&admin, r#"DROP DATABASE "a b""#);
    assert!(listed(&admin).contains(&"a".to_string()));
    assert!(!listed(&admin).contains(&"a b".to_string()));
    dial_db(&addr, "a").expect("a is still there");
}

// A name holds any words, and those of an error message are no exception.
#[test]
fn database_names_holding_phrases_of_errors() {
    let (addr, admin) = quoted_names_setup();
    for name in [
        "already exists",
        "does not exist",
        "IF EXISTS",
        "being accessed by other users",
    ] {
        must_exec(&admin, &format!(r#"CREATE DATABASE "{name}""#));
        assert!(listed(&admin).contains(&name.to_string()), "{name}");
        let conn = dial_db(&addr, name).expect(name);
        must_exec(&conn, "CREATE TABLE t (a int)");
        conn.close().unwrap();
        let err = admin.exec(&format!(r#"CREATE DATABASE "{name}""#), &[]).unwrap_err();
        assert_sqlstate(&err, "42P04");
        eventually(Duration::from_secs(3), Duration::from_millis(20), || {
            admin.exec(&format!(r#"DROP DATABASE "{name}""#), &[]).is_ok()
        });
        let err = admin.exec(&format!(r#"DROP DATABASE "{name}""#), &[]).unwrap_err();
        assert_sqlstate(&err, "3D000");
        must_exec(&admin, &format!(r#"DROP DATABASE IF EXISTS "{name}""#));
    }
}

// A name is a file name underneath, and may not walk out of the directory.
#[test]
fn database_name_is_not_a_path() {
    let (addr, admin) = quoted_names_setup();
    must_exec(&admin, r#"CREATE DATABASE "../escape.x/y""#);
    assert!(listed(&admin).contains(&"../escape.x/y".to_string()));
    assert!(addr.dir().join("%2E%2E%2Fescape%2Ex%2Fy.db").exists());
    assert_no_file_exists(addr.dir().parent().unwrap().join("escape.x"));
    dial_db(&addr, "../escape.x/y").expect("the quoted name connects");
}

#[test]
fn unquoted_database_names_fold() {
    let (addr, admin) = quoted_names_setup();
    must_exec(&admin, "CREATE DATABASE Shop");
    assert!(dial_db(&addr, "shop").is_some());
    assert!(dial_db(&addr, "Shop").is_none());
    assert_sqlstate(&admin.exec(r#"DROP DATABASE "Shop""#, &[]).unwrap_err(), "3D000");
}

#[test]
fn zero_length_database_name_is_a_syntax_error() {
    let (_addr, admin) = quoted_names_setup();
    assert_sqlstate(&admin.exec(r#"DROP DATABASE """#, &[]).unwrap_err(), "42601");
    assert_sqlstate(&admin.exec("DROP DATABASE", &[]).unwrap_err(), "42601");
}

#[test]
fn alter_database() {
    let (addr, admin) = quoted_names_setup();
    must_exec(&admin, r#"CREATE DATABASE "a b""#);
    must_exec(&admin, r#"ALTER DATABASE "a b" OWNER TO postgres"#);
    must_exec(&admin, r#"ALTER DATABASE "a b" SET search_path = public"#);
    must_exec(&admin, r#"ALTER DATABASE "a b" RENAME TO "c d""#);
    assert!(listed(&admin).contains(&"c d".to_string()));
    assert!(dial_db(&addr, "a b").is_none());
    let conn = dial_db(&addr, "c d").expect("the new name connects");
    conn.ping().unwrap();

    let code = |sql: &str| admin.exec(sql, &[]).unwrap_err().code().to_string();
    assert_eq!(code("ALTER DATABASE nope SET search_path = public"), "3D000");
    assert_eq!(code("ALTER DATABASE nope RENAME TO other"), "3D000");
    assert_eq!(code(r#"ALTER DATABASE "c d" RENAME TO other"#), "55006");
    assert_eq!(
        code(&format!(r#"ALTER DATABASE "{ENTRY_DATABASE}" RENAME TO other"#)),
        "55006"
    );
    must_exec(&admin, "CREATE DATABASE taken");
    conn.close().unwrap();
    eventually(Duration::from_secs(3), Duration::from_millis(20), || {
        code(r#"ALTER DATABASE "c d" RENAME TO taken"#) == "42P04"
    });
}

#[test]
fn database_ddl_refused_in_a_transaction() {
    let (_addr, admin) = quoted_names_setup();
    for sql in ["CREATE DATABASE x", "DROP DATABASE x"] {
        must_exec(&admin, "BEGIN");
        assert_sqlstate(&admin.exec(sql, &[]).unwrap_err(), "25001");
        assert_sqlstate(&admin.exec("SELECT 1", &[]).unwrap_err(), "25P02");
        must_exec(&admin, "ROLLBACK");
    }
}

// The set of databases is everyone's too.
#[test]
fn open_session_sees_a_new_database() {
    let (addr, admin) = quoted_names_setup();
    let open = dial_db(&addr, ENTRY_DATABASE).expect("second connection");
    assert!(!listed(&open).contains(&"fresh".to_string()));
    must_exec(&admin, "CREATE DATABASE fresh");
    assert!(listed(&open).contains(&"fresh".to_string()));
    must_exec(&admin, "DROP DATABASE fresh");
    assert!(!listed(&open).contains(&"fresh".to_string()));
}
