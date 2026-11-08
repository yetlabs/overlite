use crate::common::*;

#[test]
fn declared_bytea_round_trip() {
    let conn = connect(&start_server());
    conn.exec("CREATE TABLE pictures (id integer, picture bytea)", &[])
        .unwrap();
    let want: Vec<u8> = vec![0, 0xff, 0x5c, 0x31];
    conn.exec("INSERT INTO pictures VALUES ($1, $2)", &[&1, &want]).unwrap();
    // Rows written as hex text (including before this fix) must read identically.
    conn.exec(r"INSERT INTO pictures VALUES (2, '\x00ff5c31')", &[])
        .unwrap();
    for format in [TEXT_FORMAT, BINARY_FORMAT] {
        let rows = conn
            .query_formats("SELECT picture FROM pictures ORDER BY id", &[format], &[])
            .unwrap();
        assert_eq!(rows.fields[0].data_type_oid, 17);
        let mut n = 0;
        for row in &rows {
            let value: Vec<u8> = row.scan().unwrap();
            assert_eq!(value, want);
            n += 1;
        }
        assert_eq!(n, 2);
    }
}

// A computed column holding bytes is a bytea, in Describe as in the values: read as
// int8, a binary client got the bytes parsed as a number.
#[test]
fn computed_blob_is_bytea() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, b bytea)");
    conn.exec("INSERT INTO t VALUES ($1, $2)", &[&1i64, &vec![0xdeu8, 0xad, 0xbe]])
        .unwrap();

    let sql = "SELECT substr(b, 1, 2) FROM t";
    let desc = conn.prepare("", sql).unwrap();
    assert_eq!(desc.fields[0].data_type_oid, 17, "Describe sees the first row");
    for format in [TEXT_FORMAT, BINARY_FORMAT] {
        let rows = conn.query_formats(sql, &[format], &[]).unwrap();
        assert_eq!(rows.fields[0].data_type_oid, 17);
        let got: Vec<u8> = rows.iter().next().unwrap().scan().unwrap();
        assert_eq!(got, [0xde, 0xad]);
    }
}
