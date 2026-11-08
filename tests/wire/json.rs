use crate::common::*;

// json/jsonb columns, the -> / ->> / #> operators, and the builder/aggregate
// functions, end to end.
#[test]
fn json_basics() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE TABLE docs (id SERIAL PRIMARY KEY, data JSONB)");
    must_exec(
        &conn,
        r#"INSERT INTO docs (data) VALUES ('{"nome":"ana","tags":["x","y"],"end":{"cidade":"SP"}}')"#,
    );

    // ->> returns text; -> returns JSON (quoted).
    let (nome, nome_json): (String, String) = conn
        .query_row("SELECT data ->> 'nome', data -> 'nome' FROM docs", &[])
        .scan()
        .unwrap();
    assert_eq!(nome, "ana");
    assert_eq!(nome_json, r#""ana""#);

    // Array element and nested path via #>>.
    let (tag0, cidade): (String, String) = conn
        .query_row("SELECT data -> 'tags' ->> 0, data #>> '{end,cidade}' FROM docs", &[])
        .scan()
        .unwrap();
    assert_eq!(tag0, "x");
    assert_eq!(cidade, "SP");

    // Filter using a JSON extraction.
    let count: i64 = conn
        .query_row("SELECT count(*) FROM docs WHERE data ->> 'nome' = 'ana'", &[])
        .scan()
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn json_builders() {
    let conn = connect(&start_server());

    let (obj, arr): (String, String) = conn
        .query_row(
            "SELECT jsonb_build_object('a', 1, 'b', 'x'), json_build_array(1, 2, 3)",
            &[],
        )
        .scan()
        .unwrap();
    assert_json_eq(r#"{"a":1,"b":"x"}"#, &obj);
    assert_eq!(arr, "[1,2,3]");

    must_exec(&conn, "CREATE TABLE nums (n INTEGER)");
    must_exec(&conn, "INSERT INTO nums VALUES (1), (2), (3)");
    let agg: String = conn.query_row("SELECT jsonb_agg(n) FROM nums", &[]).scan().unwrap();
    assert_eq!(agg, "[1,2,3]");
}

#[test]
fn json_catalog_type() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE docs (id INTEGER, data JSONB, meta JSON)");

    let type_of = |col: &str| -> String {
        conn.query_row(
            "
			SELECT format_type(a.atttypid, a.atttypmod)
			FROM pg_catalog.pg_attribute a
			JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
			WHERE c.relname = 'docs' AND a.attname = $1",
            &[&col],
        )
        .scan()
        .unwrap()
    };
    assert_eq!(type_of("data"), "jsonb");
    assert_eq!(type_of("meta"), "json");
}
