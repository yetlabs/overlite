use crate::common::*;

// Prisma drops a relation with ALTER TABLE … DROP CONSTRAINT before dropping
// its table; SQLite refused it ("constraint may not be dropped").
#[test]
fn drop_constraint() {
    let conn = connect_extended(&start_server());

    must_exec(
        &conn,
        r#"CREATE TABLE "Instance" ("id" TEXT NOT NULL, CONSTRAINT "Instance_pkey" PRIMARY KEY ("id"))"#,
    );
    must_exec(
        &conn,
        r#"CREATE TABLE "Template" ("id" TEXT NOT NULL, "instanceId" TEXT NOT NULL, "name" TEXT, CONSTRAINT "Template_pkey" PRIMARY KEY ("id"))"#,
    );
    must_exec(&conn, r#"CREATE INDEX "Template_name_idx" ON "Template" ("name")"#);
    must_exec(
        &conn,
        r#"ALTER TABLE "Template" ADD CONSTRAINT "Template_instanceId_fkey" FOREIGN KEY ("instanceId") REFERENCES "Instance"("id") ON DELETE CASCADE ON UPDATE CASCADE"#,
    );
    must_exec(&conn, r#"INSERT INTO "Instance" VALUES ('i1')"#);
    must_exec(&conn, r#"INSERT INTO "Template" VALUES ('t1', 'i1', 'a')"#);

    // With the key in place, a row pointing nowhere is refused.
    conn.exec(r#"INSERT INTO "Template" VALUES ('t2', 'nowhere', 'b')"#, &[])
        .expect_err("the foreign key holds before the drop");

    must_exec(
        &conn,
        r#"ALTER TABLE "Template" DROP CONSTRAINT "Template_instanceId_fkey""#,
    );

    // The key is gone, the rows and the index stayed, the primary key holds.
    must_exec(&conn, r#"INSERT INTO "Template" VALUES ('t2', 'nowhere', 'b')"#);
    let n: i64 = conn
        .query_row(r#"SELECT count(*) FROM "Template""#, &[])
        .scan()
        .unwrap();
    assert_eq!(n, 2);
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'Template_name_idx'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1, "indexes survive the rebuild");
    conn.exec(r#"INSERT INTO "Template" VALUES ('t1', 'i1', 'dup')"#, &[])
        .expect_err("the primary key is still there");

    // A UNIQUE constraint added by ALTER is an index of its name.
    must_exec(
        &conn,
        r#"ALTER TABLE "Template" ADD CONSTRAINT "Template_name_key" UNIQUE ("name")"#,
    );
    must_exec(&conn, r#"ALTER TABLE "Template" DROP CONSTRAINT "Template_name_key""#);
    must_exec(&conn, r#"INSERT INTO "Template" VALUES ('t3', 'i1', 'a')"#);

    // IF EXISTS on a missing one is no error; drop_constraint_missing has the rest.
    must_exec(
        &conn,
        r#"ALTER TABLE "Template" DROP CONSTRAINT IF EXISTS "nothing_here""#,
    );

    // And the migration's next step works.
    must_exec(&conn, r#"DROP TABLE "Template""#);
}

fn accepted(conn: &Conn, sql: &str) -> bool {
    conn.exec(sql, &[]).is_ok()
}

// A constraint written on its column is as droppable as one written apart.
#[test]
fn drop_column_constraint() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE p (id int PRIMARY KEY);
        CREATE TABLE t (
            a int DEFAULT 1 CONSTRAINT colk UNIQUE NOT NULL,
            b text CONSTRAINT "my col key" UNIQUE,
            c int CONSTRAINT colck CHECK (c > 0) DEFAULT 5,
            d int CONSTRAINT colfk REFERENCES p (id) ON DELETE SET NULL NOT DEFERRABLE CHECK (d <> 9)
        )"#,
    );
    let ins = |vals: &str| format!("INSERT INTO t (a, b, c, d) VALUES {vals}");
    must_exec(&conn, "INSERT INTO p VALUES (1)");
    must_exec(&conn, &ins("(1, 'x', 1, 1)"));

    assert!(!accepted(&conn, &ins("(1, 'y', 1, 1)")));
    must_exec(&conn, "ALTER TABLE t DROP CONSTRAINT colk");
    must_exec(&conn, &ins("(1, 'y', 1, 1)"));
    assert!(!accepted(&conn, &ins("(NULL, 'z', 1, 1)")), "NOT NULL beside it stays");

    assert!(!accepted(&conn, &ins("(2, 'x', 1, 1)")));
    must_exec(&conn, r#"ALTER TABLE t DROP CONSTRAINT "my col key""#);
    must_exec(&conn, &ins("(2, 'x', 1, 1)"));

    assert!(!accepted(&conn, &ins("(3, 'w', -1, 1)")));
    must_exec(&conn, "ALTER TABLE t DROP CONSTRAINT colck");
    must_exec(&conn, &ins("(3, 'w', -1, 1)"));
    let c: i64 = conn
        .query_row("INSERT INTO t (a) VALUES (4) RETURNING c", &[])
        .scan()
        .unwrap();
    assert_eq!(c, 5, "the DEFAULT after it stays");

    assert!(!accepted(&conn, &ins("(5, 'v', 1, 7)")));
    must_exec(&conn, "ALTER TABLE t DROP CONSTRAINT colfk");
    must_exec(&conn, &ins("(5, 'v', 1, 7)"));
    assert!(!accepted(&conn, &ins("(6, 'u', 1, 9)")), "the CHECK after it stays");
}

// A table constraint named with a space in it.
#[test]
fn drop_constraint_quoted_name() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE t (a int, b int, CONSTRAINT "my key" UNIQUE (a))"#,
    );
    must_exec(&conn, "INSERT INTO t VALUES (1, 1)");
    assert!(!accepted(&conn, "INSERT INTO t VALUES (1, 2)"));
    must_exec(&conn, r#"ALTER TABLE t DROP CONSTRAINT "my key""#);
    must_exec(&conn, "INSERT INTO t VALUES (1, 2)");

    // The same, added afterwards.
    must_exec(&conn, r#"ALTER TABLE t ADD CONSTRAINT "other key" UNIQUE (b)"#);
    assert!(!accepted(&conn, "INSERT INTO t VALUES (2, 1)"));
    must_exec(&conn, r#"ALTER TABLE t DROP CONSTRAINT "other key""#);
    must_exec(&conn, "INSERT INTO t VALUES (2, 1)");
}

// What Postgres would have named an unnamed one, which is what pg_constraint shows.
#[test]
fn drop_constraint_default_names() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE p (id int PRIMARY KEY);
        CREATE TABLE t (id int PRIMARY KEY, a int UNIQUE, b int REFERENCES p (id), c int CHECK (c > 0),
            d int, e int, f int, g int, UNIQUE (d, e), FOREIGN KEY (f) REFERENCES p (id),
            CHECK (g > 0), CHECK (g > d))",
    );
    let names = query_column(
        &conn,
        "SELECT conname FROM pg_constraint WHERE conrelid = 't'::regclass AND contype IN ('p', 'f') ORDER BY 1",
        0,
    );
    assert_eq!(names, ["t_b_fkey", "t_f_fkey", "t_pkey"]);
    must_exec(&conn, "INSERT INTO p VALUES (1)");
    must_exec(&conn, "INSERT INTO t (id, a, d, e) VALUES (1, 1, 1, 1)");

    for name in [
        "t_pkey",
        "t_a_key",
        "t_b_fkey",
        "t_c_check",
        "t_d_e_key",
        "t_f_fkey",
        "t_g_check",
        "t_check",
    ] {
        must_exec(&conn, &format!("ALTER TABLE t DROP CONSTRAINT {name}"));
    }
    must_exec(
        &conn,
        "INSERT INTO t (id, a, b, c, d, e, f, g) VALUES (1, 1, 7, -1, 1, 1, 7, -1)",
    );
}

// Postgres refuses a constraint that is not there, unless told IF EXISTS.
#[test]
fn drop_constraint_missing() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE SCHEMA s1");
    must_exec(&conn, "CREATE TABLE t (a int)");
    must_exec(&conn, "CREATE TABLE s1.t (a int, CONSTRAINT k UNIQUE (a))");

    let err = conn.exec("ALTER TABLE t DROP CONSTRAINT nope", &[]).unwrap_err();
    assert_pg_error(&err, "42704", r#"constraint "nope" of relation "t" does not exist"#);
    must_exec(&conn, "ALTER TABLE t DROP CONSTRAINT IF EXISTS nope");

    let err = conn.exec("ALTER TABLE nowhere DROP CONSTRAINT nope", &[]).unwrap_err();
    assert_sqlstate(&err, "42P01");
    must_exec(&conn, "ALTER TABLE IF EXISTS nowhere DROP CONSTRAINT nope");

    // And in its own schema the table is found.
    must_exec(&conn, "INSERT INTO s1.t VALUES (1)");
    must_exec(&conn, "ALTER TABLE s1.t DROP CONSTRAINT k");
    must_exec(&conn, "INSERT INTO s1.t VALUES (1)");
}

// A constraint belongs to its table: an index of that name elsewhere is no match.
#[test]
fn drop_constraint_stays_on_its_table() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE p8 (a int)");
    must_exec(&conn, "CREATE TABLE p9 (a int)");
    must_exec(&conn, "CREATE UNIQUE INDEX shared_idx ON p9 (a)");
    must_exec(&conn, "CREATE INDEX plain_idx ON p8 (a)");

    let err = conn.exec("ALTER TABLE p8 DROP CONSTRAINT shared_idx", &[]).unwrap_err();
    assert_pg_error(&err, "42704", "shared_idx");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'shared_idx'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "the index of the other table is gone");

    // A plain index is no constraint, on its own table either.
    let err = conn.exec("ALTER TABLE p8 DROP CONSTRAINT plain_idx", &[]).unwrap_err();
    assert_sqlstate(&err, "42704");

    // The unique index of the table itself is what ADD UNIQUE may have left.
    must_exec(&conn, "ALTER TABLE p9 DROP CONSTRAINT shared_idx");
    must_exec(&conn, "INSERT INTO p9 VALUES (1), (1)");
}

const FK_REFUSED: &str = "2BP01";

// A key another table's foreign key stands on: Postgres refuses, and CASCADE takes
// the foreign key with it. SQLite would answer "foreign key mismatch" from then on.
#[test]
fn drop_referenced_key() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE p (id int, code int CONSTRAINT p_code UNIQUE, spare int UNIQUE, CONSTRAINT p_key PRIMARY KEY (id));
        CREATE TABLE c1 (id int PRIMARY KEY, pid int, CONSTRAINT c1_fk FOREIGN KEY (pid) REFERENCES p (id));
        CREATE TABLE c2 (id int PRIMARY KEY, pid int REFERENCES p, code int CONSTRAINT c2_fk REFERENCES p (code) ON DELETE CASCADE);
        CREATE INDEX c1_pid ON c1 (pid);
        INSERT INTO p VALUES (1, 1, 1);
        INSERT INTO c1 VALUES (1, 1);
        INSERT INTO c2 VALUES (1, 1, 1);",
    );

    for sql in [
        "ALTER TABLE p DROP CONSTRAINT p_key",
        "ALTER TABLE p DROP CONSTRAINT p_key RESTRICT",
        "ALTER TABLE p DROP CONSTRAINT p_code",
    ] {
        let err = conn.exec(sql, &[]).unwrap_err();
        assert_pg_error(&err, FK_REFUSED, "because other objects depend on it");
    }
    must_exec(&conn, "INSERT INTO c1 VALUES (2, 1)"); // no mismatch: the key is there
    conn.exec("INSERT INTO c1 VALUES (3, 9)", &[])
        .expect_err("the key still holds");

    // A key nothing references goes without CASCADE.
    must_exec(&conn, "ALTER TABLE p DROP CONSTRAINT p_spare_key");

    must_exec(&conn, "ALTER TABLE p DROP CONSTRAINT p_code CASCADE");
    must_exec(&conn, "INSERT INTO c2 VALUES (2, 1, 9)"); // c2_fk is gone
    conn.exec("INSERT INTO c2 VALUES (3, 9, 1)", &[])
        .expect_err("the other foreign key of c2 is not");

    must_exec(&conn, "ALTER TABLE p DROP CONSTRAINT p_key CASCADE");
    must_exec(&conn, "INSERT INTO c1 VALUES (3, 9)");
    must_exec(&conn, "INSERT INTO c2 VALUES (3, 9, 1)");
    must_exec(&conn, "INSERT INTO p VALUES (1, 2, 1)"); // the key itself is gone
    let n: i64 = conn.query_row("SELECT count(*) FROM c1", &[]).scan().unwrap();
    assert_eq!(n, 3, "the rows of the child survive its rebuild");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'c1_pid'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "and so does its index");
}

// A table referencing itself, a schema, and a transaction that rolls it all back.
#[test]
fn drop_referenced_key_cascade_edges() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE tree (id int PRIMARY KEY, up int REFERENCES tree (id));
        INSERT INTO tree VALUES (1, NULL), (2, 1);",
    );
    let err = conn
        .exec("ALTER TABLE tree DROP CONSTRAINT tree_pkey", &[])
        .unwrap_err();
    assert_sqlstate(&err, FK_REFUSED);
    must_exec(&conn, "ALTER TABLE tree DROP CONSTRAINT tree_pkey CASCADE");
    must_exec(&conn, "INSERT INTO tree VALUES (2, 9)");

    must_exec(
        &conn,
        "CREATE SCHEMA s1;
        CREATE TABLE s1.p (id int PRIMARY KEY);
        CREATE TABLE s1.c (pid int REFERENCES s1.p (id));
        INSERT INTO s1.p VALUES (1);",
    );
    let err = conn.exec("ALTER TABLE s1.p DROP CONSTRAINT p_pkey", &[]).unwrap_err();
    assert_sqlstate(&err, FK_REFUSED);

    let tx = conn.begin().unwrap();
    tx.exec("ALTER TABLE s1.p DROP CONSTRAINT p_pkey CASCADE", &[]).unwrap();
    tx.exec("INSERT INTO s1.c VALUES (9)", &[]).unwrap();
    tx.rollback().unwrap();
    conn.exec("INSERT INTO s1.c VALUES (9)", &[])
        .expect_err("undone with the transaction");
    must_exec(&conn, "INSERT INTO s1.c VALUES (1)");
}

