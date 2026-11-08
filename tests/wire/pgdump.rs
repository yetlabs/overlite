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
