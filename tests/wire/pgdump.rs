use std::process::Command;

use crate::common::*;

// dump_machinery exercises the catalog features pg_dump relies on, at the
// wire level (so it runs without the pg_dump binary installed).
#[test]
fn dump_machinery() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE t (id int PRIMARY KEY, code text)"#);

    // tableoid is exposed on every catalog view.
    let toid: i64 = conn
        .query_row(r#"SELECT tableoid FROM pg_catalog.pg_class WHERE relname = 't'"#, &[])
        .scan()
        .unwrap();
    assert_eq!(toid, 1259);

    // unnest of a literal array expands into rows (pg_dump drives bulk fetches
    // this way), including the column-alias-list syntax.
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT x::text FROM unnest('{10,20,30}') AS t(x) ORDER BY x"#,
            0,
        ),
        ["10", "20", "30"]
    );

    // array_agg is a real aggregate building a Postgres array literal.
    let agg: String = conn
        .query_row(
            r#"SELECT array_agg(v ORDER BY v) FROM (SELECT 3 AS v UNION SELECT 1 UNION SELECT 2)"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(agg, "{1,2,3}");

    // pg_get_indexdef reconstructs a full CREATE INDEX.
    must_exec(&conn, r#"CREATE INDEX t_code_idx ON t (code)"#);
    let def: String = conn
        .query_row(
            r#"SELECT pg_get_indexdef(indexrelid) FROM pg_catalog.pg_index i
		 JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid WHERE c.relname = 't_code_idx'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_contains!(def, "CREATE INDEX");
    assert_contains!(def, "USING btree (code)");
}

// sequence_readable_as_relation checks a sequence can be read as a one-row
// relation (last_value/is_called), which is how pg_dump captures its state.
#[test]
fn sequence_readable_as_relation() {
    let addr = start_server();
    let c1 = connect(&addr);
    must_exec(&c1, r#"CREATE SEQUENCE s START WITH 42"#);
    seq_val(&c1, r#"SELECT nextval('s')"#); // -> 42, is_called becomes true

    // A fresh connection materializes the sequence-as-relation view.
    let c2 = connect(&addr);
    let (last, called): (i64, bool) = c2
        .query_row(r#"SELECT last_value, is_called FROM s"#, &[])
        .scan()
        .unwrap();
    assert_eq!(last, 42);
    assert!(called);
}

// pg_dump_integration runs the real pg_dump against the server when the binary
// is available, asserting a usable schema dump.
#[test]
fn pg_dump_integration() {
    let Some(bin) = look_path("pg_dump") else {
        skip_test!("pg_dump not installed");
    };
    let addr = start_server();
    let conn = connect(&addr);
    must_exec(&conn, r#"CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')"#);
    must_exec(
        &conn,
        r#"CREATE TABLE dept (id int PRIMARY KEY, name text UNIQUE NOT NULL)"#,
    );
    must_exec(
        &conn,
        r#"CREATE TABLE emp (
		id int PRIMARY KEY, dept_id int REFERENCES dept(id), active boolean DEFAULT true, m mood)"#,
    );
    must_exec(&conn, r#"CREATE SEQUENCE order_seq START WITH 100 INCREMENT BY 5"#);

    let port = addr.port().to_string();
    let out = Command::new(bin)
        .args([
            "-h",
            addr.host(),
            "-p",
            &port,
            "-U",
            "postgres",
            "-d",
            "test",
            "--schema-only",
        ])
        .output()
        .expect("pg_dump must start");
    // CombinedOutput: stdout and stderr together.
    let dump = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "pg_dump failed: {dump}");

    assert_contains!(dump, "CREATE TABLE public.dept");
    assert_contains!(dump, "id integer NOT NULL"); // NOT NULL
    assert_contains!(dump, "active boolean DEFAULT true"); // DEFAULT
    assert_contains!(dump, "ADD CONSTRAINT emp_pkey PRIMARY KEY (id)"); // PK constraint
    // pg_dump empties the search_path, so every name is qualified.
    assert_contains!(dump, "FOREIGN KEY (dept_id) REFERENCES public.dept(id)");
    assert_contains!(dump, "ADD CONSTRAINT dept_name_key UNIQUE (name)"); // UNIQUE
    assert_contains!(dump, "CREATE SEQUENCE public.order_seq"); // sequence
    assert_contains!(dump, "INCREMENT BY 5");
    assert_contains!(dump, "CREATE TYPE public.mood AS ENUM"); // enum type (needs PREPARE/EXECUTE)
}

/// The major version of a pg_dump binary ("pg_dump (PostgreSQL) 18.1" is 18).
fn pg_dump_major(bin: &std::path::Path) -> u32 {
    let out = Command::new(bin).arg("--version").output().expect("pg_dump must start");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find_map(|w| w.split('.').next()?.parse().ok())
        .unwrap_or(0)
}

/// pg_dump of the test database, plain format.
fn dump(bin: &std::path::Path, addr: &Server, database: &str) -> String {
    let port = addr.port().to_string();
    let out = Command::new(bin)
        .args(["-h", addr.host(), "-p", &port, "-U", "postgres", "-d", database])
        .output()
        .expect("pg_dump must start");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "pg_dump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    text
}

// A database dumped by pg_dump and restored by psql into another comes back whole:
// what restores it is the dump's DDL (serial defaults, CHECKs, expression and partial
// indexes, generated columns, views, functions) and COPY's text of each value, which
// Postgres reads too. Each piece below was lost or broke the restore before.
#[test]
fn pg_dump_restores_into_a_new_database() {
    let (Some(bin), Some(psql)) = (look_path("pg_dump"), look_path("psql")) else {
        skip_test!("pg_dump or psql not installed");
    };
    // A virtual generated column is dumped as one by pg_dump 18 on; an older one
    // reads attgenerated 'v' as a plain default.
    let virtual_columns = pg_dump_major(&bin) >= 18;
    let half = if virtual_columns {
        ", half numeric GENERATED ALWAYS AS (price / 2)"
    } else {
        ""
    };
    let addr = start_cluster(8);
    let conn = dial_db(&addr, ENTRY_DATABASE).unwrap();
    let books = format!(
        "CREATE TABLE books (id bigserial PRIMARY KEY, author_id integer NOT NULL REFERENCES authors(id), \
         n serial, title text NOT NULL, price numeric(10,2) DEFAULT 0 CHECK (price >= 0), tags text[], \
         twice numeric GENERATED ALWAYS AS (price * 2) STORED{half})"
    );
    for sql in [
        "CREATE TABLE authors (id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name text NOT NULL, \
         created_at timestamptz NOT NULL DEFAULT now(), CONSTRAINT name_not_blank CHECK (name <> ''))",
        books.as_str(),
        "CREATE UNIQUE INDEX books_title_lower ON books (lower(title))",
        "CREATE INDEX books_dear ON books (author_id) WHERE price > 10",
        "CREATE SEQUENCE invoice_seq START 1000 INCREMENT 5",
        "CREATE VIEW cheap_books AS SELECT id, title FROM books WHERE price < 10",
        "CREATE FUNCTION add_one(i integer) RETURNS integer LANGUAGE sql AS 'SELECT i + 1'",
        "INSERT INTO authors (name, created_at) VALUES ('Ann', '2026-01-02 03:04:05.5+00')",
        "INSERT INTO books (author_id, title, price, tags) VALUES (1, 'First', 5.5, '{a,\"b c\"}'), (1, 'Second', 12, NULL)",
        "SELECT nextval('invoice_seq')",
        "CREATE DATABASE copy",
    ] {
        must_exec(&conn, sql);
    }
    let text = dump(&bin, &addr, ENTRY_DATABASE);
    for want in [
        // A sequence's default is set after the sequence is made, as Postgres' is.
        "ALTER TABLE ONLY public.books ALTER COLUMN id SET DEFAULT nextval('public.books_id_seq'::regclass);",
        "ALTER TABLE ONLY public.books ALTER COLUMN n SET DEFAULT nextval('public.books_n_seq'::regclass);",
        "SELECT id, title FROM public.books WHERE price < 10;",
        "CONSTRAINT name_not_blank CHECK (name <> '')",
        "CREATE UNIQUE INDEX books_title_lower ON public.books USING btree (lower(title));",
        "WHERE (price > 10)",
        "twice numeric GENERATED ALWAYS AS ((price * 2)) STORED",
        "{a,\"b c\"}",
        "2026-01-02 03:04:05.5+00",
    ] {
        assert!(text.contains(want), "the dump lacks {want}:\n{text}");
    }
    if virtual_columns {
        let want = "half numeric GENERATED ALWAYS AS ((price / 2))";
        assert!(text.contains(want), "the dump lacks {want}:\n{text}");
    }
    assert!(
        !text.contains("SUPPORT 0"),
        "a function's support is '-', not 0:\n{text}"
    );

    let port = addr.port().to_string();
    let out = Command::new(psql)
        .args([
            "-X",
            "-q",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            addr.host(),
            "-p",
            &port,
            "-U",
            "postgres",
            "-d",
            "copy",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.take().unwrap().write_all(text.as_bytes())?;
            c.wait_with_output()
        })
        .expect("psql must start");
    assert!(
        out.status.success(),
        "the restore failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let copy = dial_db(&addr, "copy").unwrap();
    let generated = if virtual_columns {
        "SELECT count(*) FROM books WHERE twice = price * 2 AND half = price / 2"
    } else {
        "SELECT count(*) FROM books WHERE twice = price * 2"
    };
    for (q, want) in [
        ("SELECT string_agg(title, ',' ORDER BY id) FROM books", "First,Second"),
        (generated, "2"),
        ("SELECT tags FROM books WHERE id = 1", "{a,\"b c\"}"),
        ("SELECT created_at FROM authors", "2026-01-02 03:04:05.5+00"),
        ("SELECT nextval('invoice_seq')", "1005"),
        ("SELECT add_one(1)", "2"),
        ("SELECT count(*) FROM cheap_books", "1"),
        // The defaults came back: a new row takes the next id of each sequence.
        (
            "INSERT INTO books (author_id, title) VALUES (1, 'Third') RETURNING id || ',' || n",
            "3,3",
        ),
        ("INSERT INTO authors (name) VALUES ('Bo') RETURNING id", "2"),
    ] {
        assert_eq!(scan_str(&copy, q), want, "{q}");
    }
    copy.exec("INSERT INTO books (author_id, title) VALUES (1, 'FIRST')", &[])
        .expect_err("the expression index is unique on lower(title)");
    copy.exec("INSERT INTO authors (name) VALUES ('')", &[])
        .expect_err("the CHECK came back");
}

// The catalogs pg_dump -Fc and pg_dumpall read before any object: a database's
// settings, the roles (without their passwords: every role may read pg_authid here)
// and each membership's options, as Postgres 16 has them.
#[test]
fn catalogs_of_pg_dump_fc_and_pg_dumpall() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE ROLE readers");
    must_exec(&conn, "CREATE ROLE app LOGIN PASSWORD 'secret'");
    must_exec(&conn, "GRANT readers TO app");
    assert_eq!(
        scan_str(&conn, "SELECT count(*) FROM pg_db_role_setting WHERE setrole = 0"),
        "0"
    );
    assert_eq!(
        scan_str(
            &conn,
            "SELECT string_agg(rolname || ':' || coalesce(rolpassword, '-'), ',' ORDER BY rolname) FROM pg_authid WHERE rolname IN ('app', 'readers')"
        ),
        "app:-,readers:-"
    );
    assert_eq!(
        scan_str(
            &conn,
            "SELECT ur.rolname || '>' || um.rolname || ':' || a.inherit_option::int || a.set_option::int FROM pg_auth_members a \
             JOIN pg_authid ur ON ur.oid = a.roleid JOIN pg_authid um ON um.oid = a.member"
        ),
        "readers>app:11"
    );
}

/// Feeds `script` to psql connected to `database`; ON_ERROR_STOP when `stop`.
fn psql_feed(psql: &std::path::Path, addr: &Server, database: &str, script: &[u8], stop: bool) -> std::process::Output {
    use std::io::Write;
    use std::process::Stdio;
    let port = addr.port().to_string();
    let on_error = if stop { "ON_ERROR_STOP=1" } else { "ON_ERROR_STOP=0" };
    let mut child = Command::new(psql)
        .args([
            "-X",
            "-q",
            "-v",
            on_error,
            "-h",
            addr.host(),
            "-p",
            &port,
            "-U",
            "postgres",
            "-d",
            database,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("psql must start");
    let mut stdin = child.stdin.take().unwrap();
    let script = script.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&script));
    let out = child.wait_with_output().expect("psql must finish");
    writer.join().unwrap().expect("psql reads its script");
    out
}

// pg_dumpall reads the global catalogs (roles, pg_parameter_acl, tablespaces) and then
// dumps each database; its script restores into another server. The one error is
// Postgres' own: the script creates the role it runs as, which is there already.
#[test]
fn pg_dumpall_restores_into_a_new_server() {
    let (Some(bin), Some(psql)) = (look_path("pg_dumpall"), look_path("psql")) else {
        skip_test!("pg_dumpall or psql not installed");
    };
    let addr = start_cluster(8);
    let conn = dial_db(&addr, ENTRY_DATABASE).unwrap();
    for sql in [
        "CREATE ROLE app LOGIN",
        "CREATE TABLE notes (id serial PRIMARY KEY, body text NOT NULL)",
        "INSERT INTO notes (body) VALUES ('one'), ('two')",
        "CREATE DATABASE shop",
    ] {
        must_exec(&conn, sql);
    }
    let shop = dial_db(&addr, "shop").unwrap();
    must_exec(&shop, "CREATE TABLE items (sku text PRIMARY KEY, qty int)");
    must_exec(&shop, "INSERT INTO items VALUES ('a-1', 3)");

    let port = addr.port().to_string();
    let out = Command::new(bin)
        .args(["-h", addr.host(), "-p", &port, "-U", "postgres"])
        .output()
        .expect("pg_dumpall must start");
    assert!(
        out.status.success(),
        "pg_dumpall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("CREATE ROLE app;"), "no role in:\n{text}");
    // The public schema keeps the comment it comes with, so none is written.
    assert!(!text.contains("COMMENT ON SCHEMA public"), "{text}");

    let fresh = start_cluster(8);
    let out = psql_feed(&psql, &fresh, ENTRY_DATABASE, text.as_bytes(), false);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let errors: Vec<&str> = stderr.lines().filter(|l| l.contains("ERROR")).collect();
    assert!(
        errors.len() == 1 && errors[0].contains("role \"postgres\" already exists"),
        "the restore failed: {stderr}"
    );
    let restored = dial_db(&fresh, ENTRY_DATABASE).unwrap();
    assert_eq!(
        scan_str(&restored, "SELECT string_agg(body, ',' ORDER BY id) FROM notes"),
        "one,two"
    );
    assert_eq!(
        scan_str(&restored, "SELECT count(*) FROM pg_roles WHERE rolname = 'app'"),
        "1"
    );
    let shop = dial_db(&fresh, "shop").unwrap();
    assert_eq!(scan_str(&shop, "SELECT sku || ':' || qty FROM items"), "a-1:3");
}

// The public schema's comment is Postgres' until it is changed, and COMMENT ON SCHEMA
// public IS NULL takes it away.
#[test]
fn public_schema_comment() {
    let conn = connect(&start_server());
    let q = "SELECT coalesce(obj_description('public'::regnamespace, 'pg_namespace'), '-')";
    assert_eq!(scan_str(&conn, q), "standard public schema");
    must_exec(&conn, "COMMENT ON SCHEMA public IS 'mine'");
    assert_eq!(scan_str(&conn, q), "mine");
    must_exec(&conn, "COMMENT ON SCHEMA public IS NULL");
    assert_eq!(scan_str(&conn, q), "-");
}

// A custom-format dump (pg_dump -Fc) restores with pg_restore into a new database.
// What Postgres 18 needs of it to restore there too: a view after the tables it reads
// (pg_rewrite and pg_depend say so, whatever the names' order), a sequence's default
// set after the sequence is made, and every name qualified, as pg_dump empties the
// search_path.
#[test]
fn pg_restore_of_a_custom_dump() {
    let (Some(dump_bin), Some(restore)) = (look_path("pg_dump"), look_path("pg_restore")) else {
        skip_test!("pg_dump or pg_restore not installed");
    };
    let addr = start_cluster(8);
    let conn = dial_db(&addr, ENTRY_DATABASE).unwrap();
    for sql in [
        "CREATE TABLE zauthors (id serial PRIMARY KEY, name text NOT NULL)",
        "CREATE TABLE zbooks (id bigserial PRIMARY KEY, author_id int NOT NULL REFERENCES zauthors(id), title text)",
        "CREATE VIEW a_titles AS SELECT b.title, a.name FROM zbooks b JOIN zauthors a ON a.id = b.author_id",
        "CREATE VIEW a_first AS SELECT title FROM a_titles WHERE title < 'M'",
        "CREATE SCHEMA shop",
        "CREATE TABLE shop.items (sku text PRIMARY KEY, book bigint REFERENCES zbooks(id))",
        "INSERT INTO zauthors (name) VALUES ('Ann')",
        "INSERT INTO zbooks (author_id, title) VALUES (1, 'First'), (1, 'Second')",
        "INSERT INTO shop.items VALUES ('a-1', 2)",
        "CREATE DATABASE copy",
    ] {
        must_exec(&conn, sql);
    }
    let port = addr.port().to_string();
    let file = addr.dir().join("db.dump");
    let file = file.to_str().unwrap();
    let out = Command::new(dump_bin)
        .args([
            "-h",
            addr.host(),
            "-p",
            &port,
            "-U",
            "postgres",
            "-Fc",
            "-f",
            file,
            ENTRY_DATABASE,
        ])
        .output()
        .expect("pg_dump must start");
    assert!(
        out.status.success(),
        "pg_dump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let script = Command::new(&restore)
        .args(["-f", "-", file])
        .output()
        .expect("pg_restore must start");
    let script = String::from_utf8_lossy(&script.stdout).into_owned();
    let at = |want: &str| script.find(want).unwrap_or_else(|| panic!("no {want} in:\n{script}"));
    assert!(
        at("CREATE TABLE public.zbooks") < at("CREATE VIEW public.a_titles"),
        "{script}"
    );
    assert!(
        at("CREATE VIEW public.a_titles") < at("CREATE VIEW public.a_first"),
        "{script}"
    );
    assert!(
        at("CREATE SEQUENCE public.zbooks_id_seq") < at("ALTER COLUMN id SET DEFAULT"),
        "{script}"
    );
    for want in [
        "FROM public.zbooks AS b JOIN public.zauthors AS a",
        "FROM public.a_titles",
        "nextval('public.zbooks_id_seq'::regclass)",
        "REFERENCES public.zbooks(id)",
    ] {
        at(want);
    }

    let out = Command::new(&restore)
        .args([
            "-h",
            addr.host(),
            "-p",
            &port,
            "-U",
            "postgres",
            "-d",
            "copy",
            "--exit-on-error",
            file,
        ])
        .output()
        .expect("pg_restore must start");
    assert!(
        out.status.success(),
        "pg_restore failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let copy = dial_db(&addr, "copy").unwrap();
    for (q, want) in [
        (
            "SELECT string_agg(title || ':' || name, ',' ORDER BY title) FROM a_titles",
            "First:Ann,Second:Ann",
        ),
        ("SELECT string_agg(title, ',') FROM a_first", "First"),
        ("SELECT sku || ':' || book FROM shop.items", "a-1:2"),
        // The defaults set after the tables were made take the sequences' next values.
        (
            "INSERT INTO zbooks (author_id, title) VALUES (1, 'Third') RETURNING id",
            "3",
        ),
        ("INSERT INTO zauthors (name) VALUES ('Bo') RETURNING id", "2"),
    ] {
        assert_eq!(scan_str(&copy, q), want, "{q}");
    }
}

// ALTER COLUMN ... SET DEFAULT nextval(...), as a dump sets a serial's default, gives
// a row that leaves the column out the sequence's next value; DROP DEFAULT stops it.
#[test]
fn set_default_nextval() {
    let conn = connect(&start_server());
    for sql in [
        "CREATE TABLE t (id bigint NOT NULL, v text)",
        "CREATE SEQUENCE t_id_seq START 10",
        "ALTER SEQUENCE t_id_seq OWNED BY t.id",
        "ALTER TABLE ONLY public.t ALTER COLUMN id SET DEFAULT nextval('public.t_id_seq'::regclass)",
    ] {
        must_exec(&conn, sql);
    }
    assert_eq!(scan_str(&conn, "INSERT INTO t (v) VALUES ('a') RETURNING id"), "10");
    assert_eq!(scan_str(&conn, "INSERT INTO t (v) VALUES ('b') RETURNING id"), "11");
    assert_eq!(
        scan_str(
            &conn,
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 't'::regclass"
        ),
        "nextval('t_id_seq'::regclass)"
    );
    must_exec(&conn, "ALTER TABLE t ALTER COLUMN id DROP DEFAULT");
    conn.exec("INSERT INTO t (v) VALUES ('c')", &[])
        .expect_err("no default: id is NULL");
}
