//! Column privileges: GRANT/REVOKE (col, ...) ON t. A role holding only column
//! privileges uses exactly those columns, as in Postgres, whose answers (17) these
//! tests repeat: the statements refused there are refused here, with 42501.

use crate::common::*;
use std::process::Command;

fn extended(conn: &Conn, sql: &str) -> Result<Tag> {
    conn.exec_params::<&[u8]>(sql, &[], &[], &[], &[]).map(|r| r.tag)
}

/// Refused with 42501 in both protocols.
#[track_caller]
fn refused(conn: &Conn, sql: &str) {
    for (path, got) in [("simple", conn.exec(sql, &[])), ("extended", extended(conn, sql))] {
        match got {
            Err(e) if e.code() == "42501" => {}
            other => panic!("{path}: {sql}: {other:?}"),
        }
    }
}

/// Runs in both protocols.
#[track_caller]
fn allowed(conn: &Conn, sql: &str) {
    for (path, got) in [("simple", conn.exec(sql, &[])), ("extended", extended(conn, sql))] {
        if let Err(e) = got {
            panic!("{path}: {sql}: {e}");
        }
    }
}

#[track_caller]
fn copy_out(conn: &Conn, sql: &str) -> std::result::Result<String, String> {
    let mut out = Vec::new();
    match conn.copy_to(&mut out, sql) {
        Ok(_) => Ok(String::from_utf8(out).unwrap()),
        Err(e) => Err(e.code().to_string()),
    }
}

#[track_caller]
fn copy_in(conn: &Conn, sql: &str, data: &str) -> std::result::Result<(), String> {
    conn.copy_from(data.as_bytes(), sql)
        .map(|_| ())
        .map_err(|e| e.code().to_string())
}

/// A server with the superuser postgres, roles carol and dave, and tables t (a, b, c)
/// and u owned by postgres.
fn setup() -> (Server, Conn, Conn) {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE carol LOGIN PASSWORD 'c'");
    must_exec(&admin, "CREATE ROLE dave LOGIN PASSWORD 'd'");
    must_exec(&admin, "CREATE TABLE t (a int, b int DEFAULT 7, c text)");
    must_exec(&admin, "INSERT INTO t VALUES (1, 2, 'x')");
    must_exec(&admin, "CREATE TABLE u (id int, s text)");
    must_exec(&admin, "INSERT INTO u VALUES (1, 's')");
    let carol = connect_as(&addr, "carol", "c");
    (addr, admin, carol)
}

/// The column ACLs of t as pg_attribute reports them, in column order.
fn attacl(conn: &Conn, table: &str) -> Vec<String> {
    let q = format!(
        "SELECT attname || ':' || coalesce(attacl::text, '') FROM pg_attribute \
         WHERE attrelid = '{table}'::regclass AND attnum > 0 ORDER BY attnum"
    );
    query_column(conn, &q, 0)
}

// The probe that found it: SELECT (a) let carol read b.
#[test]
fn a_column_grant_is_not_a_table_grant() {
    let (_addr, admin, carol) = setup();
    must_exec(&admin, "GRANT SELECT (a) ON t TO carol");
    let a: i32 = carol.query_row("SELECT a FROM t", &[]).scan().unwrap();
    assert_eq!(a, 1);
    refused(&carol, "SELECT b FROM t");
    refused(&carol, "SELECT * FROM t");
}