// What a client reads from pg_constraint is a name it can drop by.
#[test]
fn drop_unique_by_its_catalog_name() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (a int, b int, CONSTRAINT k UNIQUE (a, b))");
    let name: String = conn
        .query_row(
            "SELECT conname FROM pg_constraint WHERE conrelid = 't'::regclass AND contype = 'u'",
            &[],
        )
        .scan()
        .unwrap();
    must_exec(&conn, &format!("ALTER TABLE t DROP CONSTRAINT {name}"));
    must_exec(&conn, "INSERT INTO t VALUES (1, 1), (1, 1)");
}

// Through an index rather than a constraint of the CREATE TABLE: a unique index
// holds the key a foreign key stands on, and goes with it the same way.
#[test]
fn drop_referenced_unique_index_cascade() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE p (id int PRIMARY KEY, code int);
        CREATE UNIQUE INDEX p_code_ui ON p (code);
        CREATE TABLE c (pid int REFERENCES p (id), code int REFERENCES p (code));
        INSERT INTO p VALUES (1, 10);
        INSERT INTO c VALUES (1, 10);",
    );
    conn.exec("INSERT INTO c VALUES (1, 99)", &[])
        .expect_err("the key holds");

    let err = conn.exec("ALTER TABLE p DROP CONSTRAINT p_code_ui", &[]).unwrap_err();
    assert_pg_error(&err, "2BP01", "because other objects depend on it");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'p_code_ui'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1, "refused, it stays");

    must_exec(&conn, "ALTER TABLE p DROP CONSTRAINT p_code_ui CASCADE");
    let n: i64 = conn
        .query_row("SELECT count(*) FROM sqlite_master WHERE name = 'p_code_ui'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
    must_exec(&conn, "INSERT INTO c VALUES (1, 99)"); // no foreign key, no mismatch
    must_exec(&conn, "INSERT INTO p VALUES (2, 10)"); // no key
    conn.exec("INSERT INTO c VALUES (9, 10)", &[])
        .expect_err("the other foreign key stays");
    let n: i64 = conn.query_row("SELECT count(*) FROM c", &[]).scan().unwrap();
    assert_eq!(n, 2);

    // Inside a transaction, a rollback brings all of it back.
    must_exec(
        &conn,
        "CREATE TABLE q (id int); CREATE UNIQUE INDEX q_ui ON q (id);
        CREATE TABLE r (qid int REFERENCES q (id)); INSERT INTO q VALUES (1);",
    );
    let tx = conn.begin().unwrap();
    tx.exec("ALTER TABLE q DROP CONSTRAINT q_ui CASCADE", &[]).unwrap();
    tx.exec("INSERT INTO r VALUES (9)", &[]).unwrap();
    tx.rollback().unwrap();
    conn.exec("INSERT INTO r VALUES (9)", &[])
        .expect_err("the foreign key is back");
}
