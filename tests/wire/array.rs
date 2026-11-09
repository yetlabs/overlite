use crate::common::*;

// Array columns (stored as JSON) round-trip through the wire as real Postgres
// arrays, built with ARRAY[...] and with '{...}'::type[].
#[test]
fn array_round_trip() {
    let conn = connect(&start_server()); // simple protocol (text), like psql

    conn.exec("CREATE TABLE t (id int, tags text[], nums int[])", &[])
        .unwrap();
    conn.exec("INSERT INTO t VALUES (1, ARRAY['a','b'], ARRAY[10,20])", &[])
        .unwrap();
    conn.exec("INSERT INTO t VALUES (2, '{c,d}'::text[], '{30,40}'::int[])", &[])
        .unwrap();

    let (tags, nums): (Vec<String>, Vec<i64>) = conn
        .query_row("SELECT tags, nums FROM t WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert_eq!(tags, ["a", "b"]);
    assert_eq!(nums, [10, 20]);

    let (tags, nums): (Vec<String>, Vec<i64>) = conn
        .query_row("SELECT tags, nums FROM t WHERE id = 2", &[])
        .scan()
        .unwrap();
    assert_eq!(tags, ["c", "d"]);
    assert_eq!(nums, [30, 40]);

    // An empty array and one with a quoted element (contains a comma).
    conn.exec("INSERT INTO t VALUES (3, ARRAY['x,y'], ARRAY[]::int[])", &[])
        .unwrap();
    // Option: an empty array is not NULL, so a NULL must not pass here.
    let (tags, nums): (Vec<String>, Option<Vec<i64>>) = conn
        .query_row("SELECT tags, nums FROM t WHERE id = 3", &[])
        .scan()
        .unwrap();
    assert_eq!(tags, ["x,y"]);
    assert_eq!(nums, Some(vec![]));
}

// TestArrayOps: element access, length, and membership on stored arrays.
#[test]
fn array_ops() {
    let conn = connect(&start_server());
    conn.exec("CREATE TABLE t (id int, tags text[], nums int[])", &[])
        .unwrap();
    conn.exec("INSERT INTO t VALUES (1, ARRAY['a','b','c'], ARRAY[10,20,30])", &[])
        .unwrap();

    // Subscript is 1-based.
    let s: String = conn.query_row("SELECT tags[2] FROM t", &[]).scan().unwrap();
    assert_eq!(s, "b");
    let n: i64 = conn.query_row("SELECT nums[1] FROM t", &[]).scan().unwrap();
    assert_eq!(n, 10);

    // array_length / cardinality.
    let l: i64 = conn
        .query_row("SELECT array_length(tags, 1) FROM t", &[])
        .scan()
        .unwrap();
    assert_eq!(l, 3);
    let c: i64 = conn.query_row("SELECT cardinality(nums) FROM t", &[]).scan().unwrap();
    assert_eq!(c, 3);

    // = ANY(arr) membership, a boolean as in Postgres, and in WHERE.
    let found: bool = conn.query_row("SELECT 'b' = ANY(tags) FROM t", &[]).scan().unwrap();
    let missing: bool = conn.query_row("SELECT 'z' = ANY(tags) FROM t", &[]).scan().unwrap();
    assert!(found);
    assert!(!missing);
    let cnt: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE 20 = ANY(nums)", &[])
        .scan()
        .unwrap();
    assert_eq!(cnt, 1);
    let cnt: i64 = conn
        .query_row("SELECT count(*) FROM t WHERE 99 = ANY(nums)", &[])
        .scan()
        .unwrap();
    assert_eq!(cnt, 0);
}

// TestArrayUnnest: unnest expands an array to a set of rows.
#[test]
fn array_unnest() {
    let conn = connect(&start_server());
    conn.exec("CREATE TABLE t (id int, tags text[])", &[]).unwrap();
    conn.exec("INSERT INTO t VALUES (1, ARRAY['a','b','c'])", &[]).unwrap();

    let rows = conn.query("SELECT unnest(tags) FROM t ORDER BY 1", &[]).unwrap();
    let mut got: Vec<String> = Vec::new();
    for row in &rows {
        let v: String = row.scan().unwrap();
        got.push(v);
    }
    assert_eq!(got, ["a", "b", "c"]);
}

fn binary_cell(conn: &Conn, sql: &str) -> Cell {
    let rows = conn.query_formats(sql, &[BINARY_FORMAT], &[]).unwrap();
    rows.rows[0].cells[0].clone()
}

// array_send: ndim, has-null flag, element OID, then for each dimension its size and
// lower bound, then each element with its length.
#[test]
fn array_binary_send() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE t (id int, tags text[], nums int[], flags boolean[], fl float8[], m int[])",
    );
    must_exec(
        &conn,
        "INSERT INTO t VALUES (1, ARRAY['a',NULL], ARRAY[10,20], '{t,f}'::boolean[], ARRAY[1.5], ARRAY[ARRAY[1,2],ARRAY[3,4]])",
    );
    must_exec(
        &conn,
        "INSERT INTO t VALUES (2, ARRAY[]::text[], ARRAY[]::int[], NULL, NULL, NULL)",
    );

    let c = binary_cell(&conn, "SELECT tags FROM t WHERE id = 1");
    assert_eq!((c.oid, c.format), (1009, BINARY_FORMAT));
    assert_eq!(
        hex::encode(c.raw()),
        concat!(
            "00000001", "00000001", "00000019", "00000002", "00000001", "00000001", "61", "ffffffff"
        )
    );

    // An int[] is Postgres' int4[], its elements four bytes each.
    let c = binary_cell(&conn, "SELECT nums FROM t WHERE id = 1");
    assert_eq!(c.oid, 1007);
    assert_eq!(
        hex::encode(c.raw()),
        concat!(
            "00000001", "00000000", "00000017", "00000002", "00000001", "00000004", "0000000a", "00000004", "00000014"
        )
    );

    let c = binary_cell(&conn, "SELECT flags FROM t WHERE id = 1");
    assert_eq!(c.oid, 1000);
    assert_eq!(
        hex::encode(c.raw()),
        concat!(
            "00000001", "00000000", "00000010", "00000002", "00000001", "00000001", "01", "00000001", "00"
        )
    );

    let c = binary_cell(&conn, "SELECT fl FROM t WHERE id = 1");
    assert_eq!(c.oid, 1022);
    assert_eq!(
        hex::encode(c.raw()),
        concat!(
            "00000001",
            "00000000",
            "000002bd",
            "00000001",
            "00000001",
            "00000008",
            "3ff8000000000000"
        )
    );

    let c = binary_cell(&conn, "SELECT m FROM t WHERE id = 1");
    assert_eq!(
        hex::encode(c.raw()),
        concat!(
            "00000002", "00000000", "00000017", "00000002", "00000001", "00000002", "00000001", "00000004", "00000001",
            "00000004", "00000002", "00000004", "00000003", "00000004", "00000004"
        )
    );

    // An empty array has no dimensions.
    let c = binary_cell(&conn, "SELECT tags FROM t WHERE id = 2");
    assert_eq!(hex::encode(c.raw()), concat!("00000000", "00000000", "00000019"));
    let c = binary_cell(&conn, "SELECT flags FROM t WHERE id = 2");
    assert!(c.is_null());
}