#[test]
fn select_uses_only_granted_columns() {
    let (_addr, admin, carol) = setup();
    must_exec(&admin, "GRANT SELECT (a) ON t TO carol");
    for sql in [
        "SELECT a FROM t",
        "SELECT count(*) FROM t",
        "SELECT 1 FROM t",
        "SELECT a AS b FROM t ORDER BY b",
        "SELECT a FROM t WHERE a = 1 ORDER BY a",
        "SELECT a FROM t GROUP BY a HAVING count(*) > 0",
        "SELECT DISTINCT a FROM t",
        "SELECT (SELECT max(a) FROM t)",
        "SELECT EXISTS (SELECT 1 FROM t)",
        "WITH w AS (SELECT a FROM t) SELECT * FROM w",
        "SELECT x.a FROM (SELECT a FROM t) x",
        "SELECT t1.a FROM t t1 JOIN t t2 USING (a)",
        "SELECT a FROM t WHERE a = ANY (ARRAY[1, 2])",
    ] {
        allowed(&carol, sql);
    }
    for sql in [
        "SELECT b FROM t",
        "SELECT * FROM t",
        "SELECT t.* FROM t",
        "SELECT x.* FROM t x",
        "SELECT a FROM t WHERE b = 2",
        "SELECT a FROM t ORDER BY b",
        "SELECT a FROM t GROUP BY a, c",
        "SELECT a FROM t WHERE a IN (SELECT b FROM t)",
        "SELECT x.a FROM (SELECT * FROM t) x",
        "WITH w AS (SELECT b FROM t) SELECT 1 FROM w",
        "SELECT EXISTS (SELECT 1 FROM t WHERE b = 1)",
        "SELECT t1.a FROM t t1 NATURAL JOIN t t2",
        "SELECT a, upper(c) FROM t",
        "SELECT a FROM t UNION SELECT b FROM t",
        // u: no privilege at all.
        "SELECT t.a FROM t JOIN u ON u.id = t.a",
        "SELECT a FROM t WHERE a IN (SELECT id FROM u)",
    ] {
        refused(&carol, sql);
    }
    // A whole-row reference, which only the simple protocol gets as far as checking.
    for sql in ["SELECT t FROM t", "SELECT count(t) FROM t"] {
        assert_sqlstate(&carol.exec(sql, &[]).unwrap_err(), "42501");
    }
    // The row's id, which is the key of a table whose key is an INTEGER PRIMARY KEY.
    must_exec(&admin, "CREATE TABLE w (id integer PRIMARY KEY, a int)");
    must_exec(&admin, "INSERT INTO w VALUES (42, 1)");
    must_exec(&admin, "GRANT SELECT (a) ON w TO carol");
    allowed(&carol, "SELECT a FROM w");
    for sql in [
        "SELECT rowid FROM w",
        "SELECT _rowid_ FROM w",
        "SELECT ctid FROM w",
        "SELECT a FROM w WHERE rowid = 42",
    ] {
        refused(&carol, sql);
    }
    // A parameter changes nothing.
    let n: i64 = carol
        .query_row("SELECT count(*) FROM t WHERE a = $1", &[&1_i32])
        .scan()
        .unwrap();
    assert_eq!(n, 1);
    let e = carol
        .query("SELECT a FROM t WHERE b = $1", &[&2_i32])
        .expect_err("b is not carol's");
    assert_pg_error(&e, "42501", "permission denied for table t");

    // COPY, with a column list or not.
    assert_eq!(copy_out(&carol, "COPY t (a) TO STDOUT"), Ok("1\n".to_string()));
    assert_eq!(
        copy_out(&carol, "COPY (SELECT a FROM t) TO STDOUT"),
        Ok("1\n".to_string())
    );
    assert_eq!(copy_out(&carol, "COPY t TO STDOUT"), Err("42501".to_string()));
    assert_eq!(copy_out(&carol, "COPY t (b) TO STDOUT"), Err("42501".to_string()));
    assert_eq!(
        copy_out(&carol, "COPY (SELECT b FROM t) TO STDOUT"),
        Err("42501".to_string())
    );
}

#[test]
fn insert_names_only_granted_columns() {
    let (_addr, admin, carol) = setup();
    must_exec(&admin, "GRANT SELECT (a), INSERT (a) ON t TO carol");
    for sql in [
        "INSERT INTO t (a) VALUES (5)",
        "INSERT INTO t DEFAULT VALUES",
        "INSERT INTO t (a) VALUES (5) RETURNING a",
        "INSERT INTO t (a) SELECT a FROM t",
    ] {
        allowed(&carol, sql);
    }
    for sql in [
        "INSERT INTO t VALUES (6, 1)",
        "INSERT INTO t (b) VALUES (1)",
        "INSERT INTO t (a, c) VALUES (1, 'z')",
        "INSERT INTO t (a) VALUES (5) RETURNING b",
        "INSERT INTO t (a) VALUES (5) RETURNING *",
        "INSERT INTO t (a) SELECT b FROM t",
        "INSERT INTO t (a) SELECT id FROM u",
    ] {
        refused(&carol, sql);
    }
    assert_eq!(copy_in(&carol, "COPY t (a) FROM STDIN", "8\n"), Ok(()));
    assert_eq!(
        copy_in(&carol, "COPY t FROM STDIN", "8\t8\tz\n"),
        Err("42501".to_string())
    );
    assert_eq!(
        copy_in(&carol, "COPY t (b) FROM STDIN", "8\n"),
        Err("42501".to_string())
    );
    let b7: i64 = admin
        .query_row("SELECT count(*) FROM t WHERE b = 7", &[])
        .scan()
        .unwrap();
    assert!(b7 > 0, "defaults filled the columns carol did not name");
}

