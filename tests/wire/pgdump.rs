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
    assert_contains!(dump, "FOREIGN KEY (dept_id) REFERENCES dept(id)");
    assert_contains!(dump, "ADD CONSTRAINT dept_name_key UNIQUE (name)"); // UNIQUE
    assert_contains!(dump, "CREATE SEQUENCE public.order_seq"); // sequence
    assert_contains!(dump, "INCREMENT BY 5");
    assert_contains!(dump, "CREATE TYPE public.mood AS ENUM"); // enum type (needs PREPARE/EXECUTE)
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
    let addr = start_cluster(8);
    let conn = dial_db(&addr, ENTRY_DATABASE).unwrap();
    for sql in [
        "CREATE TABLE authors (id integer GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name text NOT NULL, \
         created_at timestamptz NOT NULL DEFAULT now(), CONSTRAINT name_not_blank CHECK (name <> ''))",
        "CREATE TABLE books (id bigserial PRIMARY KEY, author_id integer NOT NULL REFERENCES authors(id), \
         n serial, title text NOT NULL, price numeric(10,2) DEFAULT 0 CHECK (price >= 0), tags text[], \
         twice numeric GENERATED ALWAYS AS (price * 2) STORED, half numeric GENERATED ALWAYS AS (price / 2))",
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
        "id bigint DEFAULT nextval('books_id_seq'::regclass) NOT NULL",
        "n integer DEFAULT nextval('books_n_seq'::regclass) NOT NULL",
        "CONSTRAINT name_not_blank CHECK (name <> '')",
        "CREATE UNIQUE INDEX books_title_lower ON public.books USING btree (lower(title));",
        "WHERE (price > 10)",
        "twice numeric GENERATED ALWAYS AS ((price * 2)) STORED",
        "half numeric GENERATED ALWAYS AS ((price / 2))",
        "{a,\"b c\"}",
        "2026-01-02 03:04:05.5+00",
    ] {
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
    for (q, want) in [
        ("SELECT string_agg(title, ',' ORDER BY id) FROM books", "First,Second"),
        (
            "SELECT count(*) FROM books WHERE twice = price * 2 AND half = price / 2",
            "2",
        ),
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