// As a client reads them in pgx's default mode, which asks arrays in binary.
#[test]
fn array_binary_scan() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE t (id int, tags text[], nums int[], d date[], ts timestamp[], u uuid[], b bytea[])",
    );
    must_exec(
        &conn,
        r#"INSERT INTO t VALUES (1, ARRAY['x,y','z "q"'], ARRAY[10,20], '{2024-01-02}'::date[],
            '{"2024-01-02 03:04:05"}'::timestamp[], '{00000000-0000-0000-0000-000000000001}'::uuid[], NULL)"#,
    );

    let (tags, nums): (Vec<String>, Vec<i64>) = conn.query_row("SELECT tags, nums FROM t", &[]).scan().unwrap();
    assert_eq!(tags, ["x,y", r#"z "q""#]);
    assert_eq!(nums, [10, 20]);
    let u: Vec<String> = conn.query_row("SELECT u FROM t", &[]).scan().unwrap();
    assert_eq!(u, ["00000000-0000-0000-0000-000000000001"]);

    // date_send and timestamp_send inside the array, in days and microseconds.
    let c = binary_cell(&conn, "SELECT d FROM t");
    assert_eq!(c.oid, 1182);
    assert_eq!(hex::encode(&c.raw()[20..]), "000000040000223f");
    let c = binary_cell(&conn, "SELECT ts FROM t");
    assert_eq!(c.oid, 1115);
    assert_eq!(&c.raw()[20..24], &[0, 0, 0, 8]);

    // A value bound as text, in Postgres' own spelling, is stored as sent.
    conn.exec("INSERT INTO t (id, nums) VALUES (2, $1)", &[&"{1,NULL,3}"])
        .unwrap();
    let nums: Vec<Option<i64>> = conn.query_row("SELECT nums FROM t WHERE id = 2", &[]).scan().unwrap();
    assert_eq!(nums, [Some(1), None, Some(3)]);
}

// numeric_send inside array_send, against the bytes Postgres gives.
#[test]
fn numeric_array_binary() {
    let conn = connect_extended(&start_server());
    // dec[]: numeric[] does not get past the rewrite of a numeric column.
    must_exec(&conn, "CREATE TABLE t (n dec[])");
    must_exec(&conn, "INSERT INTO t VALUES ('{1.5,NULL}'::dec[])");
    let c = binary_cell(&conn, "SELECT n FROM t");
    assert_eq!(c.oid, 1231);
    assert_eq!(
        hex::encode(c.raw()),
        concat!(
            "00000001",
            "00000001",
            "000006a4",
            "00000002",
            "00000001",
            "0000000c",
            "000200000000000100011388",
            "ffffffff"
        )
    );
}

const ALL_ARRAYS: &str = "CREATE TABLE a (n numeric(10,2)[], m numeric[], t text[], v varchar(20)[], i int[], \
    b bigint[], ts timestamp(3)[], u uuid[], bo boolean[], tz timestamptz[], d date[], f float8[], j jsonb[], \
    w character varying(5)[], x timestamp(3) without time zone[], plain numeric(10,2))";

// What the catalogs say of array columns, as PostgreSQL 17 answers for this table.
#[test]
fn array_columns_in_the_catalogs() {
    let conn = connect(&start_server());
    must_exec(&conn, ALL_ARRAYS);

    let rows = conn
        .query(
            "SELECT column_name, data_type, udt_name, numeric_precision, numeric_scale, \
             character_maximum_length, datetime_precision FROM information_schema.columns \
             WHERE table_name = 'a' ORDER BY ordinal_position",
            &[],
        )
        .unwrap();
    let got: Vec<String> = rows
        .iter()
        .map(|r| {
            let cells: Vec<String> = (0..7).map(|i| r.cell(i).text()).collect();
            cells.join("|")
        })
        .collect();
    assert_eq!(
        got,
        [
            "n|ARRAY|_numeric||||",
            "m|ARRAY|_numeric||||",
            "t|ARRAY|_text||||",
            "v|ARRAY|_varchar||||",
            "i|ARRAY|_int4||||",
            "b|ARRAY|_int8||||",
            "ts|ARRAY|_timestamp||||",
            "u|ARRAY|_uuid||||",
            "bo|ARRAY|_bool||||",
            "tz|ARRAY|_timestamptz||||",
            "d|ARRAY|_date||||",
            "f|ARRAY|_float8||||",
            "j|ARRAY|_jsonb||||",
            "w|ARRAY|_varchar||||",
            "x|ARRAY|_timestamp||||",
            "plain|numeric|numeric|10|2||",
        ]
    );

    let rows = conn
        .query(
            "SELECT attname, atttypid, atttypmod, format_type(atttypid, atttypmod), attndims FROM pg_attribute \
             WHERE attrelid = 'a'::regclass AND attnum > 0 ORDER BY attnum",
            &[],
        )
        .unwrap();
    let got: Vec<String> = rows
        .iter()
        .map(|r| {
            let cells: Vec<String> = (0..5).map(|i| r.cell(i).text()).collect();
            cells.join("|")
        })
        .collect();
    assert_eq!(
        got,
        [
            "n|1231|655366|numeric(10,2)[]|1",
            "m|1231|-1|numeric[]|1",
            "t|1009|-1|text[]|1",
            "v|1015|24|character varying(20)[]|1",
            "i|1007|-1|integer[]|1",
            "b|1016|-1|bigint[]|1",
            "ts|1115|3|timestamp(3) without time zone[]|1",
            "u|2951|-1|uuid[]|1",
            "bo|1000|-1|boolean[]|1",
            "tz|1185|-1|timestamp with time zone[]|1",
            "d|1182|-1|date[]|1",
            "f|1022|-1|double precision[]|1",
            "j|3807|-1|jsonb[]|1",
            "w|1015|9|character varying(5)[]|1",
            "x|1115|3|timestamp(3) without time zone[]|1",
            "plain|1700|655366|numeric(10,2)|0",
        ]
    );

    // Clients join on udt_name and read typelem.
    let (name, elem, category): (String, i64, String) = conn
        .query_row(
            "SELECT typname, typelem, typcategory FROM pg_type WHERE oid = 1231",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!((name.as_str(), elem, category.as_str()), ("_numeric", 1700, "A"));
}

// A modifier before the brackets is kept, the column stays an array, and it survives
// a rebuild of the table.
#[test]
fn array_of_modified_type() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE p (id int, prices numeric(10,2)[], tags varchar(20)[], at timestamp(3)[])",
    );
    must_exec(
        &conn,
        "INSERT INTO p VALUES (1, '{1.5,2.25}', '{a,b}', '{\"2024-01-02 03:04:05\"}')",
    );
    let typmod = || -> Vec<String> {
        query_column(
            &conn,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'p'::regclass AND attnum = 2",
            0,
        )
    };
    assert_eq!(typmod(), ["numeric(10,2)[]"]);

    for format in [TEXT_FORMAT, BINARY_FORMAT] {
        let rows = conn
            .query_formats("SELECT prices, tags FROM p", &[format], &[])
            .unwrap();
        assert_eq!(rows.fields[0].data_type_oid, 1231);
        let prices: Vec<String> = rows.iter().next().unwrap().get(0).unwrap();
        assert_eq!(prices, ["1.50", "2.25"]);
    }

    must_exec(&conn, "ALTER TABLE p ALTER COLUMN id SET NOT NULL");
    assert_eq!(typmod(), ["numeric(10,2)[]"], "through a rebuild");
    must_exec(&conn, "ALTER TABLE p ADD COLUMN more numeric(4,1)[]");
    must_exec(&conn, "ALTER TABLE p ALTER COLUMN tags TYPE varchar(30)[]");
    let v: Vec<String> = query_column(
        &conn,
        "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'p'::regclass AND attnum > 2 ORDER BY attnum",
        0,
    );
    assert_eq!(
        v,
        [
            "character varying(30)[]",
            "timestamp(3) without time zone[]",
            "numeric(4,1)[]"
        ]
    );
}