#[test]
fn update_sets_and_reads_only_granted_columns() {
    let (addr, admin, carol) = setup();
    must_exec(&admin, "GRANT SELECT (a), UPDATE (c) ON t TO carol");
    for sql in [
        "UPDATE t SET c = 'y'",
        "UPDATE t SET c = 'y' WHERE a = 1",
        "UPDATE t SET c = 'q' RETURNING a",
    ] {
        allowed(&carol, sql);
    }
    for sql in [
        "UPDATE t SET c = 'y' WHERE b = 2",
        "UPDATE t SET b = 3",
        "UPDATE t SET c = c || 'z'",
        "UPDATE t SET c = 'q' RETURNING *",
        "UPDATE t SET c = 'q' FROM u WHERE u.id = t.a",
        "DELETE FROM t WHERE a = 1",
        "TRUNCATE t",
    ] {
        refused(&carol, sql);
    }
    assert_eq!(
        query_column(&admin, "SELECT b::text FROM t", 0),
        ["2"],
        "no refused statement wrote"
    );

    // A table-level UPDATE or DELETE still needs SELECT on the columns it reads.
    let dave = connect_as(&addr, "dave", "d");
    must_exec(&admin, "GRANT UPDATE, DELETE ON t TO dave");
    must_exec(&admin, "GRANT SELECT (a) ON t TO dave");
    allowed(&dave, "UPDATE t SET b = 1 WHERE a = 1");
    allowed(&dave, "UPDATE t SET b = 1");
    allowed(&dave, "DELETE FROM t WHERE a = 100");
    refused(&dave, "UPDATE t SET b = 1 WHERE c = 'x'");
    refused(&dave, "DELETE FROM t WHERE b = 100");
    refused(&dave, "DELETE FROM t WHERE a = 100 RETURNING b");
    allowed(&dave, "DELETE FROM t");
}

#[test]
fn on_conflict_needs_the_columns_it_reads() {
    let (_addr, admin, carol) = setup();
    must_exec(&admin, "CREATE TABLE m (a int PRIMARY KEY, b int, c text)");
    must_exec(&admin, "INSERT INTO m VALUES (1, 2, 'x')");
    must_exec(&admin, "GRANT SELECT (a), INSERT (a, b), UPDATE (b) ON m TO carol");
    refused(
        &carol,
        "INSERT INTO m (a, b) VALUES (1, 9) ON CONFLICT (a) DO UPDATE SET b = excluded.b",
    );
    must_exec(&admin, "GRANT SELECT (b) ON m TO carol");
    allowed(
        &carol,
        "INSERT INTO m (a, b) VALUES (1, 9) ON CONFLICT (a) DO UPDATE SET b = excluded.b",
    );
    allowed(&carol, "INSERT INTO m (a) VALUES (1) ON CONFLICT DO NOTHING");
    refused(
        &carol,
        "INSERT INTO m (a, b) VALUES (1, 9) ON CONFLICT (a) DO UPDATE SET b = 5 WHERE m.c = 'x'",
    );
}

#[test]
fn table_grants_and_revokes_against_column_grants() {
    let (_addr, admin, carol) = setup();
    // A table grant covers every column, and a column REVOKE leaves it be.
    must_exec(&admin, "GRANT SELECT ON t TO carol");
    must_exec(&admin, "REVOKE SELECT (b) ON t FROM carol");
    allowed(&carol, "SELECT * FROM t");
    // A table REVOKE takes the column grants of that privilege too.
    must_exec(&admin, "GRANT SELECT (a), UPDATE (a) ON t TO carol");
    must_exec(&admin, "REVOKE UPDATE ON t FROM carol");
    assert_eq!(attacl(&admin, "t"), ["a:{carol=r/postgres}", "b:", "c:"]);
    must_exec(&admin, "REVOKE SELECT ON t FROM carol");
    assert_eq!(attacl(&admin, "t"), ["a:", "b:", "c:"]);
    refused(&carol, "SELECT a FROM t");
    // ALL with columns is SELECT, INSERT, UPDATE and REFERENCES on them.
    must_exec(&admin, "GRANT ALL (b) ON t TO carol");
    must_exec(&admin, "GRANT REFERENCES (a) ON t TO carol");
    assert_eq!(
        attacl(&admin, "t"),
        ["a:{carol=x/postgres}", "b:{carol=arwx/postgres}", "c:"]
    );
    allowed(&carol, "SELECT b FROM t");
    allowed(&carol, "UPDATE t SET b = b + 1 WHERE b > 0");
    refused(&carol, "SELECT a FROM t");
    must_exec(&admin, "REVOKE ALL ON t FROM carol");
    assert_eq!(attacl(&admin, "t"), ["a:", "b:", "c:"]);
    refused(&carol, "SELECT b FROM t");
}

