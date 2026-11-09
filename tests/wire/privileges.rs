use crate::common::*;

// privilege_enforcement covers GRANT/REVOKE enforcement for a non-superuser
// role: no access without a grant, access after GRANT, denial after REVOKE,
// per-privilege granularity, owner access, and GRANT ... TO PUBLIC.
#[test]
fn privilege_enforcement() {
    let addr = start_server_env(Env::password("adminpw"));

    let admin = connect_as(&addr, "postgres", "adminpw"); // seeded superuser
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'b'");
    must_exec(&admin, "CREATE TABLE t (id int)"); // owned by postgres
    must_exec(&admin, "INSERT INTO t VALUES (1)");

    let alice = connect_as(&addr, "alice", "a");

    // No grant: alice can't read t.
    alice
        .query_row("SELECT count(*) FROM t", &[])
        .scan::<i64>()
        .expect_err("no privilege -> denied");

    // GRANT SELECT lets her read but not write.
    must_exec(&admin, "GRANT SELECT ON t TO alice");
    let n: i64 = alice.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);
    alice
        .exec("INSERT INTO t VALUES (2)", &[])
        .expect_err("only SELECT granted -> INSERT denied");

    // REVOKE removes it again.
    must_exec(&admin, "REVOKE SELECT ON t FROM alice");
    alice
        .query_row("SELECT count(*) FROM t", &[])
        .scan::<i64>()
        .unwrap_err();

    // Ownership: bob creates and uses his own table.
    let bob = connect_as(&addr, "bob", "b");
    must_exec(&bob, "CREATE TABLE bt (x int)");
    bob.exec("INSERT INTO bt VALUES (5)", &[])
        .expect("owner has all privileges");

    // alice can't touch bob's table until a PUBLIC grant.
    alice
        .query_row("SELECT count(*) FROM bt", &[])
        .scan::<i64>()
        .unwrap_err();
    must_exec(&bob, "GRANT SELECT ON bt TO PUBLIC");
    let n: i64 = alice.query_row("SELECT count(*) FROM bt", &[]).scan().unwrap();
    assert_eq!(n, 1);

    // Superuser bypasses all checks.
    admin.query_row("SELECT count(*) FROM bt", &[]).scan::<i64>().unwrap();
}

// grant_all_and_drop covers GRANT ALL and owner-only DROP.
#[test]
fn grant_all_and_drop() {
    let addr = start_server_env(Env::password("adminpw"));

    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE t (id int)");

    let alice = connect_as(&addr, "alice", "a");

    // GRANT ALL: alice can insert and select.
    must_exec(&admin, "GRANT ALL ON t TO alice");
    alice.exec("INSERT INTO t VALUES (1)", &[]).unwrap();
    let n: i64 = alice.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);

    // But she can't DROP a table she doesn't own (grants don't confer DROP).
    alice
        .exec("DROP TABLE t", &[])
        .expect_err("only the owner drops a table");
}

fn extended(conn: &Conn, sql: &str) -> Result<Tag> {
    conn.exec_params::<&[u8]>(sql, &[], &[], &[], &[]).map(|r| r.tag)
}

/// What the schema holds, to show a refused statement changed nothing.
fn schema_text(conn: &Conn) -> Vec<String> {
    query_column(
        conn,
        "SELECT name || ':' || coalesce(sql, '') FROM sqlite_master WHERE name NOT LIKE '\\_overlite%' ESCAPE '\\' ORDER BY name",
        0,
    )
}

const ALTER_FORMS: &[&str] = &[
    "ADD CONSTRAINT k2 UNIQUE (b)",
    "ADD CONSTRAINT c2 CHECK (id > 0)",
    "ADD CONSTRAINT f2 FOREIGN KEY (b) REFERENCES u (id)",
    "ADD UNIQUE (b)",
    "ADD COLUMN plain text",
    "ADD COLUMN stamp text DEFAULT now()",
    "ADD COLUMN IF NOT EXISTS extra text",
    "ADD COLUMN IF NOT EXISTS b text",
    "DROP COLUMN d",
    "DROP COLUMN IF EXISTS d",
    "DROP COLUMN IF EXISTS nope",
    "DROP CONSTRAINT k",
    "DROP CONSTRAINT IF EXISTS k",
    "DROP CONSTRAINT IF EXISTS nope",
    "DROP CONSTRAINT t_pkey CASCADE",
    "ALTER COLUMN b TYPE varchar(5)",
    "ALTER b SET NOT NULL",
    "ALTER COLUMN b SET DEFAULT 'x'",
    "ALTER COLUMN d DROP DEFAULT",
    "RENAME COLUMN b TO b2",
    "RENAME TO t2",
    "SET SCHEMA s1",
    "OWNER TO alice",
    "ENABLE ROW LEVEL SECURITY",
    "ADD COLUMN m1 text, ADD COLUMN m2 text",
    "DROP CONSTRAINT k, DROP COLUMN d",
];