// node-postgres binds an array as Postgres' text, '{1,2}', with no type. `= ANY($1)`
// read only a JSON array, so it matched nothing where Postgres matched both.
#[test]
fn any_of_an_array_parameter_in_text() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE an (id int primary key, name text)");
    must_exec(&conn, "INSERT INTO an VALUES (1, 'a'), (2, 'b,c'), (3, 'd')");
    let ids = |sql: &str, param: &str| -> Vec<String> {
        let r = conn.exec_params(sql, &[param], &[], &[], &[]).unwrap();
        (0..r.rows.len()).map(|i| r.text(i, 0)).collect()
    };
    assert_eq!(
        ids("SELECT id FROM an WHERE id = ANY($1) ORDER BY id", "{1,2}"),
        ["1", "2"]
    );
    assert_eq!(
        ids("SELECT id FROM an WHERE name = ANY($1) ORDER BY id", r#"{a,"b,c"}"#),
        ["1", "2"]
    );
    assert_eq!(ids("SELECT id FROM an WHERE id <> ALL($1) ORDER BY id", "{1,2}"), ["3"]);
    assert_eq!(ids("SELECT id FROM an WHERE id = ANY($1) ORDER BY id", "[3]"), ["3"]);
    assert_eq!(ids("SELECT id FROM an WHERE id = ANY($1)", "{}"), Vec::<String>::new());
}