#[test]
fn column_grant_errors() {
    let (addr, admin, carol) = setup();
    for (sql, code, message) in [
        (
            "GRANT DELETE (a) ON t TO carol",
            "0LP01",
            "invalid privilege type DELETE for column",
        ),
        (
            "GRANT TRUNCATE (a) ON t TO carol",
            "0LP01",
            "invalid privilege type TRUNCATE for column",
        ),
        (
            "GRANT SELECT (zz) ON t TO carol",
            "42703",
            "column \"zz\" of relation \"t\" does not exist",
        ),
        (
            "GRANT SELECT (\"A\") ON t TO carol",
            "42703",
            "column \"A\" of relation \"t\" does not exist",
        ),
    ] {
        let e = admin.exec(sql, &[]).expect_err(sql);
        assert_pg_error(&e, code, message);
    }
    assert_eq!(attacl(&admin, "t"), ["a:", "b:", "c:"], "nothing was granted");
    // Two columns, and two privileges with their own lists.
    must_exec(&admin, "GRANT SELECT (a, c), UPDATE (\"b\") ON t TO carol");
    assert_eq!(
        attacl(&admin, "t"),
        ["a:{carol=r/postgres}", "b:{carol=w/postgres}", "c:{carol=r/postgres}"]
    );
    // Only the owner grants them.
    let dave = connect_as(&addr, "dave", "d");
    refused(&dave, "GRANT SELECT (a) ON t TO dave");
    refused(&carol, "GRANT SELECT (a) ON t TO dave");
}

