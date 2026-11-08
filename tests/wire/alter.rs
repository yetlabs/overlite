use crate::common::*;

// alter_add_unique: ADD [CONSTRAINT] UNIQUE becomes a real unique index and is
// enforced.
#[test]
fn alter_add_unique() {
    let conn = connect(&start_server());
    conn.exec("CREATE TABLE t (id int, email text)", &[]).unwrap();
    conn.exec("INSERT INTO t VALUES (1,'a@x'),(2,'b@x')", &[]).unwrap();
    conn.exec("ALTER TABLE t ADD CONSTRAINT t_email_key UNIQUE (email)", &[])
        .unwrap();

    // The constraint is enforced.
    conn.exec("INSERT INTO t VALUES (3,'a@x')", &[])
        .expect_err("duplicate email rejected");
    // A new distinct value is fine.
    conn.exec("INSERT INTO t VALUES (3,'c@x')", &[]).unwrap();
}

// alter_column_type: ALTER COLUMN … TYPE rebuilds the table, preserving data,
// and re-advertises the new type.
#[test]
fn alter_column_type() {
    let conn = connect(&start_server());
    conn.exec("CREATE TABLE t (id int PRIMARY KEY, note text, price int)", &[])
        .unwrap();
    conn.exec("INSERT INTO t VALUES (1,'hi',100),(2,'yo',200)", &[])
        .unwrap();
    conn.exec("CREATE INDEX t_note ON t (note)", &[]).unwrap();

    conn.exec("ALTER TABLE t ALTER COLUMN price TYPE numeric", &[]).unwrap();

    // Data survived the rebuild.
    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 2);
    let (_id, p): (i64, i64) = conn
        .query_row("SELECT id, price FROM t WHERE id = 2", &[])
        .scan()
        .unwrap();
    assert_eq!(p, 200);
    // The PK still enforces uniqueness after the rebuild.
    conn.exec("INSERT INTO t VALUES (1,'dup',1)", &[])
        .expect_err("primary key preserved");
    // The recreated index still exists (a second ALTER that rebuilds again works).
    conn.exec("ALTER TABLE t ALTER COLUMN note TYPE varchar(50)", &[])
        .unwrap();
}

// alter_column_not_null: SET/DROP NOT NULL via rebuild.
#[test]
fn alter_column_not_null() {
    let conn = connect(&start_server());
    conn.exec("CREATE TABLE t (id int, name text)", &[]).unwrap();
    conn.exec("INSERT INTO t VALUES (1,'a')", &[]).unwrap();

    conn.exec("ALTER TABLE t ALTER COLUMN name SET NOT NULL", &[]).unwrap();
    conn.exec("INSERT INTO t (id) VALUES (2)", &[])
        .expect_err("NOT NULL enforced");

    conn.exec("ALTER TABLE t ALTER COLUMN name DROP NOT NULL", &[]).unwrap();
    conn.exec("INSERT INTO t (id) VALUES (2)", &[])
        .expect("NOT NULL dropped");
}

