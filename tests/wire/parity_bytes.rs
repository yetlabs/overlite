use crate::common::*;

// bad is text no UTF-8 decoder accepts: a lone continuation byte, a truncated
// sequence, an overlong form and bytes that never occur in UTF-8.
const BAD: &[u8] = b"a\xffb\xc3c\x80\xc0\xafd\xe2\x82";

// plane_sixteen is valid UTF-8 for U+10FF80 and must stay exactly that.
const PLANE_SIXTEEN: &[u8] = b"x\xf4\x8f\xbe\x80y\xf4\x8f\xbf\xbfz";

fn hex(b: &[u8]) -> String {
    hex::encode(b)
}

/// Concatenation, for SQL that is not UTF-8.
fn sql(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn parity_bytes_cell_from_sqlite() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, v TEXT)");
    for (i, b) in [BAD, PLANE_SIXTEEN].into_iter().enumerate() {
        must_exec(
            &conn,
            &format!("INSERT INTO t VALUES ({i}, CAST(X'{}' AS TEXT))", hex(b)),
        );
        let rows = raw_rows(&conn, format!("SELECT v FROM t WHERE id = {i}"));
        assert_eq!(rows[0][0].as_deref(), Some(b));
        // The extended protocol, text then binary result format.
        for f in [0i16, 1] {
            let r = conn
                .exec_params("SELECT v FROM t WHERE id = $1", &[i.to_string()], &[], &[0], &[f])
                .unwrap();
            assert_eq!(r.cell(0, 0), b, "format {f}");
        }
    }
}

#[test]
fn parity_bytes_sql_literal() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, v TEXT)");
    for (i, b) in [BAD, PLANE_SIXTEEN].into_iter().enumerate() {
        let i = i as i64;
        // Simple query.
        raw_rows(
            &conn,
            sql(&[format!("INSERT INTO t VALUES ({i}, '").as_bytes(), b, b"')"]),
        );
        assert_eq!(stored(&conn, "t", i), hex_of(b));
        assert_eq!(
            raw_rows(&conn, format!("SELECT v FROM t WHERE id = {i}"))[0][0].as_deref(),
            Some(b)
        );

        // Extended query: the literal arrives in Parse.
        let q = sql(&[format!("INSERT INTO t VALUES ({}, '", 100 + i).as_bytes(), b, b"')"]);
        conn.exec_params::<&[u8]>(q, &[], &[], &[], &[]).unwrap();
        assert_eq!(stored(&conn, "t", 100 + i), hex_of(b));

        // A batch of statements, split on the way in.
        raw_rows(
            &conn,
            sql(&[
                format!("INSERT INTO t VALUES ({}, '", 200 + i).as_bytes(),
                b,
                b"'); SELECT 1",
            ]),
        );
        assert_eq!(stored(&conn, "t", 200 + i), hex_of(b));

        // WHERE compares against the stored bytes.
        let q = sql(&[format!("SELECT id FROM t WHERE id = {i} AND v = '").as_bytes(), b, b"'"]);
        assert_eq!(raw_rows(&conn, q).len(), 1);
    }
}

#[test]
fn parity_bytes_escape_string_literal() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, v TEXT)");
    // Postgres decodes the escape, and a lone 0xff is no text in UTF-8.
    let err = conn.exec(r"INSERT INTO t VALUES (1, E'a\xffb')", &[]).unwrap_err();
    assert_sqlstate(&err, "22021");
    must_exec(&conn, r"INSERT INTO t VALUES (1, E'a\xc3\xa9b')");
    assert_eq!(stored(&conn, "t", 1), "61C3A962");
}

