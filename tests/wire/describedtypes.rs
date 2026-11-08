use crate::common::*;

// Rows are encoded in the types Describe advertised. Describe types a computed
// column by a probe run of the statement; when that run finds no rows it can
// only say text, and the rows of the real run used to go out as binary
// integers under that text description.
#[test]
fn rows_follow_described_types() {
    let conn = connect_extended(&start_server());
    conn.exec(r#"CREATE TABLE t (id int PRIMARY KEY, name text)"#, &[])
        .unwrap();
    conn.exec(r#"INSERT INTO t VALUES (7, 'a')"#, &[]).unwrap();

    // Binary results, as tokio-postgres (Prisma's driver) asks for them.
    let n: String = conn
        .query_row_formats(
            r#"SELECT id + 0 AS n FROM t WHERE name = $1"#,
            &[BINARY_FORMAT],
            &[&"a"],
        )
        .scan()
        .unwrap();
    assert_eq!(n, "7");
}

// The integer columns of a catalog query are described as integers even when
// the probe run finds nothing, as Prisma's introspection needs them.
#[test]
fn catalog_integers_described() {
    let conn = connect_extended(&start_server());
    conn.exec(r#"CREATE TABLE p (id int PRIMARY KEY)"#, &[]).unwrap();
    conn.exec(
        r#"CREATE TABLE c (id int PRIMARY KEY, p_id int REFERENCES p (id))"#,
        &[],
    )
    .unwrap();

    let (con_id, child): (i64, i64) = conn
        .query_row(
            r#"SELECT con.oid AS con_id, k.attnum AS child
		FROM pg_constraint con
		JOIN pg_namespace ns ON ns.oid = con.connamespace
		JOIN pg_attribute k ON k.attrelid = con.conrelid AND k.attname = 'p_id'
		WHERE ns.nspname = ANY ($1) AND con.contype = 'f'"#,
            &[&vec!["public".to_string()]],
        )
        .scan()
        .unwrap();
    assert!(con_id > 0, "{con_id} is not positive");
    assert_eq!(child, 2);
}

// is_catalog_query: naming a catalog in a literal does not type a text column as an
// integer because it is called oid.
#[test]
fn catalog_query_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE VIEW v AS SELECT 'abc' AS oid");
    let rows = conn
        .query("SELECT 'pg_class' AS a, oid FROM v WHERE 1 = 0", &[])
        .unwrap();
    assert_eq!(rows.fields[1].data_type_oid, 25);
    // A catalog named as an identifier still counts, quoted too.
    let rows = conn
        .query(r#"SELECT oid FROM "pg_class" WHERE relname = 'none'"#, &[])
        .unwrap();
    assert_eq!(rows.fields[0].data_type_oid, 20);
}

// With no row to sample, a computed column's type is what the statement shows: a cast,
// the call around it. Each OID is the one Postgres 17 described, but that an integer
// is int8 here, as an integer column is.
#[test]
fn computed_columns_with_no_row() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE t (a text, n int)");
    let ext = connect_extended(&server);
    for (item, oid) in [
        ("a::timestamp", 1114),
        ("a::timestamptz", 1184),
        ("a::date", 1082),
        ("a::time", 1083),
        ("a::uuid", 2950),
        ("a::jsonb", 3802),
        ("a::json", 114),
        ("a::numeric", 1700),
        ("a::numeric(10, 2)", 1700),
        ("a::float8", 701),
        ("a::bool", 16),
        ("a::bytea", 17),
        ("a::varchar", 1043),
        ("CAST(a AS timestamp)", 1114),
        ("a::int", 20),
        ("a::int AS labelled", 20),
        ("count(*)", 20),
        ("lower(a)", 25),
        ("'x'", 25),
        ("a::timestamp AT TIME ZONE 'UTC'", 1184),
        ("a::timestamptz AT TIME ZONE 'UTC'", 1114),
    ] {
        let sql = format!("SELECT n, {item} FROM t WHERE n = -1");
        let desc = ext.prepare("", &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(desc.fields[1].data_type_oid, oid, "{sql}");
    }
}

// A cast decides the type with a row to sample too, as its value is a text here.
#[test]
fn a_cast_decides_over_a_sample() {
    let conn = connect_extended(&start_server());
    for (sql, oid) in [
        ("SELECT '2024-01-02 03:04:05'::timestamp", 1114),
        ("SELECT '2024-01-02 03:04:05+02'::timestamptz", 1184),
        ("SELECT '2024-01-02'::date", 1082),
        ("SELECT '03:04:05'::time", 1083),
        ("SELECT '1.50'::numeric", 1700),
        ("SELECT '12345678-1234-1234-1234-123456789012'::uuid", 2950),
    ] {
        let desc = conn.prepare("", sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(desc.fields[0].data_type_oid, oid, "{sql}");
    }
    // And the text is the type's: a timestamptz shows its offset.
    let rows = conn
        .query_formats("SELECT '2024-01-02 03:04:05+02'::timestamptz", &[TEXT_FORMAT], &[])
        .unwrap();
    assert_eq!(rows.iter().next().unwrap().cell(0).text(), "2024-01-02 01:04:05+00");
}