// alter_add_constraint_enforced covers the constraints a pg_dump restore adds
// after the fact: they used to be accepted and dropped on the floor, so a
// restored database kept none of them. ONLY and the public. qualifier are how
// pg_dump writes them.
#[test]
fn alter_add_constraint_enforced() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE dept (id int, name text)");
    must_exec(&conn, "CREATE TABLE emp (id int, dept_id int)");
    must_exec(
        &conn,
        "ALTER TABLE ONLY public.dept ADD CONSTRAINT dept_pkey PRIMARY KEY (id)",
    );
    must_exec(
        &conn,
        "ALTER TABLE ONLY public.dept ADD CONSTRAINT dept_name_key UNIQUE (name)",
    );
    must_exec(
        &conn,
        "ALTER TABLE ONLY public.emp ADD CONSTRAINT fk_emp_0 FOREIGN KEY (dept_id) REFERENCES public.dept(id)",
    );

    must_exec(&conn, "INSERT INTO dept VALUES (1, 'eng')");

    conn.exec("INSERT INTO dept VALUES (1, 'other')", &[])
        .expect_err("PRIMARY KEY must reject a duplicate id");
    conn.exec("INSERT INTO dept VALUES (2, 'eng')", &[])
        .expect_err("UNIQUE must reject a duplicate name");
    conn.exec("INSERT INTO emp VALUES (1, 99)", &[])
        .expect_err("FOREIGN KEY must reject a missing parent");

    conn.exec("INSERT INTO emp VALUES (1, 1)", &[])
        .expect("a row satisfying every constraint must go in");

    // The catalog has to report them as constraints, not as bare indexes, or
    // the next pg_dump emits an index where a constraint belongs.
    let got = query_column(
        &conn,
        "SELECT conname FROM pg_catalog.pg_constraint WHERE contype IN ('p','u','f') ORDER BY conname",
        0,
    );
    assert_contains!(got, "dept_name_key");
    assert_contains!(got, "fk_emp_0");
}

// enum_column_keeps_its_type pins the declared type of an enum column. It is
// stored as text plus a CHECK, so the catalog has to recover the enum from that
// CHECK — otherwise a dump writes `m text` and the restored column is no longer
// the enum.
#[test]
fn enum_column_keeps_its_type() {
    let addr = start_server();
    let c1 = connect(&addr);
    must_exec(&c1, "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')");
    must_exec(&c1, "CREATE TABLE t (id int, m mood, nota text)");

    let type_of = |c: &Conn, col: &str| -> (u32, String) {
        c.query_row(
            "SELECT atttypid, format_type(atttypid, atttypmod) FROM pg_catalog.pg_attribute
			 WHERE attrelid = 't'::regclass AND attname = $1",
            &[&col],
        )
        .scan()
        .unwrap()
    };

    // On the connection that created the type, whose registry was loaded before
    // the type existed...
    let (oid, name) = type_of(&c1, "m");
    assert!(oid > 90000000, "an enum column reports the enum's oid, not text's");
    assert_eq!(name, "mood");

    // ...and on a client that connects afterwards, which is what pg_dump is.
    let c2 = connect(&addr);
    let (_, name) = type_of(&c2, "m");
    assert_eq!(name, "mood");

    // A plain text column must not be mistaken for one.
    let (_, name) = type_of(&c2, "nota");
    assert_eq!(name, "text");
}

fn extended(conn: &Conn, sql: &str) -> Result<PgResult> {
    conn.exec_params::<&[u8]>(sql, &[], &[], &[], &[])
}

fn columns(conn: &Conn, table: &str) -> Vec<String> {
    query_column(
        conn,
        &format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"),
        0,
    )
}

// alter_is_noop: ALTER TABLE with these words in a literal or a quoted name is a real
// statement, not the restore-time no-op they mean as keywords.
#[test]
fn alter_noop_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int)");
    must_exec(&conn, "ALTER TABLE t ADD COLUMN a text DEFAULT ' OWNER TO '");
    extended(&conn, "ALTER TABLE t ADD COLUMN b text DEFAULT ' ADD CONSTRAINT '").unwrap();
    must_exec(&conn, r#"ALTER TABLE t ADD COLUMN " OWNER TO " text"#);
    assert_eq!(columns(&conn, "t"), ["id", "a", "b", " OWNER TO "]);
    let (a, b): (String, String) = conn
        .query_row("INSERT INTO t (id) VALUES (1) RETURNING a, b", &[])
        .scan()
        .unwrap();
    assert_eq!((a.as_str(), b.as_str()), (" OWNER TO ", " ADD CONSTRAINT "));
}