#[test]
fn column_grants_follow_the_table_and_go_with_the_column() {
    let (_addr, admin, carol) = setup();
    must_exec(&admin, "GRANT SELECT (a, c) ON t TO carol");
    must_exec(&admin, "ALTER TABLE t RENAME COLUMN a TO aa");
    allowed(&carol, "SELECT aa, c FROM t");
    refused(&carol, "SELECT b FROM t");
    must_exec(&admin, "ALTER TABLE t RENAME TO t2");
    allowed(&carol, "SELECT aa FROM t2");
    must_exec(&admin, "CREATE SCHEMA s");
    must_exec(&admin, "ALTER TABLE t2 SET SCHEMA s");
    allowed(&carol, "SELECT aa, c FROM s.t2");
    refused(&carol, "SELECT b FROM s.t2");
    assert_eq!(
        attacl(&admin, "s.t2"),
        ["aa:{carol=r/postgres}", "b:", "c:{carol=r/postgres}"]
    );
    // A column dropped takes its grants: one added by the same name has none.
    must_exec(&admin, "ALTER TABLE s.t2 DROP COLUMN c");
    must_exec(&admin, "ALTER TABLE s.t2 ADD COLUMN c text");
    assert_eq!(attacl(&admin, "s.t2"), ["aa:{carol=r/postgres}", "b:", "c:"]);
    refused(&carol, "SELECT c FROM s.t2");
    // The same through the extended protocol, and with IF EXISTS.
    must_exec(&admin, "GRANT SELECT (c) ON s.t2 TO carol");
    extended(&admin, "ALTER TABLE s.t2 DROP COLUMN IF EXISTS c").unwrap();
    extended(&admin, "ALTER TABLE s.t2 ADD COLUMN c text").unwrap();
    refused(&carol, "SELECT c FROM s.t2");
    // And DROP TABLE takes them all.
    must_exec(&admin, "DROP TABLE s.t2");
    must_exec(&admin, "CREATE TABLE s.t2 (aa int)");
    refused(&carol, "SELECT aa FROM s.t2");
    let n: i64 = admin
        .query_row("SELECT count(*) FROM _overlite_column_grants", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn catalogs_report_column_grants() {
    let (addr, admin, carol) = setup();
    must_exec(&admin, "CREATE ROLE \"Gus\" LOGIN PASSWORD 'g'");
    must_exec(&admin, "GRANT SELECT (a) ON t TO carol");
    must_exec(&admin, "GRANT INSERT (a), UPDATE (c) ON t TO carol");
    must_exec(&admin, "GRANT SELECT (a) ON t TO \"Gus\", PUBLIC");
    assert_eq!(
        attacl(&admin, "t"),
        [
            "a:{carol=ar/postgres,Gus=r/postgres,=r/postgres}",
            "b:",
            "c:{carol=w/postgres}"
        ]
    );
    let relacl: Option<String> = admin
        .query_row("SELECT relacl::text FROM pg_class WHERE relname = 't'", &[])
        .scan()
        .unwrap();
    assert_eq!(relacl, None, "a column grant leaves the table's ACL alone");

    let q = "SELECT grantor || ' ' || grantee || ' ' || table_schema || '.' || table_name || '.' || column_name \
             || ' ' || privilege_type || ' ' || is_grantable FROM information_schema.column_privileges \
             WHERE table_name = 't' ORDER BY grantee, column_name, privilege_type";
    let carols = [
        "postgres carol public.t.a INSERT NO",
        "postgres carol public.t.a SELECT NO",
        "postgres carol public.t.c UPDATE NO",
    ];
    let all = query_column(&admin, q, 0);
    for row in carols.iter().chain(&[
        "postgres Gus public.t.a SELECT NO",
        "postgres PUBLIC public.t.a SELECT NO",
    ]) {
        assert!(all.contains(&(*row).to_string()), "{row} in {all:?}");
    }
    // The owner holds every column privilege, grantable.
    for row in [
        "postgres postgres public.t.b REFERENCES YES",
        "postgres postgres public.t.c SELECT YES",
    ] {
        assert!(all.contains(&row.to_string()), "{row} in {all:?}");
    }
    assert_eq!(all.len(), 3 + 2 + 12);
    // carol sees what is granted to her or to PUBLIC.
    let mine = query_column(&carol, q, 0);
    assert_eq!(
        mine,
        [
            "postgres PUBLIC public.t.a SELECT NO",
            "postgres carol public.t.a INSERT NO",
            "postgres carol public.t.a SELECT NO",
            "postgres carol public.t.c UPDATE NO",
        ]
    );
    // A table grant shows as a privilege on every column.
    let dave = connect_as(&addr, "dave", "d");
    must_exec(&admin, "GRANT UPDATE ON t TO dave");
    assert_eq!(
        query_column(&dave, q, 0)
            .into_iter()
            .filter(|r| r.contains(" dave "))
            .collect::<Vec<_>>(),
        [
            "postgres dave public.t.a UPDATE NO",
            "postgres dave public.t.b UPDATE NO",
            "postgres dave public.t.c UPDATE NO",
        ]
    );

    let row = |conn: &Conn, q: &str| -> Vec<bool> {
        let rows = conn.query(q, &[]).unwrap_or_else(|e| panic!("{q}: {e}"));
        let r = rows.iter().next().expect("a row");
        (0..r.cells.len()).map(|i| r.get::<bool>(i).unwrap()).collect()
    };
    assert_eq!(
        row(
            &admin,
            "SELECT has_column_privilege('carol', 't', 'a', 'SELECT'), has_column_privilege('carol', 't', 'b', 'SELECT'), \
             has_column_privilege('carol', 't', 'c', 'UPDATE'), has_column_privilege('carol', 't', 'c', 'SELECT'), \
             has_any_column_privilege('carol', 't', 'SELECT'), has_any_column_privilege('carol', 't', 'INSERT'), \
             has_column_privilege('dave', 't', 'b', 'UPDATE'), has_column_privilege('postgres', 't', 'b', 'SELECT'), \
             has_table_privilege('carol', 't', 'SELECT')"
        ),
        [true, false, true, false, true, true, true, true, false]
    );
    assert_eq!(
        row(
            &carol,
            "SELECT has_column_privilege('t', 'a', 'SELECT'), has_column_privilege('t', 'b', 'SELECT'), \
             has_column_privilege('t', 'c', 'UPDATE'), has_column_privilege('public.t', 'a', 'select, update'), \
             has_column_privilege('t', 1::int2, 'SELECT'), has_column_privilege('t', 2::int2, 'SELECT'), \
             has_column_privilege('t', 'a', 'SELECT WITH GRANT OPTION')"
        ),
        [true, false, true, true, true, false, false]
    );
    let e = carol
        .query("SELECT has_any_column_privilege('t', 'DELETE')", &[])
        .unwrap_err();
    assert_pg_error(&e, "22023", "unrecognized privilege type: \"DELETE\"");
    let e = carol
        .query("SELECT has_column_privilege('t', 'zz', 'SELECT')", &[])
        .unwrap_err();
    assert_pg_error(&e, "42703", "column \"zz\" of relation \"t\" does not exist");
}

/// psql's \dp, as Postgres 17 prints it for the same grants.
#[test]
fn psql_dp_shows_column_privileges() {
    let Some(bin) = look_path("psql") else {
        skip_test!("psql not installed");
    };
    let (addr, admin, _carol) = setup();
    must_exec(&admin, "GRANT SELECT (a) ON t TO carol");
    must_exec(&admin, "GRANT INSERT (a), UPDATE (c) ON t TO carol");
    let port = addr.port().to_string();
    let out = Command::new(bin)
        .args([
            "-X",
            "-h",
            addr.host(),
            "-p",
            &port,
            "-U",
            "postgres",
            "-d",
            "test",
            "-c",
            "\\dp t",
        ])
        .env("PGPASSWORD", "adminpw")
        .output()
        .expect("psql must start");
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "psql failed: {text}");
    let want = r"                             Access privileges
 Schema | Name | Type  | Access privileges |  Column privileges  | Policies
--------+------+-------+-------------------+---------------------+----------
 public | t    | table |                   | a:                 +|
        |      |       |                   |   carol=ar/postgres+|
        |      |       |                   | c:                 +|
        |      |       |                   |   carol=w/postgres  |
(1 row)
";
    // Compared line by line without the spaces that end them.
    let lines = |s: &str| s.lines().map(|l| l.trim_end().to_string()).collect::<Vec<_>>();
    assert_eq!(lines(text.trim_end()), lines(want));
}

// A role renamed keeps its column grants, as Postgres keeps them by oid.
#[test]
fn column_grants_follow_a_renamed_role() {
    let (addr, admin, carol) = setup();
    drop(carol);
    must_exec(&admin, "GRANT SELECT (a) ON t TO carol");
    must_exec(&admin, "ALTER ROLE carol RENAME TO carla");
    must_exec(&admin, "ALTER ROLE carla PASSWORD 'c'");
    assert_eq!(attacl(&admin, "t"), ["a:{carla=r/postgres}", "b:", "c:"]);
    let carla = connect_as(&addr, "carla", "c");
    allowed(&carla, "SELECT a FROM t");
    refused(&carla, "SELECT b FROM t");
}

// Where the columns a statement touches cannot be told, a role holding only column
// privileges on its table is refused, and a table grant still lets it run.
#[test]
fn unknown_columns_fail_closed() {
    let (addr, admin, carol) = setup();
    must_exec(&admin, "GRANT SELECT (a) ON t TO carol");
    let dave = connect_as(&addr, "dave", "d");
    must_exec(&admin, "GRANT SELECT ON t TO dave");
    for sql in [
        // SQLite's LIMIT, which Postgres' grammar does not take.
        "SELECT a FROM t LIMIT 0, 1",
        // A node the reader does not walk, over the one column carol may read.
        "SELECT json_object('k': a) FROM t",
        "SELECT j.a FROM (t JOIN t t2 USING (a)) AS j",
    ] {
        refused(&carol, sql);
        allowed(&dave, sql);
    }
}

// What an older server wrote for a column GRANT is a table grant, and stays one: the
// row cannot be told from a real table grant (NOTES.md says how to review them).
#[test]
fn old_table_rows_stay_table_grants() {
    let (_addr, admin, carol) = setup();
    must_exec(&admin, "GRANT SELECT ON t TO carol");
    let rows = query_column(
        &admin,
        "SELECT grantee || ' ' || tablename || ' ' || privilege FROM _overlite_grants ORDER BY 1",
        0,
    );
    assert_eq!(rows, ["carol t SELECT"]);
    allowed(&carol, "SELECT * FROM t");
}
