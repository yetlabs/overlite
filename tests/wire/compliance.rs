use crate::common::*;

// unnest_with_ordinality expands a literal array to rows carrying a 1-based
// ordinality column.
#[test]
fn unnest_with_ordinality() {
    let conn = connect(&start_server());
    assert_eq!(
        query_column(
            &conn,
            r#"SELECT v || ':' || n FROM unnest('{10,20,30}') WITH ORDINALITY AS t(v, n) ORDER BY n"#,
            0,
        ),
        ["10:1", "20:2", "30:3"]
    );
}

// pg_proc_populated checks pg_proc lists the functions we provide (so \df
// works and tools that introspect functions see them).
#[test]
fn pg_proc_populated() {
    let conn = connect(&start_server());
    let names = query_column(&conn, r#"SELECT proname FROM pg_catalog.pg_proc ORDER BY proname"#, 0);
    assert_ne!(names, [] as [std::string::String; 0]);
    assert_subset(&names, &["version", "now", "age", "to_char"]);
}

// information_schema_routines checks routines is populated from the same set.
#[test]
fn information_schema_routines() {
    let conn = connect(&start_server());
    let names = query_column(
        &conn,
        r#"SELECT routine_name FROM information_schema.routines WHERE routine_schema = 'pg_catalog'"#,
        0,
    );
    assert_contains!(names, "version");
    // routines exposes routine_type = 'FUNCTION'.
    let types = query_column(
        &conn,
        r#"SELECT DISTINCT routine_type FROM information_schema.routines"#,
        0,
    );
    assert_eq!(types, ["FUNCTION"]);
}

// composite_type records CREATE TYPE ... AS (...) in the catalog (typtype 'c').
#[test]
fn composite_type() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TYPE addr AS (street text, zip text)"#);

    assert_eq!(
        query_column(
            &conn,
            r#"SELECT typtype FROM pg_catalog.pg_type WHERE typname = 'addr'"#,
            0,
        ),
        ["c"]
    );
    // It shows up in the type list (psql \dT).
    assert_contains!(
        query_column(
            &conn,
            r#"SELECT typname FROM pg_catalog.pg_type WHERE typtype = 'c'"#,
            0,
        ),
        "addr"
    );

    must_exec(&conn, r#"DROP TYPE addr"#);
    assert_not_contains!(
        query_column(
            &conn,
            r#"SELECT typname FROM pg_catalog.pg_type WHERE typtype = 'c'"#,
            0,
        ),
        "addr"
    );
}

// array_to_string_of_subquery covers array_to_string(array(SELECT ...), sep),
// which \dT+ uses to render an enum's element list.
#[test]
fn array_to_string_of_subquery() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')"#);

    let elems: String = conn
        .query_row(
            r#"SELECT array_to_string(array(
			SELECT e.enumlabel FROM pg_catalog.pg_enum e
			JOIN pg_catalog.pg_type t ON t.oid = e.enumtypid
			WHERE t.typname = 'mood' ORDER BY e.enumsortorder), ', ')"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(elems, "sad, ok, happy");
}

// sql_prepare_execute covers SQL-level PREPARE/EXECUTE/DEALLOCATE (used by
// psql and pg_dump).
#[test]
fn sql_prepare_execute() {
    let conn = connect(&start_server());
    must_exec(&conn, r#"CREATE TABLE t (id int, name text)"#);
    must_exec(&conn, r#"INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')"#);

    must_exec(&conn, r#"PREPARE byId (int) AS SELECT name FROM t WHERE id = $1"#);
    let name: String = conn.query_row(r#"EXECUTE byId (2)"#, &[]).scan().unwrap();
    assert_eq!(name, "b");
    let name: String = conn.query_row(r#"EXECUTE byId (3)"#, &[]).scan().unwrap();
    assert_eq!(name, "c");

    must_exec(&conn, r#"DEALLOCATE byId"#);
    conn.exec(r#"EXECUTE byId (1)"#, &[])
        .expect_err("deallocated statement must be gone");
}

// age computes a calendar interval between two timestamps.
#[test]
fn age() {
    let conn = connect(&start_server());

    let s: String = conn
        .query_row(r#"SELECT age('2020-03-15', '2019-01-10')"#, &[])
        .scan()
        .unwrap();
    assert_eq!(s, "1 year 2 mons 5 days");

    let s: String = conn
        .query_row(r#"SELECT age('2020-03-05', '2019-01-10')"#, &[])
        .scan()
        .unwrap();
    assert_eq!(s, "1 year 1 mon 24 days");

    let s: String = conn
        .query_row(r#"SELECT age('2020-01-10', '2020-01-10')"#, &[])
        .scan()
        .unwrap();
    assert_eq!(s, "00:00:00");
}

// A statement that takes the full rewrite keeps the text of its literals, stored or
// returned: each of these came back as the SQL it spelled, or as a syntax error.
#[test]
fn rewrites_stay_out_of_literals() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, a text)");
    for lit in [
        "x = ANY(y)",
        "EXTRACT(year FROM x)",
        "generate_series(1,2)",
        "unnest(a)",
        "AS primary",
    ] {
        let rs = conn.pg_exec(format!("SELECT '{lit}', 1::text")).unwrap();
        assert_eq!(rs[0].text(0, 0), lit);
        must_exec(&conn, "DELETE FROM t");
        must_exec(&conn, &format!("INSERT INTO t VALUES (1::int, '{lit}')"));
        let got: String = conn.query_row("SELECT a FROM t", &[]).scan().unwrap();
        assert_eq!(got, lit);
    }
}
