use crate::common::*;

// 'true'::boolean was CAST('true' AS boolean): NUMERIC affinity, so any text
// became 0 and a true cast to false without an error. (A cast's result is
// still described by its value, SQLite's 1 or 0, not as bool.)
#[test]
fn boolean_cast() {
    let conn = connect_extended(&start_server());

    for (input, want) in [
        ("'true'::boolean", 1),
        ("'t'::bool", 1),
        ("'yes'::boolean", 1),
        ("'on'::boolean", 1),
        ("'1'::boolean", 1),
        ("1::boolean", 1),
        ("'false'::boolean", 0),
        ("'f'::bool", 0),
        ("'off'::boolean", 0),
        ("0::boolean", 0),
        ("CAST('true' AS boolean)", 1),
        ("CAST(' Yes ' AS pg_catalog.bool)", 1),
    ] {
        let sql = format!("SELECT {input}");
        let got: bool = conn.query_row(&sql, &[]).scan().expect(input);
        assert_eq!(got, want != 0, "{input}");
    }

    let null: Option<bool> = conn.query_row("SELECT NULL::boolean", &[]).scan().unwrap();
    assert!(null.is_none());

    let err = conn
        .exec("SELECT 'maybe'::boolean", &[])
        .expect_err("Postgres rejects what is not a boolean");
    assert_contains!(err, r#"invalid input syntax for type boolean: "maybe""#);

    // A bound parameter cast to boolean, as `$1::boolean` with "true".
    must_exec(&conn, "CREATE TABLE b (k int, v boolean)");
    conn.exec_params("INSERT INTO b VALUES (1, $1::boolean)", &["true"], &[], &[], &[])
        .unwrap();
    let v: bool = conn.query_row("SELECT v FROM b WHERE k = 1", &[]).scan().unwrap();
    assert!(v);
}

// xorm reads `COALESCE(bool_or(x = 1), false)` as a boolean: it came back int8 1. An
// expression whose value is a boolean is one on the wire, as in Postgres 17, however
// it is wrapped.
#[test]
fn boolean_expressions_go_out_as_booleans() {
    let server = start_server();
    let sql = "SELECT COALESCE(bool_or(x = 1), false) AS a, bool_or(x = 9) AS b, \
               CASE WHEN min(x) = 1 THEN true ELSE false END AS c, NULLIF(max(x) > 1, false) AS d, \
               GREATEST(min(x) = 1, false) AS e, LEAST(max(x), 7) AS f, GREATEST(1, NULL, 3) AS g, \
               count(*) = 2 AS h, true AS i, coalesce(max(x), 0) AS j FROM (VALUES (1), (2)) v(x)";
    for conn in [connect(&server), connect_extended(&server)] {
        let r = conn.exec_params(sql, &[] as &[&str], &[], &[], &[]).unwrap();
        let oids: Vec<u32> = r.fields.iter().map(|f| f.data_type_oid).collect();
        assert_eq!(oids[..5], [16, 16, 16, 16, 16], "{oids:?}");
        assert_eq!(oids[7..9], [16, 16], "{oids:?}");
        let row: Vec<String> = (0..r.fields.len()).map(|j| r.text(0, j)).collect();
        assert_eq!(row, ["t", "f", "t", "t", "t", "2", "3", "t", "t", "2"]);
    }
}
