use crate::common::*;

const UUID_V7: &str = r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";

// Postgres 18's functions an application may send: uuidv7() above all, as a key that
// sorts by when it was made. Each answer here is Postgres 18's.
#[test]
fn postgres_18_functions() {
    let conn = connect(&start_server());
    assert_regexp(UUID_V7, &scan_str(&conn, "SELECT uuidv7()::text"));
    assert_regexp(r"^[0-9a-f]{8}-[0-9a-f]{4}-4", &scan_str(&conn, "SELECT uuidv4()::text"));
    for (q, want) in [
        ("SELECT uuid_extract_version(uuidv7())", "7"),
        ("SELECT uuid_extract_version(uuidv4())", "4"),
        (
            "SELECT uuid_extract_timestamp('019a6bd4-3c6b-7a5e-8f3b-1c2d3e4f5a6b'::uuid)",
            "2025-11-10 03:34:22.571+00",
        ),
        (
            "SELECT uuid_extract_timestamp('C232AB00-9414-11EC-B3C8-9F6BDECED846'::uuid)",
            "2022-02-22 19:22:22+00",
        ),
        (
            "SELECT uuid_extract_timestamp(uuidv7('-1 day')) < now() - interval '23 hours'",
            "t",
        ),
        ("SELECT casefold('HeLLo')", "hello"),
        ("SELECT crc32('abc')", "891568578"),
        ("SELECT crc32c('abc')", "910901175"),
    ] {
        assert_eq!(scan_str(&conn, q), want, "{q}");
    }
    let none: Option<String> = conn
        .query_row(
            "SELECT uuid_extract_version('00000000-0000-0000-0000-000000000000')::text",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(none, None, "the nil UUID has no RFC 9562 variant");

    // The result columns say uuid and timestamptz, as Postgres' do.
    let rows = conn
        .query("SELECT uuidv7(), uuid_extract_timestamp(uuidv7())", &[])
        .unwrap();
    let oids: Vec<u32> = rows.fields.iter().map(|f| f.data_type_oid).collect();
    assert_eq!(oids, [2950, 1184]);

    // A default of uuidv7() keys rows in the order they were made.
    must_exec(&conn, "CREATE TABLE ev (id uuid PRIMARY KEY DEFAULT uuidv7(), n int)");
    for n in 1..=20 {
        must_exec(&conn, &format!("INSERT INTO ev (n) VALUES ({n})"));
    }
    let order = scan_str(&conn, "SELECT string_agg(n::text, ',' ORDER BY id) FROM ev");
    let want: Vec<String> = (1..=20).map(|n| n.to_string()).collect();
    assert_eq!(order, want.join(","));
}

// A generated column without STORED is virtual in Postgres 18, computed when read, as
// SQLite's are: pg_attribute says so, with 'v', so psql and pg_dump 18 write it back
// without STORED.
#[test]
fn virtual_generated_columns() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE gv (a int, b int GENERATED ALWAYS AS (a * 2), c int GENERATED ALWAYS AS (a * 3) VIRTUAL, d int GENERATED ALWAYS AS (a * 4) STORED)",
    );
    must_exec(&conn, "INSERT INTO gv (a) VALUES (5)");
    assert_eq!(scan_str(&conn, "SELECT b || ',' || c || ',' || d FROM gv"), "10,15,20");
    assert_eq!(
        scan_str(
            &conn,
            "SELECT string_agg(attname || '=' || attgenerated, ' ' ORDER BY attnum) FROM pg_attribute WHERE attrelid = 'gv'::regclass AND attnum > 1"
        ),
        "b=v c=v d=s"
    );
}