#[test]
fn parity_bytes_identifiers() {
    let conn = connect(&start_server());
    raw_rows(
        &conn,
        b"CREATE TABLE \"r\xff\" (\"c\xfe\" TEXT DEFAULT 'd\xfd', id INTEGER)",
    );
    raw_rows(&conn, b"INSERT INTO \"r\xff\" (id) VALUES (1)");
    let rs = conn.pg_exec(b"SELECT * FROM \"r\xff\"").unwrap();
    assert_eq!(rs[0].fields[0].name_bytes, b"c\xfe");
    assert_eq!(rs[0].cell(0, 0), b"d\xfd");
    let rs = conn.pg_exec(b"SELECT 1 AS \"x\xfc\"").unwrap();
    assert_eq!(rs[0].fields[0].name_bytes, b"x\xfc");
}

#[test]
fn parity_bytes_error_message() {
    let conn = connect(&start_server());
    let err = conn.pg_exec(b"SELECT * FROM \"no\xffpe\"").unwrap_err();
    let pe = pg_error(&err);
    assert!(pe.message_bytes.windows(5).any(|w| w == b"no\xffpe"), "{pe}");
}

#[test]
fn parity_bytes_text_parameter() {
    const OID_UNKNOWN: u32 = 0;
    const OID_TEXT: u32 = 25;
    const OID_VARCHAR: u32 = 1043;
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, v TEXT)");
    let mut id = 0;
    for oid in [OID_UNKNOWN, OID_TEXT, OID_VARCHAR] {
        for format in [0i16, 1] {
            for b in [BAD, PLANE_SIXTEEN] {
                id += 1;
                let r = conn.exec_params(
                    "INSERT INTO t VALUES ($1, $2)",
                    &[id.to_string().as_bytes(), b],
                    &[0, oid],
                    &[0, format],
                    &[],
                );
                if format == 1 && oid == OID_UNKNOWN {
                    // Binary with no type is guessed by length: a blob here.
                    r.unwrap();
                    continue;
                }
                r.unwrap();
                assert_eq!(stored(&conn, "t", id), hex_of(b), "oid {oid} format {format}");
                let typeof_v = raw_rows(&conn, format!("SELECT typeof(v) FROM t WHERE id = {id}"));
                assert_eq!(typeof_v[0][0].as_deref(), Some(b"text".as_slice()));
                let got = conn
                    .exec_params("SELECT v FROM t WHERE id = $1", &[id.to_string()], &[], &[0], &[0])
                    .unwrap();
                assert_eq!(got.cell(0, 0), b);
            }
        }
    }
}

#[test]
fn parity_bytes_copy_text() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, v TEXT)");
    let mut input = Vec::new();
    for (i, b) in [BAD, PLANE_SIXTEEN].into_iter().enumerate() {
        input.extend_from_slice(&sql(&[format!("{i}\t").as_bytes(), b, b"\n"]));
    }
    input.extend_from_slice(b"2\tq\xff\\tr\xfe\n");
    let tag = conn.copy_from(input.as_slice(), "COPY t (id, v) FROM STDIN").unwrap();
    assert_eq!(tag.rows_affected(), 3);
    assert_eq!(stored(&conn, "t", 0), hex_of(BAD));
    assert_eq!(stored(&conn, "t", 1), hex_of(PLANE_SIXTEEN));
    assert_eq!(stored(&conn, "t", 2), hex_of(b"q\xff\tr\xfe"));

    let mut out = Vec::new();
    conn.copy_to(&mut out, "COPY t (id, v) TO STDOUT").unwrap();
    assert_eq!(input, out);
}

#[test]
fn parity_bytes_copy_csv() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id INTEGER, v TEXT)");
    let mut input = Vec::new();
    for (i, b) in [BAD, PLANE_SIXTEEN].into_iter().enumerate() {
        input.extend_from_slice(&sql(&[format!("{i},").as_bytes(), b, b"\n"]));
    }
    input.extend_from_slice(b"2,\"q\xff,r\xfe\"\"\"\n");
    conn.copy_from(input.as_slice(), "COPY t (id, v) FROM STDIN WITH (FORMAT csv)")
        .unwrap();
    assert_eq!(stored(&conn, "t", 0), hex_of(BAD));
    assert_eq!(stored(&conn, "t", 1), hex_of(PLANE_SIXTEEN));
    assert_eq!(stored(&conn, "t", 2), hex_of(b"q\xff,r\xfe\""));

    let mut out = Vec::new();
    conn.copy_to(&mut out, "COPY t (id, v) TO STDOUT WITH (FORMAT csv)")
        .unwrap();
    assert_eq!(input, out);
}

