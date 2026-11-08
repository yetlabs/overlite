use crate::common::*;
use std::path::Path;

// Every write stores the storage format's canonical form (ARCHITECTURE.md, "Storage
// format"), whatever path the value takes: a literal, a text or binary parameter, COPY,
// a column's DEFAULT, WITH ... INSERT, UPDATE ... SET (a, b) = (...), INSERT ...
// SELECT and ON CONFLICT DO UPDATE. Each value goes in a spelling other than its
// canonical one, and SQLite alone reads what was stored. What a type's input refuses
// is refused with Postgres' code.

const UUID_IN: &str = "{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}";

/// Column, input text, SQL type, and what SQLite holds after: typeof and text (hex for
/// a blob).
const TYPES: &[(&str, &str, &str, &str)] = &[
    ("b", r"\xdeadbeef", "bytea", "blob:DEADBEEF"),
    ("u", UUID_IN, "uuid", "text:a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
    ("d", "Jan 2 2024", "date", "text:2024-01-02"),
    ("t", "04:05:06.500", "time", "text:04:05:06.5"),
    ("tz", "04:05:06.500+02", "timetz", "text:04:05:06.5+02"),
    ("i", "1 day 2 hours", "interval", "text:1 day 02:00:00"),
    ("ia", "{1,2}", "int[]", "text:[1,2]"),
    ("ta", r#"{a,"b c"}"#, "text[]", r#"text:["a","b c"]"#),
    (
        "ua",
        "{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}",
        "uuid[]",
        r#"text:["a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"]"#,
    ),
    ("h", r#""a"=>"1", b=>NULL"#, "hstore", r#"text:{"a":"1","b":null}"#),
    ("bo", "yes", "boolean", "integer:1"),
];

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn columns() -> String {
    TYPES.iter().map(|t| t.0).collect::<Vec<_>>().join(", ")
}

fn literals() -> String {
    TYPES.iter().map(|t| quote(t.1)).collect::<Vec<_>>().join(", ")
}

/// What SQLite holds for row id, column by column.
fn stored(path: &Path, id: i64) -> Vec<String> {
    let db = rusqlite::Connection::open(path).unwrap();
    TYPES
        .iter()
        .map(|(col, ..)| {
            db.query_row(
                &format!(
                    "SELECT typeof({col}) || ':' || CASE typeof({col}) WHEN 'blob' THEN hex({col}) ELSE {col} END FROM cw WHERE id = ?1"
                ),
                [id],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap_or_else(|e| panic!("row {id} {col}: {e}"))
            .unwrap_or_else(|| "null".into())
        })
        .collect()
}

fn want() -> Vec<String> {
    TYPES.iter().map(|t| t.3.to_string()).collect()
}

/// The binary form of each input, as a driver sends it, and its type's OID; None
/// where the value goes as text.
fn binary(col: &str) -> Option<(Vec<u8>, u32)> {
    let int32 = |v: i32| v.to_be_bytes().to_vec();
    let uuid = hex_decode("a0eebc999c0b4ef8bb6d6bb9bd380a11");
    let array = |elem: u32, items: &[Vec<u8>]| {
        let mut b = Vec::new();
        b.extend(int32(1));
        b.extend(int32(0));
        b.extend(elem.to_be_bytes());
        b.extend(int32(items.len() as i32));
        b.extend(int32(1));
        for it in items {
            b.extend(int32(it.len() as i32));
            b.extend(it);
        }
        b
    };
    Some(match col {
        "b" => (hex_decode("deadbeef"), 17),
        "u" => (uuid, 2950),
        // 2024-01-02 is day 8767 after 2000-01-01.
        "d" => (int32(8767), 1082),
        "t" => (14_706_500_000i64.to_be_bytes().to_vec(), 1083),
        // The zone in seconds west of UTC.
        "tz" => ([14_706_500_000i64.to_be_bytes().to_vec(), int32(-7200)].concat(), 1266),
        "i" => (
            [7_200_000_000i64.to_be_bytes().to_vec(), int32(1), int32(0)].concat(),
            1186,
        ),
        "ia" => (array(23, &[int32(1), int32(2)]), 1007),
        "ta" => (array(25, &[b"a".to_vec(), b"b c".to_vec()]), 1009),
        "ua" => (array(2950, &[uuid]), 2951),
        "bo" => (vec![1], 16),
        _ => return None,
    })
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn text_oid(sql_type: &str) -> u32 {
    match sql_type {
        "bytea" => 17,
        "uuid" => 2950,
        "date" => 1082,
        "time" => 1083,
        "timetz" => 1266,
        "interval" => 1186,
        "int[]" => 1007,
        "text[]" => 1009,
        "uuid[]" => 2951,
        "boolean" => 16,
        _ => 0,
    }
}

fn create(conn: &Conn) {
    let defs: Vec<String> = TYPES
        .iter()
        .map(|(col, input, ty, _)| format!("{col} {ty} DEFAULT {}", quote(input)))
        .collect();
    must_exec(
        conn,
        &format!("CREATE TABLE cw (id int PRIMARY KEY DEFAULT 7, {})", defs.join(", ")),
    );
}

#[test]
fn every_path_stores_the_canonical_form() {
    let dir = temp_dir();
    let path = dir.path().join("test.db");
    let server = start_server_on(&path, 0);
    let conn = connect(&server);
    let ext = connect_extended(&server);
    create(&conn);
    let cols = columns();
    let lits = literals();
    let placeholders: Vec<String> = (2..=TYPES.len() + 1).map(|i| format!("${i}")).collect();
    let placeholders = placeholders.join(", ");
    let nulls = vec!["NULL"; TYPES.len()].join(", ");
    // A path whose statement fails is reported with the rest, not stopped at.
    let mut failed: Vec<String> = Vec::new();
    let mut paths: Vec<(i64, &str)> = Vec::new();
    let mut run = |id: i64, name: &'static str, r: Result<Tag>| match r {
        Ok(_) => paths.push((id, name)),
        Err(e) => failed.push(format!("{name}: {e}")),
    };
    let simple = |sql: &str| conn.exec(sql, &[]);

    run(
        1,
        "literal",
        simple(&format!("INSERT INTO cw (id, {cols}) VALUES (1, {lits})")),
    );

    // Text parameters, typed as a driver types them.
    let mut params: Vec<Vec<u8>> = vec![b"2".to_vec()];
    let mut oids = vec![23u32];
    for (_, input, ty, _) in TYPES {
        params.push(input.as_bytes().to_vec());
        oids.push(text_oid(ty));
    }
    let sql = format!("INSERT INTO cw (id, {cols}) VALUES ($1, {placeholders})");
    run(
        2,
        "text parameter",
        ext.exec_params(&sql, &params, &oids, &[], &[]).map(|r| r.tag),
    );

    // Binary parameters; an hstore, whose OID is the database's, goes as text.
    let mut params: Vec<Vec<u8>> = vec![3i32.to_be_bytes().to_vec()];
    let mut oids = vec![23u32];
    let mut formats = vec![1i16];
    for (col, input, _, _) in TYPES {
        if let Some((raw, oid)) = binary(col) {
            params.push(raw);
            oids.push(oid);
            formats.push(1);
        } else {
            params.push(input.as_bytes().to_vec());
            oids.push(0);
            formats.push(0);
        }
    }
    run(
        3,
        "binary parameter",
        ext.exec_params(&sql, &params, &oids, &formats, &[]).map(|r| r.tag),
    );

    let line: Vec<String> = TYPES.iter().map(|t| t.1.replace('\\', "\\\\")).collect();
    run(
        4,
        "COPY",
        conn.copy_from(
            format!("4\t{}\n", line.join("\t")).as_bytes(),
            &format!("COPY cw (id, {cols}) FROM STDIN"),
        ),
    );

    // The column defaults: omitted, named, and the whole row (id's default is 7).
    run(5, "DEFAULT omitted", simple("INSERT INTO cw (id) VALUES (5)"));
    let defaults = vec!["DEFAULT"; TYPES.len()].join(", ");
    run(6, "DEFAULT", simple(&format!("INSERT INTO cw VALUES (6, {defaults})")));
    run(7, "DEFAULT VALUES", simple("INSERT INTO cw DEFAULT VALUES"));

    run(
        8,
        "WITH ... INSERT",
        simple(&format!(
            "WITH n AS (SELECT 8 AS id) INSERT INTO cw (id, {cols}) SELECT n.id, {lits} FROM n"
        )),
    );

    must_exec(&conn, &format!("INSERT INTO cw (id, {cols}) VALUES (9, {nulls})"));
    run(
        9,
        "UPDATE SET (a, b)",
        simple(&format!("UPDATE cw SET ({cols}) = ({lits}) WHERE id = 9")),
    );

    run(
        10,
        "INSERT ... SELECT",
        simple(&format!("INSERT INTO cw (id, {cols}) SELECT 10, {lits}")),
    );

    must_exec(&conn, &format!("INSERT INTO cw (id, {cols}) VALUES (11, {nulls})"));
    let sets: Vec<String> = TYPES.iter().map(|t| format!("{} = {}", t.0, quote(t.1))).collect();
    run(
        11,
        "ON CONFLICT DO UPDATE",
        simple(&format!(
            "INSERT INTO cw (id) VALUES (11) ON CONFLICT (id) DO UPDATE SET {}",
            sets.join(", ")
        )),
    );
    must_exec(&conn, &format!("INSERT INTO cw (id, {cols}) VALUES (12, {nulls})"));
    let excluded: Vec<String> = TYPES.iter().map(|t| format!("{0} = EXCLUDED.{0}", t.0)).collect();
    run(
        12,
        "ON CONFLICT EXCLUDED",
        simple(&format!(
            "INSERT INTO cw (id, {cols}) VALUES (12, {lits}) ON CONFLICT (id) DO UPDATE SET {}",
            excluded.join(", ")
        )),
    );

    must_exec(&conn, &format!("INSERT INTO cw (id, {cols}) VALUES (13, {nulls})"));
    let sets: Vec<String> = TYPES
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} = ${}", t.0, i + 1))
        .collect();
    let params: Vec<&str> = TYPES.iter().map(|t| t.1).collect();
    let sql = format!("UPDATE cw SET {} WHERE id = 13", sets.join(", "));
    run(
        13,
        "UPDATE SET a = $1",
        ext.exec_params(&sql, &params, &[], &[], &[]).map(|r| r.tag),
    );

    let mut wrong = failed;
    for (id, name) in &paths {
        let got = stored(&path, *id);
        for ((col, ..), (g, w)) in TYPES.iter().zip(got.iter().zip(want())) {
            if *g != w {
                wrong.push(format!("{name} {col}: {g} (want {w})"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn what_a_type_does_not_read_is_refused() {
    let server = start_server();
    let conn = connect(&server);
    let ext = connect_extended(&server);
    must_exec(
        &conn,
        "CREATE TABLE bad (id int PRIMARY KEY, b bytea, u uuid, d date, t time, i interval, ia int[], ta text[], ua uuid[], h hstore)",
    );
    must_exec(&conn, "INSERT INTO bad (id) VALUES (1)");
    let cases = [
        ("u", "abc", "22P02", "invalid input syntax for type uuid: \"abc\""),
        ("d", "xyz", "22007", "invalid input syntax for type date: \"xyz\""),
        (
            "d",
            "2024-13-45",
            "22008",
            "date/time field value out of range: \"2024-13-45\"",
        ),
        ("t", "ab", "22007", "invalid input syntax for type time: \"ab\""),
        ("i", "abc", "22007", "invalid input syntax for type interval: \"abc\""),
        ("b", r"\xZZ", "22023", "invalid hexadecimal digit: \"Z\""),
        ("ia", "{1,", "22P02", "malformed array literal: \"{1,\""),
        ("ia", "{a}", "22P02", "invalid input syntax for type integer: \"a\""),
        ("ta", "abc", "22P02", "malformed array literal: \"abc\""),
        ("ua", "{abc}", "22P02", "invalid input syntax for type uuid: \"abc\""),
        ("h", "a=>", "42601", "syntax error in hstore: unexpected end of string"),
    ];
    for (col, input, code, message) in cases {
        let e = conn
            .exec(
                &format!("INSERT INTO bad (id, {col}) VALUES (2, {})", quote(input)),
                &[],
            )
            .expect_err(input);
        assert_pg_error(&e, code, message);
        let e = conn
            .exec(&format!("UPDATE bad SET {col} = {} WHERE id = 1", quote(input)), &[])
            .expect_err(input);
        assert_pg_error(&e, code, message);
        let e = ext
            .exec_params(
                format!("INSERT INTO bad (id, {col}) VALUES (3, $1)"),
                &[input],
                &[0],
                &[],
                &[],
            )
            .expect_err(input);
        assert_pg_error(&e, code, message);
    }
}