// In the two protocols, for each spelling of the table and each way ALTER TABLE
// goes, a role that does not own the table gets 42501 and the table stays as it is.
#[test]
fn alter_table_needs_owner() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE SCHEMA s1");
    must_exec(&admin, "CREATE TABLE u (id int PRIMARY KEY)");
    must_exec(
        &admin,
        "CREATE TABLE t (id int PRIMARY KEY, b text, d text DEFAULT 'z', CONSTRAINT k UNIQUE (d))",
    );
    must_exec(&admin, "CREATE TABLE child (t_id int REFERENCES t (id))");
    let before = schema_text(&admin);

    let alice = connect_as(&addr, "alice", "a");
    let mut through = Vec::new();
    for spelling in [
        "t",
        "public.t",
        "\"t\"",
        "ONLY t",
        "IF EXISTS t",
        "IF EXISTS ONLY public.t",
    ] {
        for form in ALTER_FORMS {
            let sql = format!("ALTER TABLE {spelling} {form}");
            for (path, got) in [("simple", alice.exec(&sql, &[])), ("extended", extended(&alice, &sql))] {
                if got.as_ref().map_or_else(super::common::WireError::code, |_| "ok") != "42501" {
                    through.push(format!("{path}: {sql}: {got:?}"));
                }
            }
        }
    }
    assert!(
        through.is_empty(),
        "{} got past the check:\n{}",
        through.len(),
        through.join("\n")
    );
    assert_eq!(schema_text(&admin), before);
}

// The check goes by the table the statement names, wherever that is.
#[test]
fn alter_table_owner_passes() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE SCHEMA s1");

    let alice = connect_as(&addr, "alice", "a");
    must_exec(&alice, "CREATE TABLE mine (id int, CONSTRAINT k UNIQUE (id))");
    for sql in [
        "ALTER TABLE mine DROP CONSTRAINT k",
        "ALTER TABLE IF EXISTS mine ADD CONSTRAINT k UNIQUE (id)",
        "ALTER TABLE ONLY mine DROP CONSTRAINT k",
        "ALTER TABLE IF EXISTS ONLY public.mine ALTER COLUMN id SET NOT NULL",
    ] {
        must_exec(&alice, sql);
        extended(&alice, "ALTER TABLE mine ADD COLUMN IF NOT EXISTS c text").unwrap();
    }

    // A missing table is what it is, for anyone.
    let err = alice.exec("ALTER TABLE nowhere DROP CONSTRAINT k", &[]).unwrap_err();
    assert_sqlstate(&err, "42P01");
    let err = extended(&alice, "ALTER TABLE nowhere DROP CONSTRAINT k").unwrap_err();
    assert_sqlstate(&err, "42P01");
    must_exec(&alice, "ALTER TABLE IF EXISTS nowhere DROP CONSTRAINT k");

    // The same two words before a name, and every name of a list.
    must_exec(&alice, "DROP TABLE IF EXISTS mine");
    must_exec(&alice, "CREATE TABLE mine (id int)");
    must_exec(&admin, "CREATE TABLE theirs (id int)");
    let err = alice.exec("DROP TABLE mine, theirs", &[]).unwrap_err();
    assert_sqlstate(&err, "42501");
    let err = alice.exec("DROP TABLE IF EXISTS theirs CASCADE", &[]).unwrap_err();
    assert_sqlstate(&err, "42501");
}