// has_word in try_alter_table and alter_table_handled: UNIQUE in a default is not a
// request for a unique index, and took the place of adding the column.
#[test]
fn alter_unique_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int)");
    must_exec(&conn, "ALTER TABLE t ADD COLUMN a text DEFAULT 'is unique (id)'");
    extended(&conn, "ALTER TABLE t ADD COLUMN b text DEFAULT 'be unique (id)'").unwrap();
    assert_eq!(columns(&conn, "t"), ["id", "a", "b"]);
    must_exec(&conn, "INSERT INTO t (id) VALUES (1), (1)");
}

// parse_add_constraint: NOT VALID in the expression is not the clause.
#[test]
fn constraint_not_valid_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (a text)");
    must_exec(&conn, "ALTER TABLE t ADD CONSTRAINT k CHECK (a <> ' NOT VALID x')");
    let err = conn.exec("INSERT INTO t VALUES (' NOT VALID x')", &[]).unwrap_err();
    assert_sqlstate(&err, "23514");
    must_exec(&conn, "ALTER TABLE t ADD CONSTRAINT k2 CHECK (a <> 'b') NOT VALID");
    assert_sqlstate(&conn.exec("INSERT INTO t VALUES ('b')", &[]).unwrap_err(), "23514");
}

// add_not_null, drop_not_null, drop_default: they read a column's clauses, where a
// literal or a CHECK can spell the same words.
#[test]
fn alter_column_clauses_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE t (id int, a text DEFAULT 'not null', b text NOT NULL DEFAULT 'not null',
            c text CHECK (c <> 'default') DEFAULT 'x', d text CHECK (d IS NOT NULL), e text CHECK (e IS NOT NULL) NOT NULL DEFAULT 'y')",
    );

    must_exec(&conn, "ALTER TABLE t ALTER COLUMN a SET NOT NULL");
    assert_sqlstate(
        &conn
            .exec("INSERT INTO t (id, a, d) VALUES (1, NULL, 'd')", &[])
            .unwrap_err(),
        "23502",
    );

    must_exec(&conn, "ALTER TABLE t ALTER COLUMN b DROP NOT NULL");
    must_exec(&conn, "ALTER TABLE t ALTER COLUMN c DROP DEFAULT");
    must_exec(&conn, "ALTER TABLE t ALTER COLUMN e DROP NOT NULL");
    must_exec(&conn, "ALTER TABLE t ALTER COLUMN d SET NOT NULL");
    let (b, c, e): (String, Option<String>, String) = conn
        .query_row("INSERT INTO t (id, d) VALUES (2, 'd') RETURNING b, c, e", &[])
        .scan()
        .unwrap();
    assert_eq!(b, "not null", "the default stays");
    assert_eq!(c, None, "the default goes");
    assert_eq!(e, "y");
    assert_sqlstate(
        &conn
            .exec("INSERT INTO t (id, d, c) VALUES (3, 'd', 'default')", &[])
            .unwrap_err(),
        "23514",
    );
    must_exec(&conn, "INSERT INTO t (id, b, d, e) VALUES (4, NULL, 'd', 'x')");
    assert_sqlstate(
        &conn
            .exec("INSERT INTO t (id, d, e) VALUES (5, 'd', NULL)", &[])
            .unwrap_err(),
        "23514",
    );
}