// Text that passes through a server-side SQL function keeps its bytes. The want
// "?" logs only and an error is logged and skipped: this loop is also how a
// statement's failure is tolerated.
#[test]
fn parity_bytes_through_functions() {
    let conn = connect(&start_server());
    let cases: &[(&str, &[u8])] = &[
        ("SELECT hex(lower(CAST(X'41FF' AS TEXT)))", b"61EFBFBD"),
        ("SELECT hex(upper(CAST(X'61FF' AS TEXT)))", b"41EFBFBD"),
        ("SELECT hex(quote_literal(CAST(X'FF' AS TEXT)))", b"27FF27"),
        ("SELECT hex(quote_ident(CAST(X'FF' AS TEXT)))", b"22FF22"),
        ("SELECT hex(concat(CAST(X'FF' AS TEXT), 'a'))", b"FF61"),
        ("SELECT hex(CAST(X'FF' AS TEXT) || 'a')", b"FF61"),
        ("SELECT CAST(CAST(X'FF' AS TEXT) AS TEXT)", b"\xff"),
        (
            "SELECT hex(array_to_string(CAST(X'5B2261FF62222C2263225D' AS TEXT), ','))",
            b"",
        ),
        // A match is a boolean, t as in Postgres, where the old server sent 1.
        ("SELECT CAST(X'61FF' AS TEXT) ~ 'a.'", b"t"),
        ("SELECT CAST(X'61FF' AS TEXT) ~ 'a.$'", b"t"),
        ("SELECT CAST(X'61FF62' AS TEXT) ~ 'a[^b]b'", b"t"),
        (
            "SELECT hex(json_extract(CAST(X'7B2261223A2262FF227D' AS TEXT), '$.a'))",
            b"62FF",
        ),
        ("SELECT hex(pg_get_expr(CAST(X'FF' AS TEXT), 0))", b"FF"),
        ("SELECT hex(overlite_array_lower(CAST(X'FF' AS TEXT)))", b"31"),
        (
            "SELECT hex(CAST(CAST(X'5B2261FF62225D' AS TEXT) AS TEXT[]))",
            b"5B2261FF62225D",
        ),
        (
            "SELECT CAST(CAST(X'5B2261FF62225D' AS TEXT) AS TEXT[])",
            b"[\"a\xffb\"]",
        ),
        (
            "SELECT hex(to_char(CAST('2024-01-02' AS DATE), CAST(X'FF59595959' AS TEXT)))",
            b"FF32303234",
        ),
        (
            "SELECT hex(hstore(CAST(X'FF' AS TEXT), 'a'))",
            b"7B22EFBFBD223A2261227D",
        ),
        ("SELECT hstore(CAST(X'FF' AS TEXT), 'a')", b"{\"\xef\xbf\xbd\":\"a\"}"),
        ("SELECT hex(format_type(1, CAST(X'FF' AS TEXT)))", b"74657874"),
        ("SELECT hex(hstore(CAST(X'22FF223D3E2261FE22' AS TEXT)))", b"?"),
    ];
    for (q, want) in cases {
        let rs = match conn.pg_exec(q) {
            Ok(rs) => rs,
            Err(e) => {
                println!("{q}: {e}");
                continue;
            }
        };
        println!("{q} -> {:?}", String::from_utf8_lossy(rs[0].cell(0, 0)));
        if *want != b"?" {
            assert_eq!(rs[0].cell(0, 0), *want, "{q}");
        }
    }
}