// The role in force is what counts, and an owner's role passes it on.
#[test]
fn alter_table_owner_through_a_role() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE ROLE team");
    must_exec(&admin, "GRANT team TO alice");
    let alice = connect_as(&addr, "alice", "a");
    must_exec(&alice, "SET ROLE team");
    must_exec(&alice, "CREATE TABLE shared (id int, CONSTRAINT k UNIQUE (id))");
    must_exec(&alice, "RESET ROLE");
    must_exec(&alice, "ALTER TABLE shared DROP CONSTRAINT k");

    let bob = "bob";
    must_exec(&admin, &format!("CREATE ROLE {bob} LOGIN PASSWORD 'b'"));
    let err = extended(&connect_as(&addr, bob, "b"), "ALTER TABLE shared ADD UNIQUE (id)").unwrap_err();
    assert_sqlstate(&err, "42501");

    // Refused inside a transaction, it fails the transaction as an error does.
    let bob = connect_as(&addr, bob, "b");
    let tx = bob.begin().unwrap();
    tx.exec("ALTER TABLE shared ADD UNIQUE (id)", &[]).unwrap_err();
    let err = tx.exec("SELECT 1", &[]).unwrap_err();
    assert_sqlstate(&err, "25P02");
    tx.rollback().unwrap();
}

// EXECUTE of a statement from PREPARE is held to the same checks as the statement.
#[test]
fn sql_execute_checks_privileges() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE secret (id int)");
    must_exec(&admin, "INSERT INTO secret VALUES (1)");

    let alice = connect_as(&addr, "alice", "a");
    for (name, sql) in [
        ("q", "SELECT * FROM secret"),
        ("w", "INSERT INTO secret VALUES (2)"),
    ] {
        must_exec(&alice, &format!("PREPARE {name} AS {sql}"));
        let err = alice.exec(&format!("EXECUTE {name}"), &[]).unwrap_err();
        assert_sqlstate(&err, "42501");
    }
    // Postgres prepares only what reads or writes rows: DDL is its syntax error.
    for sql in ["ALTER TABLE secret ADD COLUMN y text", "DROP TABLE secret"] {
        let err = alice.exec(&format!("PREPARE a AS {sql}"), &[]).unwrap_err();
        assert_sqlstate(&err, "42601");
    }
    let n: i64 = admin.query_row("SELECT count(*) FROM secret", &[]).scan().unwrap();
    assert_eq!(n, 1);

    // Within a transaction it fails it, as any error does.
    let tx = alice.begin().unwrap();
    tx.exec("EXECUTE q", &[]).unwrap_err();
    assert_sqlstate(&tx.exec("SELECT 1", &[]).unwrap_err(), "25P02");
    tx.rollback().unwrap();

    // The owner's own statement runs.
    must_exec(&admin, "PREPARE mine AS SELECT count(*) FROM secret");
    must_exec(&admin, "EXECUTE mine");
}