// IF EXISTS and ONLY are not part of what SQLite runs, on the forms it runs itself as
// on the ones the session takes over, by either protocol.
#[test]
fn alter_table_if_exists_and_only() {
    let addr = start_server();
    let conn = connect(&addr);
    let mut raw = raw_conn(&addr, "overlite", "");
    for simple in [true, false] {
        must_exec(&conn, "DROP TABLE IF EXISTS t");
        must_exec(&conn, "DROP SCHEMA IF EXISTS s1 CASCADE");
        must_exec(&conn, "CREATE TABLE t (id int)");
        must_exec(&conn, "CREATE SCHEMA s1");
        must_exec(&conn, "CREATE TABLE s1.t (id int)");
        for sql in [
            "ALTER TABLE IF EXISTS t ADD COLUMN a int",
            "ALTER TABLE ONLY t ADD COLUMN b int",
            "ALTER TABLE IF EXISTS ONLY t ADD c int",
            "ALTER TABLE ONLY public.t RENAME COLUMN c TO d",
            "ALTER TABLE IF EXISTS public.t RENAME d TO e",
            "ALTER TABLE ONLY t DROP COLUMN b",
            "ALTER TABLE IF EXISTS t DROP e",
            "ALTER TABLE IF EXISTS s1.t ADD COLUMN x int",
            "ALTER TABLE ONLY s1.t RENAME COLUMN x TO y",
            "ALTER TABLE IF EXISTS ONLY \"s1\".\"t\" ADD COLUMN z int",
        ] {
            let tag = if simple {
                conn.exec(sql, &[]).map(|t| t.as_str().to_string())
            } else {
                extended_exec(&mut raw, sql)
            };
            assert_eq!(tag.unwrap_or_else(|e| panic!("{sql}: {e}")), "ALTER TABLE", "{sql}");
        }
        let cols = query_column(
            &conn,
            "SELECT column_name FROM information_schema.columns WHERE table_name = 't' AND table_schema = 'public' ORDER BY ordinal_position",
            0,
        );
        assert_eq!(cols, ["id", "a"]);
        let cols = query_column(
            &conn,
            "SELECT column_name FROM information_schema.columns WHERE table_name = 't' AND table_schema = 's1' ORDER BY ordinal_position",
            0,
        );
        assert_eq!(cols, ["id", "y", "z"]);
    }
}

