use crate::common::*;

#[test]
fn copy_from_stdin_text() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, nome TEXT, obs TEXT)");

    // Text format: tab-delimited, \N for NULL.
    let data = "1\tana\tvip\n2\tbob\t\\N\n".as_bytes();
    let tag = conn.copy_from(data, "COPY t (id, nome, obs) FROM STDIN").unwrap();
    assert_eq!(tag.rows_affected(), 2);

    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 2);

    let (nome, obs): (String, Option<String>) = conn
        .query_row("SELECT nome, obs FROM t WHERE id = 2", &[])
        .scan()
        .unwrap();
    assert_eq!(nome, "bob");
    assert_eq!(obs, None); // \N became NULL
}

#[test]
fn copy_to_stdout_text() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, nome TEXT)");
    must_exec(&conn, "INSERT INTO t VALUES (1, 'ana'), (2, 'bob')");

    let mut buf = Vec::new();
    let tag = conn.copy_to(&mut buf, "COPY t TO STDOUT").unwrap();
    assert_eq!(tag.rows_affected(), 2);
    assert_eq!(String::from_utf8(buf).unwrap(), "1\tana\n2\tbob\n");
}

#[test]
fn copy_csv_round_trip() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, nome TEXT)");

    // Import CSV with header.
    let data = "id,nome\n1,ana\n2,\"bob, jr\"\n".as_bytes();
    conn.copy_from(data, "COPY t (id, nome) FROM STDIN WITH (FORMAT csv, HEADER)")
        .unwrap();

    let nome: String = conn.query_row("SELECT nome FROM t WHERE id = 2", &[]).scan().unwrap();
    assert_eq!(nome, "bob, jr"); // quoted comma preserved

    // Export CSV.
    let mut buf = Vec::new();
    conn.copy_to(&mut buf, "COPY t TO STDOUT WITH (FORMAT csv)").unwrap();
    assert_contains!(String::from_utf8(buf).unwrap(), r#""bob, jr""#);
}

// A round trip through TO STDOUT then FROM STDIN reproduces the data (the
// pg_dump pattern).
#[test]
fn copy_dump_restore() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE src (id INTEGER, nome TEXT)");
    must_exec(&conn, "INSERT INTO src VALUES (1, 'ana'), (2, 'bob'), (3, 'carol')");

    let mut dump = Vec::new();
    conn.copy_to(&mut dump, "COPY src TO STDOUT").unwrap();

    must_exec(&conn, "CREATE TABLE dst (id INTEGER, nome TEXT)");
    let tag = conn.copy_from(dump.as_slice(), "COPY dst FROM STDIN").unwrap();
    assert_eq!(tag.rows_affected(), 3);

    let n: i64 = conn.query_row("SELECT count(*) FROM dst", &[]).scan().unwrap();
    assert_eq!(n, 3);
}

// copy_end_of_data_marker feeds the terminator pg_dump writes after every COPY
// block. psql forwards it as ordinary data, so the server has to read it as the
// end of the stream rather than as a row — the COPY TO STDOUT above never emits
// one, which is how restoring a dump could lose every table's rows unnoticed.
#[test]
fn copy_end_of_data_marker() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, nome TEXT)");

    let data = "1\tana\n2\tbob\n\\.\n".as_bytes();
    let tag = conn.copy_from(data, "COPY t (id, nome) FROM STDIN").unwrap();
    assert_eq!(tag.rows_affected(), 2);

    let n: i64 = conn.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 2);
}

// parse_copy and parse_copy_options: the keywords in a query's literals or in an
// option's value are not the keywords.
#[test]
fn copy_keywords_not_in_a_literal() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (a text)");
    must_exec(&conn, "INSERT INTO t VALUES ('v')");

    let mut buf = Vec::new();
    conn.copy_to(&mut buf, "COPY (SELECT ' TO STDOUT ' AS a) TO STDOUT")
        .unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), " TO STDOUT \n");

    let mut buf = Vec::new();
    conn.copy_to(&mut buf, "COPY t TO STDOUT (DELIMITER E'\\t', NULL 'csv header')")
        .unwrap();
    assert_eq!(String::from_utf8(buf).unwrap(), "v\n");
}

// COPY TO writes each value as Postgres' text output does, which is what pg_dump
// restores from: an array as `{...}`, not the JSON it is stored as, a timestamptz with
// its offset, a bytea as `\x...`, a boolean as t or f.
#[test]
fn copy_to_writes_values_as_postgres_does() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE c (tags text[], nums int[], at timestamptz, b bytea, ok boolean)",
    );
    must_exec(
        &conn,
        r"INSERT INTO c VALUES (ARRAY['a','b c'], '{1,2}', '2026-01-02 03:04:05.5+00', '\x00ff', true)",
    );
    let mut buf = Vec::new();
    conn.copy_to(&mut buf, "COPY c TO STDOUT").unwrap();
    assert_eq!(
        String::from_utf8(buf).unwrap(),
        "{a,\"b c\"}\t{1,2}\t2026-01-02 03:04:05.5+00\t\\\\x00ff\tt\n"
    );
    let mut buf = Vec::new();
    conn.copy_to(&mut buf, "COPY (SELECT tags, at FROM c) TO STDOUT WITH (FORMAT csv)")
        .unwrap();
    assert_eq!(
        String::from_utf8(buf).unwrap(),
        "\"{a,\"\"b c\"\"}\",2026-01-02 03:04:05.5+00\n"
    );
}