// parse_grant: a quoted name holding the keyword is a name.
#[test]
fn grant_keywords_in_a_quoted_name() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, r#"CREATE TABLE "to" (id int)"#);
    must_exec(&admin, r#"GRANT SELECT ON "to" TO alice"#);
    let alice = connect_as(&addr, "alice", "a");
    let n: i64 = alice.query_row(r#"SELECT count(*) FROM "to""#, &[]).scan().unwrap();
    assert_eq!(n, 0);
}

/// A server with roles alice and bob and schemas s1 and s2.
fn two_roles() -> (Server, Conn, Conn, Conn) {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'b'");
    must_exec(&admin, "CREATE SCHEMA s1");
    must_exec(&admin, "CREATE SCHEMA s2");
    let alice = connect_as(&addr, "alice", "a");
    let bob = connect_as(&addr, "bob", "b");
    (addr, admin, alice, bob)
}

#[track_caller]
fn refused(conn: &Conn, sql: &str) {
    for (path, got) in [("simple", conn.exec(sql, &[])), ("extended", extended(conn, sql))] {
        match got {
            Err(e) if e.code() == "42501" => {}
            other => panic!("{path}: {sql}: {other:?}"),
        }
    }
}

// Every way to touch a table, by the owner of another table of the same name.
const TOUCHES: &[&str] = &[
    "SELECT * FROM {t}",
    "INSERT INTO {t} VALUES (1)",
    "UPDATE {t} SET id = 2",
    "DELETE FROM {t}",
    "TRUNCATE {t}",
    "ALTER TABLE {t} ADD COLUMN x text",
    "ALTER TABLE {t} DROP COLUMN IF EXISTS id",
    "ALTER TABLE {t} ALTER COLUMN id SET NOT NULL",
    "ALTER TABLE {t} DROP CONSTRAINT IF EXISTS k",
    "ALTER TABLE {t} RENAME TO other",
    "ALTER TABLE {t} SET SCHEMA s2",
    "ALTER TABLE {t} ENABLE ROW LEVEL SECURITY",
    "DROP TABLE {t}",
    "DROP TABLE IF EXISTS {t}",
    "GRANT ALL ON {t} TO alice",
    "REVOKE ALL ON {t} FROM alice",
    "CREATE POLICY p ON {t} USING (true)",
    "DROP POLICY IF EXISTS p ON {t}",
];

// COPY is refused on the wire before any data; simple protocol only.
#[track_caller]
fn copy_refused(conn: &Conn, table: &str) {
    let mut out = Vec::new();
    assert_sqlstate(
        &conn.copy_to(&mut out, &format!("COPY {table} TO STDOUT")).unwrap_err(),
        "42501",
    );
    assert_sqlstate(
        &conn
            .copy_to(&mut out, &format!("COPY (SELECT * FROM {table}) TO STDOUT"))
            .unwrap_err(),
        "42501",
    );
    assert_sqlstate(
        &conn
            .copy_from("1\n".as_bytes(), &format!("COPY {table} FROM STDIN"))
            .unwrap_err(),
        "42501",
    );
}

#[test]
fn owner_of_a_public_table_does_not_own_its_namesake() {
    let (_addr, admin, alice, _bob) = two_roles();
    must_exec(&admin, "CREATE TABLE s1.mine (id int)");
    must_exec(&alice, "CREATE TABLE mine (id int)");
    // Her own table stays hers, and a grant she gives on it stays on it.
    must_exec(&alice, "GRANT ALL ON mine TO alice");
    must_exec(&alice, "ALTER TABLE mine ADD COLUMN y text");

    for spelling in ["s1.mine", "\"s1\".\"mine\""] {
        for touch in TOUCHES {
            refused(&alice, &touch.replace("{t}", spelling));
        }
    }
    copy_refused(&alice, "s1.mine");
    must_exec(&alice, "SET search_path TO s1");
    for touch in TOUCHES {
        refused(&alice, &touch.replace("{t}", "mine"));
    }
    let n: i64 = admin
        .query_row("SELECT count(*) FROM pragma_table_info('s1.mine')", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "s1.mine is as it was");
}

// A grant is on the table it names, not on all tables of that name.
#[test]
fn grants_stay_on_their_table() {
    let (_addr, admin, alice, bob) = two_roles();
    must_exec(&admin, "CREATE TABLE t (id int)");
    must_exec(&admin, "CREATE TABLE s1.t (id int)");
    must_exec(&admin, "CREATE TABLE s2.t (id int)");

    must_exec(&admin, "GRANT SELECT ON s1.t TO alice");
    must_exec(&alice, "SELECT * FROM s1.t");
    refused(&alice, "SELECT * FROM t");
    refused(&alice, "SELECT * FROM s2.t");

    must_exec(&admin, "GRANT SELECT ON public.t TO bob");
    must_exec(&bob, "SELECT * FROM t");
    refused(&bob, "SELECT * FROM s1.t");

    must_exec(&admin, "REVOKE SELECT ON s1.t FROM alice");
    refused(&alice, "SELECT * FROM s1.t");
    must_exec(&bob, "SELECT * FROM public.t");
}

// GRANT is the owner's to give.
#[test]
fn grant_needs_owner() {
    let (_addr, admin, alice, bob) = two_roles();
    must_exec(&admin, "CREATE TABLE theirs (id int)");
    must_exec(&alice, "CREATE TABLE hers (id int)");
    refused(&bob, "GRANT ALL ON theirs TO bob");
    refused(&bob, "GRANT SELECT ON hers TO bob");
    refused(&bob, "REVOKE ALL ON hers FROM alice");
    refused(&bob, "SELECT * FROM hers");
    must_exec(&alice, "GRANT SELECT ON hers TO bob");
    must_exec(&bob, "SELECT * FROM hers");
}

#[test]
fn same_named_tables_keep_their_owners() {
    let (_addr, _admin, alice, bob) = two_roles();
    must_exec(&alice, "CREATE TABLE t (id int)");
    must_exec(&bob, "CREATE TABLE s1.t (id int)");
    must_exec(&alice, "ALTER TABLE t ADD COLUMN a text");
    must_exec(&bob, "ALTER TABLE s1.t ADD COLUMN b text");
    refused(&alice, "ALTER TABLE s1.t ADD COLUMN c text");
    refused(&bob, "ALTER TABLE t ADD COLUMN c text");

    // Through the search_path as well.
    must_exec(&bob, "SET search_path TO s1");
    must_exec(&bob, "ALTER TABLE t ADD COLUMN c text");
    must_exec(&bob, "CREATE TABLE u (id int)");
    must_exec(&bob, "ALTER TABLE s1.u ADD COLUMN a text");
    refused(&alice, "ALTER TABLE s1.u ADD COLUMN b text");

    // Dropping one leaves the other's owner alone, and the name free.
    must_exec(&bob, "DROP TABLE t");
    must_exec(&alice, "ALTER TABLE t ADD COLUMN d text");
    must_exec(&alice, "CREATE TABLE s1.t (id int)");
    must_exec(&alice, "ALTER TABLE s1.t ADD COLUMN e text");
    must_exec(&alice, "DROP TABLE t");
    must_exec(&alice, "ALTER TABLE s1.t ADD COLUMN f text");
}

// Dropping a schema takes its owners with it: the next table of that name starts clean.
#[test]
fn drop_schema_forgets_owners() {
    let (_addr, admin, alice, bob) = two_roles();
    must_exec(&alice, "CREATE TABLE s1.t (id int)");
    must_exec(&admin, "DROP SCHEMA s1 CASCADE");
    must_exec(&admin, "CREATE SCHEMA s1");
    must_exec(&bob, "CREATE TABLE IF NOT EXISTS s1.t (id int)");
    must_exec(&bob, "ALTER TABLE s1.t ADD COLUMN a text");
    refused(&alice, "ALTER TABLE s1.t ADD COLUMN b text");
}

#[test]
fn ownership_survives_a_move() {
    let (_addr, admin, alice, bob) = two_roles();
    must_exec(&alice, "CREATE TABLE s1.t (id int)");
    must_exec(&alice, "GRANT SELECT ON s1.t TO bob");

    must_exec(&alice, "ALTER TABLE s1.t SET SCHEMA s2");
    must_exec(&alice, "ALTER TABLE s2.t ADD COLUMN a text");
    refused(&bob, "ALTER TABLE s2.t ADD COLUMN b text");
    must_exec(&bob, "SELECT * FROM s2.t");

    must_exec(&admin, "ALTER SCHEMA s2 RENAME TO s3");
    must_exec(&alice, "ALTER TABLE s3.t ADD COLUMN b text");
    refused(&bob, "ALTER TABLE s3.t ADD COLUMN c text");
    must_exec(&bob, "SELECT * FROM s3.t");

    must_exec(&alice, "ALTER TABLE s3.t SET SCHEMA public");
    must_exec(&alice, "ALTER TABLE t ADD COLUMN c text");
    must_exec(&bob, "SELECT * FROM t");

    must_exec(&alice, "ALTER TABLE t RENAME TO t2");
    must_exec(&alice, "ALTER TABLE t2 ADD COLUMN d text");
    refused(&bob, "ALTER TABLE t2 ADD COLUMN e text");
    must_exec(&bob, "SELECT * FROM t2");

    // What took the old name has an owner of its own.
    must_exec(&bob, "CREATE TABLE t (id int)");
    refused(&alice, "ALTER TABLE t ADD COLUMN x text");
    must_exec(&alice, "ALTER TABLE t2 SET SCHEMA s1");
    must_exec(&alice, "ALTER TABLE s1.t2 RENAME TO t3");
    must_exec(&alice, "ALTER TABLE s1.t3 ADD COLUMN e text");
}

// IF NOT EXISTS on a table that is there creates nothing, so it takes nothing.
#[test]
fn create_if_not_exists_takes_no_ownership() {
    let (_addr, _admin, alice, bob) = two_roles();
    must_exec(&alice, "CREATE TABLE t (id int)");
    must_exec(&bob, "CREATE TABLE IF NOT EXISTS t (id int)");
    refused(&bob, "ALTER TABLE t ADD COLUMN a text");
    must_exec(&alice, "ALTER TABLE t ADD COLUMN a text");
}

// pg_shdepend ties each table to its own owner.
#[test]
fn shdepend_reads_owners_by_schema() {
    let (_addr, admin, alice, bob) = two_roles();
    must_exec(&alice, "CREATE TABLE t (id int)");
    must_exec(&bob, "CREATE TABLE s1.t (id int)");
    let owners = query_column(
        &admin,
        "SELECT n.nspname || ':' || r.rolname FROM pg_shdepend d
         JOIN pg_class c ON c.oid = d.objid
         JOIN pg_namespace n ON n.oid = c.relnamespace
         JOIN pg_roles r ON r.oid = d.refobjid
         WHERE c.relname = 't' ORDER BY 1",
        0,
    );
    assert_eq!(owners, ["public:alice", "s1:bob"]);
}

/// A database as the server left it before owners were keyed by stored name: a row
/// for a table outside public holds the bare name. Written directly, with no server.
fn with_old_owner_rows(db: &std::path::Path, rows: &[(&str, &str)]) {
    let c = rusqlite::Connection::open(db).unwrap();
    for (stored, bare) in rows {
        c.execute(
            "UPDATE _overlite_owners SET tablename = ?1 WHERE tablename = ?2",
            [bare, stored],
        )
        .unwrap();
    }
}

// The safety of keying owners by stored name without migrating: an old row outside
// public stops granting, and the table then has no owner, which is admin only.
#[test]
fn old_owner_rows_fail_closed() {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    let first = start_proc(&db);
    let admin = connect(&first);
    must_exec(&admin, "CREATE ROLE alice LOGIN");
    must_exec(&admin, "CREATE ROLE bob LOGIN");
    must_exec(&admin, "CREATE SCHEMA s1");
    let alice = connect_as(&first, "alice", "");
    must_exec(&alice, "CREATE TABLE mine (id int)");
    must_exec(&alice, "CREATE TABLE s1.old (id int, CONSTRAINT k UNIQUE (id))");
    must_exec(&alice, "INSERT INTO mine VALUES (1)");
    drop((alice, admin));
    signal_and_wait(&first, libc::SIGTERM).0.unwrap();
    with_old_owner_rows(&db, &[("s1.old", "old")]);

    // It opens, and the public table keeps its owner.
    let server = start_proc(&db);
    let (admin, alice, bob) = (
        connect(&server),
        connect_as(&server, "alice", ""),
        connect_as(&server, "bob", ""),
    );
    must_exec(&alice, "ALTER TABLE mine ADD COLUMN a text");
    let n: i64 = alice.query_row("SELECT count(*) FROM mine", &[]).scan().unwrap();
    assert_eq!(n, 1);
    must_exec(&alice, "GRANT SELECT ON mine TO bob");
    must_exec(&bob, "SELECT * FROM mine");
    refused(&bob, "ALTER TABLE mine ADD COLUMN b text");

    // Neither the creator of the old table nor anyone else gets near it.
    for role in [&alice, &bob] {
        for touch in TOUCHES {
            refused(role, &touch.replace("{t}", "s1.old"));
        }
        copy_refused(role, "s1.old");
    }
    let n: i64 = admin
        .query_row(
            "SELECT count(*) FROM _overlite_owners WHERE lower(tablename) = 's1.old'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 0, "no row for it: it reads as having no owner");

    // An admin still can, and can hand it back.
    must_exec(&admin, "INSERT INTO s1.old VALUES (1)");
    must_exec(&admin, "ALTER TABLE s1.old ADD COLUMN c text");
    must_exec(&admin, "GRANT SELECT ON s1.old TO alice");
    must_exec(&alice, "SELECT * FROM s1.old");
    refused(&alice, "ALTER TABLE s1.old ADD COLUMN d text");
}

// Nobody's table is open to everyone: no row at all, as a table made before owners
// were recorded, is the same.
#[test]
fn a_table_without_an_owner_is_admin_only() {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    let first = start_proc(&db);
    let admin = connect(&first);
    must_exec(&admin, "CREATE ROLE alice LOGIN");
    must_exec(&admin, "CREATE SCHEMA s1");
    must_exec(&admin, "CREATE TABLE plain (id int)");
    must_exec(&admin, "CREATE TABLE s1.plain (id int)");
    drop(admin);
    signal_and_wait(&first, libc::SIGTERM).0.unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM _overlite_owners", [])
        .unwrap();

    let server = start_proc(&db);
    let alice = connect_as(&server, "alice", "");
    for t in ["plain", "s1.plain"] {
        for touch in TOUCHES {
            refused(&alice, &touch.replace("{t}", t));
        }
    }
    copy_refused(&alice, "plain");
    must_exec(&connect(&server), "INSERT INTO s1.plain VALUES (1)");
}