// IF EXISTS on a table that is not there succeeds, whatever the action.
#[test]
fn alter_table_if_exists_missing() {
    let addr = start_server();
    let conn = connect(&addr);
    let mut raw = raw_conn(&addr, "overlite", "");
    for sql in [
        "ALTER TABLE IF EXISTS missing ADD COLUMN x int",
        "ALTER TABLE IF EXISTS ONLY missing ADD COLUMN x int",
        "ALTER TABLE IF EXISTS missing DROP COLUMN x",
        "ALTER TABLE IF EXISTS missing RENAME COLUMN a TO b",
        "ALTER TABLE IF EXISTS missing RENAME TO other",
        "ALTER TABLE IF EXISTS missing OWNER TO postgres",
        "ALTER TABLE IF EXISTS missing ALTER COLUMN a SET NOT NULL",
        "ALTER TABLE IF EXISTS missing ALTER COLUMN a TYPE text",
        "ALTER TABLE IF EXISTS missing SET SCHEMA s1",
        "ALTER TABLE IF EXISTS missing ADD COLUMN a int, ADD COLUMN b int",
        "ALTER TABLE IF EXISTS missing ADD CONSTRAINT c UNIQUE (a)",
        "ALTER TABLE IF EXISTS missing DROP CONSTRAINT c",
        "ALTER TABLE IF EXISTS missing ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE IF EXISTS nope.missing ADD COLUMN x int",
        "ALTER TABLE IF EXISTS \"Missing\" ADD COLUMN x int",
    ] {
        let tag = conn.exec(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(tag.as_str(), "ALTER TABLE", "{sql}");
        let tag = extended_exec(&mut raw, sql).unwrap_or_else(|e| panic!("extended {sql}: {e}"));
        assert_eq!(tag, "ALTER TABLE", "extended {sql}");
    }
    let n: i64 = conn
        .query_row("SELECT count(*) FROM _overlite_rls", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0, "nothing is recorded for a table that is not there");

    // Without IF EXISTS it is an error, ONLY or not.
    let sql = "ALTER TABLE ONLY missing ADD COLUMN x int";
    assert_sqlstate(&conn.exec(sql, &[]).unwrap_err(), "42P01");
    assert_sqlstate(&extended_exec(&mut raw, sql).unwrap_err(), "42P01");
}

// The notice Postgres sends a client of IF EXISTS on a missing table, with the tag
// behind it.
#[test]
fn alter_table_if_exists_notice() {
    let addr = start_server();
    let mut raw = raw_conn(&addr, "overlite", "");

    raw.send_query("ALTER TABLE IF EXISTS missing ADD COLUMN x int");
    raw.flush().unwrap();
    let Backend::NoticeResponse(notice) = raw.receive().unwrap() else {
        panic!("expected a notice");
    };
    assert_eq!(notice.severity, "NOTICE");
    assert_eq!(notice.code, "00000");
    assert_eq!(notice.message, "relation \"missing\" does not exist, skipping");
    let Backend::CommandComplete(tag) = raw.receive().unwrap() else {
        panic!("expected the tag");
    };
    assert_eq!(tag, "ALTER TABLE");
    raw.wait_ready().unwrap();

    // Parse, Bind and Execute get the same answer.
    raw.send_parse("", "ALTER TABLE IF EXISTS missing RENAME TO other", &[]);
    raw.send_bind("", "", &[], &[], &[]);
    raw.send_execute("", 0);
    raw.send_sync();
    raw.flush().unwrap();
    let notice = raw.wait_for(|m| matches!(m, Backend::NoticeResponse(_))).unwrap();
    let Backend::NoticeResponse(notice) = notice else {
        unreachable!();
    };
    assert_eq!(notice.message, "relation \"missing\" does not exist, skipping");
    raw.wait_ready().unwrap();
}

// The DEFAULT that makes ADD COLUMN a rebuild is the clause, not a word in a literal
// or a name.
#[test]
fn add_column_default_is_the_clause() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int)");
    must_exec(&conn, "INSERT INTO t VALUES (1)");

    // A quoted name that spells it, ahead of the clause.
    must_exec(&conn, r#"ALTER TABLE t ADD COLUMN "default" text DEFAULT lower('X')"#);
    let v: String = conn.query_row(r#"SELECT "default" FROM t"#, &[]).scan().unwrap();
    assert_eq!(v, "x");

    // A literal that reads like a computed default.
    must_exec(
        &conn,
        "ALTER TABLE t ADD COLUMN note text CHECK (note <> 'default (1)')",
    );
    let err = conn
        .exec("INSERT INTO t (id, note) VALUES (2, 'default (1)')", &[])
        .unwrap_err();
    assert_sqlstate(&err, "23514");
}

// A rebuild writes the table's name back into statements, so whatever needs quoting has
// to stay quoted, on every path that rebuilds: a computed default, SET/DROP NOT NULL,
// SET/DROP DEFAULT, a constraint in and out, and a foreign key pointing at the table.
#[test]
fn rebuild_keeps_a_name_that_needs_quotes() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE SCHEMA \"Sch\"");
    for (i, name) in [
        r#""add""#,
        r#""order""#,
        r#""select""#,
        r#""table""#,
        r#""Mixed Case""#,
        r#""we""ird""#,
        r#""a.b""#,
        r#""Sch"."add""#,
        r#""Sch"."Mixed Case""#,
    ]
    .into_iter()
    .enumerate()
    {
        must_exec(
            &conn,
            &format!("CREATE TABLE {name} (id int PRIMARY KEY, a int, b text)"),
        );
        must_exec(&conn, &format!("CREATE INDEX i{i} ON {name} (a)"));
        must_exec(&conn, &format!("INSERT INTO {name} (id, a) VALUES (1, 2)"));
        for action in [
            "ADD COLUMN c timestamptz DEFAULT now()",
            "ADD COLUMN d text DEFAULT lower('X')",
            "ALTER COLUMN a SET NOT NULL",
            "ALTER COLUMN a DROP NOT NULL",
            "ALTER COLUMN b SET DEFAULT 'x'",
            "ALTER COLUMN b DROP DEFAULT",
            "ALTER COLUMN a TYPE bigint",
            "ADD CONSTRAINT a_pos CHECK (a > 0)",
            "DROP CONSTRAINT a_pos",
            "ADD CONSTRAINT a_uq UNIQUE (a)",
            "DROP CONSTRAINT a_uq",
            "DROP COLUMN d",
        ] {
            let sql = format!("ALTER TABLE {name} {action}");
            conn.exec(&sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        let q = format!("SELECT a FROM {name}");
        assert_eq!(conn.query_row(&q, &[]).scan::<i64>().unwrap(), 2, "{name}");
        must_exec(&conn, &format!("INSERT INTO {name} (id, a) VALUES (2, 3)"));
        must_exec(&conn, &format!("DROP TABLE {name}"));
    }
}

// A table referenced by a foreign key keeps the reference through a rebuild of either.
#[test]
fn rebuild_with_a_foreign_key_on_a_reserved_name() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE "user" (id int PRIMARY KEY)"#);
    must_exec(
        &conn,
        r#"CREATE TABLE "group" (id int PRIMARY KEY, uid int REFERENCES "user"(id))"#,
    );
    must_exec(&conn, r#"INSERT INTO "user" VALUES (1)"#);
    must_exec(&conn, r#"INSERT INTO "group" VALUES (1, 1)"#);
    must_exec(&conn, r#"ALTER TABLE "user" ADD COLUMN at timestamptz DEFAULT now()"#);
    must_exec(&conn, r#"ALTER TABLE "group" ADD COLUMN at timestamptz DEFAULT now()"#);
    assert_sqlstate(
        &conn
            .exec(r#"INSERT INTO "group" (id, uid) VALUES (2, 99)"#, &[])
            .unwrap_err(),
        "23503",
    );
    assert_sqlstate(&conn.exec(r#"DELETE FROM "user""#, &[]).unwrap_err(), "23503");
}

/// The notices of a Query message.
fn notices_of(fe: &mut RawConn, sql: &str) -> Vec<(String, String)> {
    fe.send_query(sql);
    fe.flush().unwrap();
    let mut out = Vec::new();
    loop {
        match fe.receive().unwrap() {
            Backend::NoticeResponse(n) => out.push((n.code, n.message)),
            Backend::ReadyForQuery(_) => return out,
            _ => {}
        }
    }
}

// One statement, one notice for a table that is not there, however many actions it
// has; and each action that skips itself says so, as in Postgres.
#[test]
fn alter_notices() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE t (a int)");
    let mut fe = RawConn::dial(&server).unwrap();
    fe.startup("postgres", "test", "").unwrap();

    let missing = (
        "00000".to_string(),
        r#"relation "nope" does not exist, skipping"#.to_string(),
    );
    assert_eq!(
        notices_of(&mut fe, "ALTER TABLE IF EXISTS nope ADD COLUMN a int, ADD COLUMN b int"),
        std::slice::from_ref(&missing)
    );
    assert_eq!(
        notices_of(&mut fe, "ALTER TABLE IF EXISTS nope ADD COLUMN a int"),
        std::slice::from_ref(&missing)
    );
    // Two statements are two notices.
    assert_eq!(
        notices_of(
            &mut fe,
            "ALTER TABLE IF EXISTS nope ADD COLUMN a int, ADD b int; ALTER TABLE IF EXISTS nope ADD c int"
        ),
        [missing.clone(), missing]
    );
    assert_eq!(
        notices_of(
            &mut fe,
            "ALTER TABLE t DROP COLUMN IF EXISTS x, DROP COLUMN IF EXISTS y"
        ),
        [
            (
                "00000".to_string(),
                r#"column "x" of relation "t" does not exist, skipping"#.to_string()
            ),
            (
                "00000".to_string(),
                r#"column "y" of relation "t" does not exist, skipping"#.to_string()
            ),
        ]
    );
    assert_eq!(
        notices_of(
            &mut fe,
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS a int, ADD COLUMN IF NOT EXISTS b int"
        ),
        [(
            "42701".to_string(),
            r#"column "a" of relation "t" already exists, skipping"#.to_string()
        )]
    );
}
