//! What Django's schema editor and introspection send while migrating (Wagtail's
//! bakerydemo was the case), each answered as Postgres 17 answers it.

use crate::common::*;

// A missing column of a table the statement names is 42703 in Postgres' words. The
// quoted qualifier hid the table from the check, which then took it for a missing
// FROM entry.
#[test]
fn missing_column_of_a_named_table() {
    let server = start_server();
    for conn in [connect(&server), connect_extended(&server)] {
        must_exec(&conn, "CREATE TABLE IF NOT EXISTS r9 (id int)");
        for (sql, message) in [
            (r#"SELECT "r9"."nope" FROM "r9""#, "column r9.nope does not exist"),
            ("SELECT r9.nope FROM r9", "column r9.nope does not exist"),
            (r#"SELECT x."nope" FROM "r9" x"#, "column x.nope does not exist"),
            (r#"SELECT "nope" FROM "r9""#, r#"column "nope" does not exist"#),
        ] {
            let err = conn.exec(sql, &[]).expect_err(sql);
            assert_sqlstate(&err, "42703");
            assert_eq!(pg_error(&err).message, message, "{sql}");
        }
        // A qualifier nothing in the statement is called stays a missing FROM entry.
        let err = conn.exec(r#"SELECT "zz"."id" FROM "r9""#, &[]).expect_err("zz");
        assert_pg_error(&err, "42P01", r#"missing FROM-clause entry for table "zz""#);
    }
}

// Postgres' byte-order collations are SQLite's default, BINARY, in a column
// definition and in ALTER COLUMN TYPE, where SQLite answered "no such collation
// sequence: C". A TYPE without COLLATE gives the column its type's collation back.
#[test]
fn byte_order_collations_in_column_definitions() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("r3_{i}");
        must_exec(
            &conn,
            &format!(
                r#"CREATE TABLE {t} ("path" varchar(255) NOT NULL UNIQUE, a text COLLATE "POSIX", b text COLLATE "default", c text COLLATE ucs_basic, d text COLLATE case_insensitive)"#
            ),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE {t} ALTER COLUMN path TYPE VARCHAR(255) COLLATE "C""#),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ALTER COLUMN "d" TYPE text COLLATE "C""#),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ADD COLUMN "e" varchar(10) COLLATE "C" NULL"#),
        );
        must_exec(
            &conn,
            &format!(
                "INSERT INTO {t} (path, a, b, c, d, e) VALUES ('a', 'a', 'a', 'a', 'a', 'a'), ('B', 'B', 'B', 'B', 'B', 'B')"
            ),
        );
        for col in ["path", "a", "b", "c", "d", "e"] {
            let got = query_column(&conn, &format!("SELECT {col} FROM {t} ORDER BY {col}"), 0);
            assert_eq!(got, ["B", "a"], "{col} sorts by bytes");
        }
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {t} WHERE d = 'b'"), &[])
            .scan()
            .unwrap();
        assert_eq!(n, 0, "the case-insensitive collation is gone");
        conn.exec(&format!("INSERT INTO {t} (path) VALUES ('a')"), &[])
            .expect_err("the unique key survives the new type");
    }
}

// A cast in a DEFAULT is the value it casts to, in CREATE TABLE, ADD COLUMN and SET
// DEFAULT: Django writes `SET DEFAULT '<hex>'::uuid` for a uuid field's default,
// which reached SQLite as written, and a CAST left by the rewrite needs the
// parentheses SQLite wants around an expression.
#[test]
fn casts_in_column_defaults() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("r6_{i}");
        must_exec(
            &conn,
            &format!(
                r#"CREATE TABLE {t} ("k" uuid NULL, n int DEFAULT '5'::integer, u uuid DEFAULT '08dc6092358649c8b4108813ba44b39e'::uuid, j jsonb DEFAULT '{{}}'::jsonb, b boolean DEFAULT 'true'::boolean, s text DEFAULT CAST(3 AS text))"#
            ),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ALTER COLUMN "k" SET DEFAULT '08dc6092358649c8b4108813ba44b39e'::uuid"#),
        );
        must_exec(
            &conn,
            &format!(
                r#"ALTER TABLE "{t}" ADD COLUMN "z" uuid DEFAULT '08dc6092358649c8b4108813ba44b39e'::uuid NOT NULL"#
            ),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ALTER COLUMN "n" SET DEFAULT 7::smallint"#),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ALTER COLUMN "j" SET DEFAULT '[]'::jsonb"#),
        );
        must_exec(&conn, &format!("INSERT INTO {t} DEFAULT VALUES"));
        let row = conn
            .query_row(
                &format!("SELECT k::text, n, u::text, j::text, b, s, z::text FROM {t}"),
                &[],
            )
            .scan::<(String, i64, String, String, bool, String, String)>()
            .unwrap();
        let uuid = "08dc6092-3586-49c8-b410-8813ba44b39e".to_string();
        assert_eq!(
            row,
            (
                uuid.clone(),
                7,
                uuid.clone(),
                "[]".to_string(),
                true,
                "3".to_string(),
                uuid
            )
        );
        // And DROP DEFAULT takes the whole of it away.
        must_exec(&conn, &format!(r#"ALTER TABLE "{t}" ALTER COLUMN "k" DROP DEFAULT"#));
        must_exec(&conn, &format!("INSERT INTO {t} (n) VALUES (1)"));
        let k: Option<String> = conn
            .query_row(&format!("SELECT k::text FROM {t} WHERE n = 1"), &[])
            .scan()
            .unwrap();
        assert_eq!(k, None);
    }
}

// An operator class names how an index compares; SQLite has its one btree, so the
// class is dropped for SQLite and kept for the catalog. Django makes a `_like`
// index with varchar_pattern_ops or text_pattern_ops beside every indexed text
// column, and SQLite answered a syntax error.
#[test]
fn index_operator_classes() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("r2_{i}");
        must_exec(
            &conn,
            &format!(r#"CREATE TABLE {t} ("name" varchar(150) NOT NULL UNIQUE, "t" text, "c" char(3), n int)"#),
        );
        must_exec(
            &conn,
            &format!(r#"CREATE INDEX "{t}_name_like" ON "{t}" ("name" varchar_pattern_ops)"#),
        );
        must_exec(
            &conn,
            &format!(r#"CREATE INDEX "{t}_t_like" ON "{t}" ("t" text_pattern_ops)"#),
        );
        must_exec(
            &conn,
            &format!(r#"CREATE INDEX "{t}_c_like" ON "{t}" USING btree ("c" bpchar_pattern_ops)"#),
        );
        must_exec(
            &conn,
            &format!(
                r#"CREATE UNIQUE INDEX "{t}_two" ON public."{t}" (t pg_catalog.text_pattern_ops DESC, n int4_ops)"#
            ),
        );
        let got = query_column(
            &conn,
            &format!("SELECT indexname FROM pg_indexes WHERE tablename = '{t}' AND indexname LIKE '%like' ORDER BY 1"),
            0,
        );
        assert_eq!(
            got,
            [format!("{t}_c_like"), format!("{t}_name_like"), format!("{t}_t_like")]
        );
        let def = scan_str(
            &conn,
            &format!(
                "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = '{t}_name_like'"
            ),
        );
        assert_eq!(
            def,
            format!("CREATE INDEX {t}_name_like ON public.{t} USING btree (name varchar_pattern_ops)")
        );
        let def = scan_str(
            &conn,
            &format!(
                "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = '{t}_two'"
            ),
        );
        assert_eq!(
            def,
            format!("CREATE UNIQUE INDEX {t}_two ON public.{t} USING btree (t text_pattern_ops DESC, n)")
        );
        // The indexes work: the unique one refuses a duplicate.
        must_exec(&conn, &format!("INSERT INTO {t} (name, t, n) VALUES ('a', 'x', 1)"));
        conn.exec(&format!("INSERT INTO {t} (name, t, n) VALUES ('b', 'x', 1)"), &[])
            .expect_err("unique on (t, n)");
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {t} WHERE name LIKE 'a%'"), &[])
            .scan()
            .unwrap();
        assert_eq!(n, 1);
    }
}

// TRUNCATE in each of its forms: Django's flush truncates every table at once, and
// its test runner with RESTART IDENTITY. SQLite has no TRUNCATE.
#[test]
fn truncate_forms() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let (a, b, c) = (format!("r21a_{i}"), format!("r21b_{i}"), format!("r21c_{i}"));
        must_exec(&conn, &format!("CREATE TABLE {a} (id serial PRIMARY KEY, v text)"));
        must_exec(&conn, &format!("CREATE TABLE {b} (id serial PRIMARY KEY, v text)"));
        let fill = |conn: &Conn| {
            must_exec(conn, &format!("INSERT INTO {a} (v) VALUES ('x'), ('y')"));
            must_exec(conn, &format!("INSERT INTO {b} (v) VALUES ('z')"));
        };
        let count = |conn: &Conn, t: &str| -> i64 {
            conn.query_row(&format!("SELECT count(*) FROM {t}"), &[])
                .scan()
                .unwrap()
        };
        fill(&conn);
        let tag = conn.exec(&format!(r#"TRUNCATE "{a}""#), &[]).unwrap();
        assert_eq!(tag.as_str(), "TRUNCATE TABLE");
        assert_eq!(count(&conn, &a), 0);
        fill(&conn);
        must_exec(
            &conn,
            &format!(r#"TRUNCATE TABLE ONLY "{a}", public."{b}" CONTINUE IDENTITY RESTRICT"#),
        );
        assert_eq!((count(&conn, &a), count(&conn, &b)), (0, 0));
        let id: i64 = conn
            .query_row(&format!("INSERT INTO {a} (v) VALUES ('n') RETURNING id"), &[])
            .scan()
            .unwrap();
        assert_eq!(id, 5, "CONTINUE IDENTITY keeps the sequence");
        must_exec(&conn, &format!(r#"TRUNCATE "{a}", "{b}" RESTART IDENTITY;"#));
        let id: i64 = conn
            .query_row(&format!("INSERT INTO {a} (v) VALUES ('n') RETURNING id"), &[])
            .scan()
            .unwrap();
        assert_eq!(id, 1, "RESTART IDENTITY restarts it");

        // A table another one's foreign key points at goes only with it, or with CASCADE.
        must_exec(
            &conn,
            &format!("CREATE TABLE {c} (id serial PRIMARY KEY, a_id int REFERENCES {a}(id))"),
        );
        must_exec(&conn, &format!("INSERT INTO {c} (a_id) VALUES (1)"));
        let err = conn.exec(&format!("TRUNCATE {a}"), &[]).expect_err("referenced");
        assert_pg_error(
            &err,
            "0A000",
            "cannot truncate a table referenced in a foreign key constraint",
        );
        must_exec(&conn, &format!("TRUNCATE {a}, {c}"));
        must_exec(&conn, &format!("INSERT INTO {a} (v) VALUES ('n')"));
        must_exec(&conn, &format!("INSERT INTO {c} (a_id) SELECT max(id) FROM {a}"));
        must_exec(&conn, &format!("TRUNCATE {a} CASCADE"));
        assert_eq!((count(&conn, &a), count(&conn, &c)), (0, 0));

        // Inside a transaction it is undone with it.
        fill(&conn);
        must_exec(&conn, "BEGIN");
        must_exec(&conn, &format!("TRUNCATE {a}, {b}, {c} RESTART IDENTITY"));
        assert_eq!(count(&conn, &a), 0);
        must_exec(&conn, "ROLLBACK");
        assert_eq!((count(&conn, &a), count(&conn, &b)), (2, 1));

        let err = conn.exec("TRUNCATE nothing_here", &[]).expect_err("missing");
        assert_pg_error(&err, "42P01", r#"relation "nothing_here" does not exist"#);
    }
}

// A foreign key keeps the name it is given, written on its column (Django's ADD
// COLUMN ... CONSTRAINT "name" REFERENCES) or apart, and its DEFERRABLE INITIALLY
// DEFERRED: pg_constraint showed `<table>_<col>_fkey` for one on its column, so
// Django could not find the key it had made, and two keys on one column shared
// one name.
#[test]
fn named_foreign_keys() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let (p, t) = (format!("r14p_{i}"), format!("r14_{i}"));
        must_exec(
            &conn,
            &format!(r#"CREATE TABLE {p} ("id" serial PRIMARY KEY, code int UNIQUE)"#),
        );
        must_exec(
            &conn,
            &format!(
                r#"CREATE TABLE {t} ("id" serial PRIMARY KEY, "x" integer CONSTRAINT "{t}_x_named" REFERENCES {p}(id) DEFERRABLE INITIALLY DEFERRED, y int REFERENCES {p}(code), CONSTRAINT "{t}_tbl_fk" FOREIGN KEY (y) REFERENCES {p}(code) DEFERRABLE INITIALLY DEFERRED)"#
            ),
        );
        must_exec(
            &conn,
            &format!(
                r#"ALTER TABLE "{t}" ADD COLUMN "p_id" integer NULL CONSTRAINT "{t}_p_id_3c7e30a6_fk_r14p_id" REFERENCES "{p}"("id") DEFERRABLE INITIALLY DEFERRED"#
            ),
        );
        must_exec(&conn, &format!(r#"ALTER TABLE "{t}" ADD COLUMN "q_id" integer NULL"#));
        must_exec(
            &conn,
            &format!(
                r#"ALTER TABLE "{t}" ADD CONSTRAINT "{t}_q_id_fk" FOREIGN KEY ("q_id") REFERENCES "{p}" ("id") DEFERRABLE INITIALLY DEFERRED"#
            ),
        );
        let rows = conn
            .query(
                &format!(
                    "SELECT conname, condeferrable, condeferred, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = '{t}'::regclass ORDER BY 1"
                ),
                &[],
            )
            .unwrap()
            .collect_rows::<(String, bool, bool, String)>()
            .unwrap();
        let row = |name: &str, d: bool, def: &str| (format!("{t}_{name}"), d, d, def.replace("{p}", &p));
        assert_eq!(
            rows,
            [
                row(
                    "p_id_3c7e30a6_fk_r14p_id",
                    true,
                    "FOREIGN KEY (p_id) REFERENCES {p}(id) DEFERRABLE INITIALLY DEFERRED"
                ),
                row("pkey", false, "PRIMARY KEY (id)"),
                row(
                    "q_id_fk",
                    true,
                    "FOREIGN KEY (q_id) REFERENCES {p}(id) DEFERRABLE INITIALLY DEFERRED"
                ),
                row(
                    "tbl_fk",
                    true,
                    "FOREIGN KEY (y) REFERENCES {p}(code) DEFERRABLE INITIALLY DEFERRED"
                ),
                row(
                    "x_named",
                    true,
                    "FOREIGN KEY (x) REFERENCES {p}(id) DEFERRABLE INITIALLY DEFERRED"
                ),
                row("y_fkey", false, "FOREIGN KEY (y) REFERENCES {p}(code)"),
            ]
        );
        // Deferred: a row pointing nowhere waits for the commit.
        must_exec(&conn, "BEGIN");
        must_exec(&conn, &format!("INSERT INTO {t} (p_id) VALUES (99)"));
        must_exec(&conn, &format!("INSERT INTO {p} (id) VALUES (99)"));
        must_exec(&conn, "COMMIT");

        for name in ["p_id_3c7e30a6_fk_r14p_id", "x_named", "q_id_fk"] {
            must_exec(&conn, &format!(r#"ALTER TABLE "{t}" DROP CONSTRAINT "{t}_{name}""#));
        }
        let names = query_column(
            &conn,
            &format!("SELECT conname FROM pg_constraint WHERE conrelid = '{t}'::regclass ORDER BY 1"),
            0,
        );
        assert_eq!(
            names,
            [format!("{t}_pkey"), format!("{t}_tbl_fk"), format!("{t}_y_fkey")]
        );
        let err = conn
            .exec(&format!(r#"ALTER TABLE "{t}" DROP CONSTRAINT "{t}_x_named""#), &[])
            .expect_err("gone");
        assert_sqlstate(&err, "42704");
        must_exec(&conn, &format!("INSERT INTO {t} (p_id, x) VALUES (12345, 12345)"));
    }
}

// DROP COLUMN takes the indexes and table constraints on the column with it, as
// Postgres does, a multi-column one whole. SQLite refuses to drop an indexed column
// or one in a UNIQUE ("error in index ... after drop column").
#[test]
fn drop_column_with_its_indexes_and_constraints() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let (p, t) = (format!("r17p_{i}"), format!("r17_{i}"));
        must_exec(
            &conn,
            &format!("CREATE TABLE {p} (id serial PRIMARY KEY, k int UNIQUE)"),
        );
        must_exec(
            &conn,
            &format!(
                r#"CREATE TABLE {t} ("id" serial PRIMARY KEY, "flag" boolean NOT NULL, "q" integer NULL, "d" date NULL, "u" int UNIQUE, "f" int REFERENCES {p}(id), "g" int, "c" int, CONSTRAINT "{t}_g_fk" FOREIGN KEY ("g") REFERENCES {p}(k), CONSTRAINT "{t}_c_check" CHECK ("c" > 0 AND "c" < 100))"#
            ),
        );
        must_exec(&conn, &format!(r#"CREATE INDEX "{t}_flag_idx" ON "{t}" ("flag")"#));
        must_exec(
            &conn,
            &format!(r#"CREATE INDEX "{t}_d_q_idx" ON "{t}" ("d", "q" DESC)"#),
        );
        must_exec(
            &conn,
            &format!(r#"CREATE INDEX "{t}_lower" ON "{t}" ((c + 1)) WHERE g > 0"#),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ADD CONSTRAINT "{t}_q_d_uniq" UNIQUE ("q", "d")"#),
        );
        must_exec(&conn, &format!("INSERT INTO {p} (k) VALUES (7)"));
        must_exec(
            &conn,
            &format!("INSERT INTO {t} (flag, q, d, u, f, g, c) VALUES (true, 1, '2024-01-02', 5, 1, 7, 3)"),
        );
        // A key another table's foreign key stands on goes only with CASCADE.
        let err = conn
            .exec(&format!("ALTER TABLE {p} DROP COLUMN k"), &[])
            .expect_err("the key of a foreign key");
        assert_sqlstate(&err, "2BP01");
        for col in ["flag", "q", "u", "f", "g", "c"] {
            must_exec(&conn, &format!(r#"ALTER TABLE "{t}" DROP COLUMN "{col}""#));
        }
        let cols = query_column(
            &conn,
            &format!(
                "SELECT column_name FROM information_schema.columns WHERE table_name = '{t}' ORDER BY ordinal_position"
            ),
            0,
        );
        assert_eq!(cols, ["id", "d"]);
        let indexes = query_column(
            &conn,
            &format!(
                "SELECT c.relname FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = '{t}'::regclass ORDER BY 1"
            ),
            0,
        );
        assert_eq!(indexes, [format!("{t}_pkey")]);
        let cons = query_column(
            &conn,
            &format!("SELECT conname FROM pg_constraint WHERE conrelid = '{t}'::regclass ORDER BY 1"),
            0,
        );
        assert_eq!(cons, [format!("{t}_pkey")]);
        let d = scan_str(&conn, &format!("SELECT d::text FROM {t}"));
        assert_eq!(d, "2024-01-02");

        let err = conn
            .exec(&format!("ALTER TABLE {t} DROP COLUMN nope"), &[])
            .expect_err("no such column");
        assert_pg_error(
            &err,
            "42703",
            &format!(r#"column "nope" of relation "{t}" does not exist"#),
        );
    }
}

// A serial column's sequence answers pg_get_serial_sequence, setval, nextval and
// currval, and Django's sequence reset after a load with explicit ids gives the next
// id past them. pg_get_serial_sequence was no function here.
#[test]
fn serial_sequences() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let (t, e, n) = (format!("r8_{i}"), format!("r8e_{i}"), format!("r8n_{i}"));
        must_exec(
            &conn,
            &format!(r#"CREATE TABLE {t} ("id" serial PRIMARY KEY, "a" text)"#),
        );
        must_exec(&conn, &format!("CREATE TABLE {e} (id bigserial PRIMARY KEY)"));
        must_exec(&conn, &format!("CREATE SEQUENCE {n}_id_seq"));
        must_exec(
            &conn,
            &format!("CREATE TABLE {n} (id int DEFAULT nextval('{n}_id_seq'), b int)"),
        );
        let seq = |sql: String| -> Option<String> { conn.query_row(&sql, &[]).scan().unwrap() };
        assert_eq!(
            seq(format!(r#"SELECT pg_get_serial_sequence('"{t}"','id')"#)),
            Some(format!("public.{t}_id_seq"))
        );
        assert_eq!(
            seq(format!("SELECT pg_get_serial_sequence('public.{t}', 'id')")),
            Some(format!("public.{t}_id_seq"))
        );
        // A default on a sequence of its own is no serial: the sequence is not the column's.
        assert_eq!(seq(format!("SELECT pg_get_serial_sequence('{n}', 'id')")), None);
        assert_eq!(seq(format!("SELECT pg_get_serial_sequence('{t}', 'a')")), None);
        let err = conn
            .exec(&format!("SELECT pg_get_serial_sequence('{t}', 'nope')"), &[])
            .expect_err("no column");
        assert_pg_error(
            &err,
            "42703",
            &format!(r#"column "nope" of relation "{t}" does not exist"#),
        );

        // Explicit ids, then the reset Django runs after loaddata.
        must_exec(&conn, &format!("INSERT INTO {t} (id, a) VALUES (5, 'x')"));
        let reset = |table: &str| -> i64 {
            conn.query_row(
                &format!(
                    r#"SELECT setval(pg_get_serial_sequence('"{table}"','id'), coalesce(max("id"), 1), max("id") IS NOT null) FROM "{table}""#
                ),
                &[],
            )
            .scan()
            .unwrap()
        };
        assert_eq!(reset(&t), 5);
        let id: i64 = conn
            .query_row(&format!("INSERT INTO {t} (a) VALUES ('y') RETURNING id"), &[])
            .scan()
            .unwrap();
        assert_eq!(id, 6);
        // An empty table: the next id is 1.
        assert_eq!(reset(&e), 1);
        let id: i64 = conn
            .query_row(&format!("INSERT INTO {e} DEFAULT VALUES RETURNING id"), &[])
            .scan()
            .unwrap();
        assert_eq!(id, 1);

        // nextval and currval on a serial's sequence, and setval by its name.
        assert_eq!(seq_val(&conn, &format!("SELECT nextval('{t}_id_seq')")), 7);
        assert_eq!(seq_val(&conn, &format!("SELECT currval('public.{t}_id_seq')")), 7);
        assert_eq!(seq_val(&conn, &format!("SELECT setval('{t}_id_seq', 20)")), 20);
        let id: i64 = conn
            .query_row(&format!("INSERT INTO {t} (a) VALUES ('z') RETURNING id"), &[])
            .scan()
            .unwrap();
        assert_eq!(id, 21);
        assert_eq!(seq_val(&conn, &format!("SELECT setval('{t}_id_seq', 30, false)")), 30);
        assert_eq!(seq_val(&conn, &format!("SELECT nextval('{t}_id_seq')")), 30);

        // So the reset leaves it be: setval of NULL is NULL.
        must_exec(&conn, &format!("INSERT INTO {n} (id, b) VALUES (9, 1)"));
        let none: Option<i64> = conn
            .query_row(
                &format!(r#"SELECT setval(pg_get_serial_sequence('"{n}"','id'), coalesce(max("id"), 1), max("id") IS NOT null) FROM "{n}""#),
                &[],
            )
            .scan()
            .unwrap();
        assert_eq!(none, None);
        must_exec(&conn, &format!("INSERT INTO {n} (b) VALUES (2)"));
        assert_eq!(seq_val(&conn, &format!("SELECT id FROM {n} WHERE b = 2")), 1);
    }
}

// Identity columns, which Django 4.1 and later make every AutoField: GENERATED BY
// DEFAULT AS IDENTITY, which SQLite answered with a syntax error. A key one is SQLite's
// AUTOINCREMENT, any other a sequence of its own; both are in the catalog as Postgres
// has them, and ALWAYS refuses a value without OVERRIDING SYSTEM VALUE.
#[test]
fn identity_columns() {
    let server = start_server();
    for (n, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let [t1, t2, t3, t4] = ["t1", "t2", "t3", "t4"].map(|t| format!("id{n}_{t}"));
        let ids = |sql: &str| -> Vec<i64> {
            conn.query(sql, &[])
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
                .collect_rows::<(i64,)>()
                .unwrap()
                .into_iter()
                .map(|r| r.0)
                .collect()
        };
        must_exec(
            &conn,
            &format!(
                r#"CREATE TABLE {t1} ("id" integer NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY, "a" text)"#
            ),
        );
        assert_eq!(ids(&format!("INSERT INTO {t1} (a) VALUES ('x') RETURNING id")), [1]);
        must_exec(
            &conn,
            &format!(r#"CREATE TABLE {t2} ("id" bigint NOT NULL PRIMARY KEY GENERATED ALWAYS AS IDENTITY, a text)"#),
        );
        assert_eq!(ids(&format!("INSERT INTO {t2} (a) VALUES ('x') RETURNING id")), [1]);
        for sql in [
            format!("INSERT INTO {t2} (id, a) VALUES (10, 'y')"),
            format!("INSERT INTO {t2} VALUES (10, 'y')"),
        ] {
            let err = conn.exec(&sql, &[]).expect_err(&sql);
            assert_pg_error(&err, "428C9", r#"cannot insert a non-DEFAULT value into column "id""#);
        }
        must_exec(
            &conn,
            &format!("INSERT INTO {t2} (id, a) OVERRIDING SYSTEM VALUE VALUES (10, 'y')"),
        );
        must_exec(
            &conn,
            &format!(
                "CREATE TABLE {t3} (id int GENERATED BY DEFAULT AS IDENTITY (START WITH 100 INCREMENT BY 5), b text)"
            ),
        );
        assert_eq!(
            ids(&format!("INSERT INTO {t3} (b) VALUES ('a'), ('b') RETURNING id")),
            [100, 105]
        );
        assert_eq!(ids(&format!("INSERT INTO {t3} DEFAULT VALUES RETURNING id")), [110]);

        let rows = conn
            .query(
                &format!(
                    "SELECT c.relname, attname, attidentity, attnotnull, format_type(atttypid, atttypmod) FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid WHERE c.relname IN ('{t1}', '{t2}', '{t3}') AND attnum > 0 ORDER BY 1, attnum"
                ),
                &[],
            )
            .unwrap()
            .collect_rows::<(String, String, String, bool, String)>()
            .unwrap();
        let row = |t: &str, c: &str, i: &str, nn: bool, ty: &str| {
            (t.to_string(), c.to_string(), i.to_string(), nn, ty.to_string())
        };
        assert_eq!(
            rows,
            [
                row(&t1, "id", "d", true, "integer"),
                row(&t1, "a", "", false, "text"),
                row(&t2, "id", "a", true, "bigint"),
                row(&t2, "a", "", false, "text"),
                row(&t3, "id", "d", true, "integer"),
                row(&t3, "b", "", false, "text"),
            ]
        );
        let rows = conn
            .query(
                &format!(
                    "SELECT table_name, column_name, is_identity, coalesce(identity_generation, ''), data_type FROM information_schema.columns WHERE table_name IN ('{t1}', '{t2}', '{t3}') AND column_name = 'id' ORDER BY 1"
                ),
                &[],
            )
            .unwrap()
            .collect_rows::<(String, String, String, String, String)>()
            .unwrap();
        let row = |t: &str, g: &str, ty: &str| {
            (
                t.to_string(),
                "id".to_string(),
                "YES".to_string(),
                g.to_string(),
                ty.to_string(),
            )
        };
        assert_eq!(
            rows,
            [
                row(&t1, "BY DEFAULT", "integer"),
                row(&t2, "ALWAYS", "bigint"),
                row(&t3, "BY DEFAULT", "integer")
            ]
        );
        for t in [&t1, &t2, &t3] {
            let seq: Option<String> = conn
                .query_row(&format!("SELECT pg_get_serial_sequence('{t}', 'id')"), &[])
                .scan()
                .unwrap();
            assert_eq!(seq, Some(format!("public.{t}_id_seq")));
        }

        must_exec(
            &conn,
            &format!("ALTER TABLE {t2} ALTER COLUMN id SET GENERATED BY DEFAULT"),
        );
        must_exec(&conn, &format!("INSERT INTO {t2} (id, a) VALUES (50, 'q')"));
        let err = conn
            .exec(&format!("ALTER TABLE {t1} ALTER COLUMN a DROP IDENTITY"), &[])
            .expect_err("not identity");
        assert_pg_error(
            &err,
            "55000",
            &format!(r#"column "a" of relation "{t1}" is not an identity column"#),
        );
        must_exec(
            &conn,
            &format!("ALTER TABLE {t1} ALTER COLUMN a DROP IDENTITY IF EXISTS"),
        );
        must_exec(&conn, &format!("ALTER TABLE {t1} ALTER COLUMN id DROP IDENTITY"));
        let err = conn
            .exec(&format!("INSERT INTO {t1} (a) VALUES ('n')"), &[])
            .expect_err("no default any more");
        assert_sqlstate(&err, "23502");
        assert_eq!(ids(&format!("SELECT id FROM {t1}")), [1]);

        // Django's AlterField to an AutoField, then to a BigAutoField.
        must_exec(
            &conn,
            &format!("CREATE TABLE {t4} (id integer NOT NULL PRIMARY KEY, a text)"),
        );
        must_exec(&conn, &format!("INSERT INTO {t4} (id) VALUES (7)"));
        must_exec(
            &conn,
            &format!("ALTER TABLE {t4} ALTER COLUMN id ADD GENERATED BY DEFAULT AS IDENTITY"),
        );
        must_exec(&conn, &format!("ALTER TABLE {t4} ALTER COLUMN id TYPE bigint"));
        let rows = conn
            .query(
                &format!(
                    "SELECT attname, attidentity, format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = '{t4}'::regclass AND attnum > 0 ORDER BY attnum"
                ),
                &[],
            )
            .unwrap()
            .collect_rows::<(String, String, String)>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("id".to_string(), "d".to_string(), "bigint".to_string()),
                ("a".to_string(), String::new(), "text".to_string())
            ]
        );
        must_exec(&conn, &format!("INSERT INTO {t4} (id, a) VALUES (8, 'x')"));
        must_exec(&conn, &format!("INSERT INTO {t4} (a) VALUES ('y')"));
        assert_eq!(ids(&format!("SELECT count(*) FROM {t4} WHERE id IS NOT NULL")), [3]);

        must_exec(&conn, &format!("ALTER TABLE {t3} ALTER COLUMN id DROP IDENTITY"));
        let err = conn
            .exec(&format!("INSERT INTO {t3} (b) VALUES ('c')"), &[])
            .expect_err("no default any more");
        assert_sqlstate(&err, "23502");
    }
}

/// Each row of a result as text, NULL spelled so, sorted.
fn text_rows(rows: &Rows) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|row| {
            (0..rows.fields.len())
                .map(|i| {
                    let cell = row.cell(i);
                    if cell.is_null() {
                        "NULL".to_string()
                    } else {
                        cell.text()
                    }
                })
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect();
    out.sort();
    out
}

// The queries of Django's introspection (get_constraints, get_relations,
// get_sequences), as Django 6 sends them, answered as Postgres 17 answers them:
// conkey and confkey are int2[], `c.confkey[1]` subscripts a qualified array, a
// multi-argument unnest WITH ORDINALITY names its columns, ARRAY(subquery) and
// array_agg are arrays, pg_depend ties a serial or identity column to its sequence,
// and a UNIQUE constraint keeps its name.
#[test]
fn django_introspection_queries() {
    let server = start_server();
    for (n, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let [p, c, s] = ["p", "c", "s"].map(|t| format!("in{n}_{t}"));
        for sql in [
            format!(
                r#"CREATE TABLE {p} ("id" bigint NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY, "code" varchar(10) NOT NULL UNIQUE)"#
            ),
            format!(
                r#"CREATE TABLE {c} ("id" integer NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY, "name" varchar(100) NOT NULL, "slug" varchar(50) NOT NULL, "n" integer NULL, "d" date NULL, "p_id" bigint NULL)"#
            ),
            format!(
                r#"ALTER TABLE "{c}" ADD CONSTRAINT "{c}_p_id_fk" FOREIGN KEY ("p_id") REFERENCES "{p}" ("id") DEFERRABLE INITIALLY DEFERRED"#
            ),
            format!(r#"CREATE INDEX "{c}_p_id_idx" ON "{c}" ("p_id")"#),
            format!(r#"CREATE INDEX "{c}_slug_like" ON "{c}" ("slug" varchar_pattern_ops)"#),
            format!(r#"ALTER TABLE "{c}" ADD CONSTRAINT "{c}_n_d_uniq" UNIQUE ("n", "d")"#),
            format!(r#"CREATE INDEX "{c}_name_n_idx" ON "{c}" ("name", "n" DESC)"#),
            format!(
                r#"CREATE TABLE {s} ("id" serial PRIMARY KEY, "c_id" integer NULL CONSTRAINT "{s}_c_id_fk" REFERENCES "{c}"("id") DEFERRABLE INITIALLY DEFERRED)"#
            ),
        ] {
            must_exec(&conn, &sql);
        }
        let constraints = format!(
            r#"
            SELECT
                c.conname,
                array(
                    SELECT attname
                    FROM unnest(c.conkey) WITH ORDINALITY cols(colid, arridx)
                    JOIN pg_attribute AS ca ON cols.colid = ca.attnum
                    WHERE ca.attrelid = c.conrelid
                    ORDER BY cols.arridx
                ),
                c.contype,
                (SELECT fkc.relname || '.' || fka.attname
                FROM pg_attribute AS fka
                JOIN pg_class AS fkc ON fka.attrelid = fkc.oid
                WHERE fka.attrelid = c.confrelid AND fka.attnum = c.confkey[1]),
                cl.reloptions
            FROM pg_constraint AS c
            JOIN pg_class AS cl ON c.conrelid = cl.oid
            WHERE cl.relname = '{c}'
                AND pg_catalog.pg_table_is_visible(cl.oid)
                AND c.contype != 'n'
        "#
        );
        let rows = conn.query_formats(&constraints, &[0], &[]).unwrap();
        assert_eq!(rows.fields[1].data_type_oid, 1009, "ARRAY(SELECT attname) is an array");
        let want = [
            format!("{c}_n_d_uniq | {{n,d}} | u | NULL | NULL"),
            format!("{c}_p_id_fk | {{p_id}} | f | {p}.id | NULL"),
            format!("{c}_pkey | {{id}} | p | NULL | NULL"),
        ];
        assert_eq!(text_rows(&rows), want);

        let indexes = format!(
            r#"
            SELECT
                indexname,
                array_agg(attname ORDER BY arridx),
                indisunique,
                indisprimary,
                array_agg(ordering ORDER BY arridx),
                amname,
                exprdef,
                s2.attoptions
            FROM (
                SELECT
                    c2.relname as indexname, idx.*, attr.attname, am.amname,
                    CASE
                        WHEN idx.indexprs IS NOT NULL THEN
                            pg_get_indexdef(idx.indexrelid)
                    END AS exprdef,
                    CASE am.amname
                        WHEN 'btree' THEN
                            CASE (option & 1)
                                WHEN 1 THEN 'DESC' ELSE 'ASC'
                            END
                    END as ordering,
                    c2.reloptions as attoptions
                FROM (
                    SELECT *
                    FROM
                        pg_index i,
                        unnest(i.indkey, i.indoption)
                            WITH ORDINALITY koi(key, option, arridx)
                ) idx
                LEFT JOIN pg_class c ON idx.indrelid = c.oid
                LEFT JOIN pg_class c2 ON idx.indexrelid = c2.oid
                LEFT JOIN pg_am am ON c2.relam = am.oid
                LEFT JOIN
                    pg_attribute attr ON attr.attrelid = c.oid AND attr.attnum = idx.key
                WHERE c.relname = '{c}' AND pg_catalog.pg_table_is_visible(c.oid)
            ) s2
            GROUP BY indexname, indisunique, indisprimary, amname, exprdef, attoptions;
        "#
        );
        let rows = conn.query_formats(&indexes, &[0], &[]).unwrap();
        assert_eq!(rows.fields[1].data_type_oid, 1009, "array_agg(attname) is an array");
        let want = [
            format!("{c}_n_d_uniq | {{n,d}} | t | f | {{ASC,ASC}} | btree | NULL | NULL"),
            format!("{c}_name_n_idx | {{name,n}} | f | f | {{ASC,DESC}} | btree | NULL | NULL"),
            format!("{c}_p_id_idx | {{p_id}} | f | f | {{ASC}} | btree | NULL | NULL"),
            format!("{c}_pkey | {{id}} | t | t | {{ASC}} | btree | NULL | NULL"),
            format!("{c}_slug_like | {{slug}} | f | f | {{ASC}} | btree | NULL | NULL"),
        ];
        assert_eq!(text_rows(&rows), want);

        let relations = format!(
            r#"
            SELECT a1.attname, c2.relname, a2.attname, con.confdeltype
            FROM pg_constraint con
            LEFT JOIN pg_class c1 ON con.conrelid = c1.oid
            LEFT JOIN pg_class c2 ON con.confrelid = c2.oid
            LEFT JOIN
                pg_attribute a1 ON c1.oid = a1.attrelid AND a1.attnum = con.conkey[1]
            LEFT JOIN
                pg_attribute a2 ON c2.oid = a2.attrelid AND a2.attnum = con.confkey[1]
            WHERE
                c1.relname = '{s}' AND
                con.contype = 'f' AND
                c1.relnamespace = c2.relnamespace AND
                pg_catalog.pg_table_is_visible(c1.oid)
        "#
        );
        let rows = conn.query_formats(&relations, &[0], &[]).unwrap();
        assert_eq!(text_rows(&rows), [format!("c_id | {c} | id | a")]);

        for t in [&p, &c, &s] {
            let sequences = format!(
                r#"
            SELECT
                s.relname AS sequence_name,
                a.attname AS colname
            FROM
                pg_class s
                JOIN pg_depend d ON d.objid = s.oid
                    AND d.classid = 'pg_class'::regclass
                    AND d.refclassid = 'pg_class'::regclass
                JOIN pg_attribute a ON d.refobjid = a.attrelid
                    AND d.refobjsubid = a.attnum
                JOIN pg_class tbl ON tbl.oid = d.refobjid
                    AND tbl.relname = '{t}'
                    AND pg_catalog.pg_table_is_visible(tbl.oid)
            WHERE
                s.relkind = 'S';
        "#
            );
            let rows = conn.query_formats(&sequences, &[0], &[]).unwrap();
            assert_eq!(text_rows(&rows), [format!("{t}_id_seq | id")]);
        }

        // pg_indexes lists the key's index too, and writes each as Postgres does.
        let rows = conn
            .query(
                &format!("SELECT indexname, indexdef FROM pg_indexes WHERE tablename = '{c}'"),
                &[],
            )
            .unwrap();
        let want = [
            format!("{c}_n_d_uniq | CREATE UNIQUE INDEX {c}_n_d_uniq ON public.{c} USING btree (n, d)"),
            format!("{c}_name_n_idx | CREATE INDEX {c}_name_n_idx ON public.{c} USING btree (name, n DESC)"),
            format!("{c}_p_id_idx | CREATE INDEX {c}_p_id_idx ON public.{c} USING btree (p_id)"),
            format!("{c}_pkey | CREATE UNIQUE INDEX {c}_pkey ON public.{c} USING btree (id)"),
            format!("{c}_slug_like | CREATE INDEX {c}_slug_like ON public.{c} USING btree (slug varchar_pattern_ops)"),
        ];
        assert_eq!(text_rows(&rows), want);

        // What the cursor says of a column: varchar(100) is 100 long.
        let rows = conn.query(&format!(r#"SELECT * FROM "{c}" LIMIT 1"#), &[]).unwrap();
        let name = &rows.fields[1];
        assert_eq!((name.data_type_oid, name.type_modifier), (1043, 104));
        assert_eq!(rows.fields[4].data_type_size, 4, "a date is 4 bytes");
    }
}

// A column with a computed default (Django's db_default=Now()) is added by a rebuild,
// which put it after the table's constraints, where SQLite takes no column.
#[test]
fn computed_default_column_after_table_constraints() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("cd_{i}");
        must_exec(
            &conn,
            &format!("CREATE TABLE {t} (id serial PRIMARY KEY, a int, b int)"),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ADD CONSTRAINT "{t}_a_b_uniq" UNIQUE ("a", "b")"#),
        );
        must_exec(&conn, &format!("INSERT INTO {t} (a, b) VALUES (1, 2)"));
        must_exec(
            &conn,
            &format!(
                r#"ALTER TABLE "{t}" ADD COLUMN "seen" timestamp with time zone DEFAULT (STATEMENT_TIMESTAMP()) NOT NULL"#
            ),
        );
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {t} WHERE seen IS NOT NULL"), &[])
            .scan()
            .unwrap();
        assert_eq!(n, 1);
        conn.exec(&format!("INSERT INTO {t} (a, b) VALUES (1, 2)"), &[])
            .expect_err("the constraint is still there");
    }
}

// ADD COLUMN numeric(p,s) is the exact decimal CREATE TABLE makes of it: it was the
// plain type, so a value kept SQLite's reading and the catalog no precision.
#[test]
fn numeric_column_added_by_alter() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("na_{i}");
        must_exec(&conn, &format!("CREATE TABLE {t} (id int)"));
        must_exec(&conn, &format!("INSERT INTO {t} VALUES (1)"));
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ADD COLUMN "rating" numeric(3, 1) DEFAULT 0 NOT NULL"#),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ALTER COLUMN "rating" DROP DEFAULT"#),
        );
        must_exec(&conn, &format!("INSERT INTO {t} VALUES (2, 2.25)"));
        let got = query_column(&conn, &format!("SELECT rating::text FROM {t} WHERE id = 2"), 0);
        assert_eq!(got, ["2.3"]);
        let typ = scan_str(
            &conn,
            &format!(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = '{t}'::regclass AND attname = 'rating'"
            ),
        );
        assert_eq!(typ, "numeric(3,1)");
        let rows = conn.query(&format!("SELECT rating FROM {t}"), &[]).unwrap();
        assert_eq!(rows.fields[0].data_type_oid, 1700);
    }
}

// A CHECK is in pg_constraint, under the name it was given or the one Postgres gives
// it, with the columns it reads: Django finds a PositiveIntegerField's check there to
// drop it when the field changes, and kept it when pg_constraint had none.
#[test]
fn check_constraints_in_the_catalog() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("ck_{i}");
        must_exec(&conn, &format!("CREATE TYPE ck_mood_{i} AS ENUM ('sad', 'ok')"));
        must_exec(
            &conn,
            &format!(
                r#"CREATE TABLE "{t}" ("id" bigint NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY, "pos" integer NOT NULL CHECK ("pos" >= 0), "a" int, "b" int, "m" ck_mood_{i}, CONSTRAINT "{t}_a_b" CHECK ("a" > "b"))"#
            ),
        );
        let rows = conn
            .query_formats(
                &format!(
                    "SELECT conname, contype, conkey, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = '{t}'::regclass ORDER BY 1"
                ),
                &[0],
                &[],
            )
            .unwrap();
        assert_eq!(
            text_rows(&rows),
            [
                format!(r#"{t}_a_b | c | {{3,4}} | CHECK ("a" > "b")"#),
                format!("{t}_pkey | p | {{1}} | PRIMARY KEY (id)"),
                format!(r#"{t}_pos_check | c | {{2}} | CHECK ("pos" >= 0)"#),
            ]
        );
        // What Django does when a PositiveIntegerField becomes an IntegerField.
        must_exec(&conn, &format!(r#"ALTER TABLE "{t}" DROP CONSTRAINT "{t}_pos_check""#));
        must_exec(&conn, &format!(r#"INSERT INTO "{t}" (pos) VALUES (-1)"#));
        must_exec(&conn, &format!(r#"DELETE FROM "{t}""#));
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ADD CONSTRAINT "{t}_pos_3ec685ce_check" CHECK ("pos" >= 0)"#),
        );
        let names = query_column(
            &conn,
            &format!("SELECT conname FROM pg_constraint WHERE conrelid = '{t}'::regclass AND contype = 'c' ORDER BY 1"),
            0,
        );
        assert_eq!(names, [format!("{t}_a_b"), format!("{t}_pos_3ec685ce_check")]);
    }
}

// ALTER COLUMN TYPE makes the rows fit the new type, as Postgres rewrites them:
// numeric(12,3) holds 9.500, and varchar(3) refuses a longer value and leaves the
// table as it was.
#[test]
fn alter_type_fits_the_rows() {
    let server = start_server();
    for (i, conn) in [connect(&server), connect_extended(&server)].into_iter().enumerate() {
        let t = format!("at_{i}");
        must_exec(
            &conn,
            &format!("CREATE TABLE {t} (id serial PRIMARY KEY, price numeric(10,2), name varchar(10))"),
        );
        must_exec(
            &conn,
            &format!("INSERT INTO {t} (price, name) VALUES (9.5, 'abcd'), (NULL, NULL)"),
        );
        must_exec(
            &conn,
            &format!(r#"ALTER TABLE "{t}" ALTER COLUMN "price" TYPE numeric(12, 3) USING "price"::numeric(12, 3)"#),
        );
        let got = query_column(&conn, &format!("SELECT price::text FROM {t} WHERE id = 1"), 0);
        assert_eq!(got, ["9.500"]);
        let err = conn
            .exec(
                &format!(r#"ALTER TABLE "{t}" ALTER COLUMN "name" TYPE varchar(3)"#),
                &[],
            )
            .expect_err("too long");
        assert_sqlstate(&err, "22001");
        let typ = scan_str(
            &conn,
            &format!(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = '{t}'::regclass AND attname = 'name'"
            ),
        );
        assert_eq!(typ, "character varying(10)");
        assert_eq!(
            query_column(&conn, &format!("SELECT name FROM {t} WHERE id = 1"), 0),
            ["abcd"]
        );
    }
}
