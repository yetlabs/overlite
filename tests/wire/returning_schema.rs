use crate::common::*;

// RETURNING against a schema-qualified table, through the extended protocol
// (which is where the statement gets described, and where the table name was
// mis-parsed). Every ORM that quotes identifiers and reads a generated key back
// lands here.

#[test]
fn returning_on_qualified_table() {
    let conn = connect_extended(&start_server());

    must_exec(&conn, "CREATE SCHEMA sales");
    must_exec(
        &conn,
        r#"CREATE TABLE "sales"."orders" (id serial primary key, total int)"#,
    );

    let id: i64 = conn
        .query_row(
            r#"INSERT INTO "sales"."orders" (total) VALUES ($1) RETURNING id"#,
            &[&10i64],
        )
        .scan()
        .unwrap();
    assert!(id > 0, "{id} is not positive");

    let total: i64 = conn
        .query_row(
            r#"UPDATE "sales"."orders" SET total = total + $1 WHERE id = $2 RETURNING total"#,
            &[&5i64, &id],
        )
        .scan()
        .unwrap();
    assert_eq!(total, 15);

    let deleted: i64 = conn
        .query_row(r#"DELETE FROM "sales"."orders" WHERE id = $1 RETURNING id"#, &[&id])
        .scan()
        .unwrap();
    assert_eq!(deleted, id);
}

// is_query and introspection_sql: RETURNING in a literal does not make a statement
// return rows.
#[test]
fn returning_not_in_a_literal() {
    for conn in [connect(&start_server()), connect_extended(&start_server())] {
        must_exec(&conn, "CREATE TABLE t (a text)");
        let tag = conn.exec("INSERT INTO t VALUES (' RETURNING a')", &[]).unwrap();
        assert_eq!(tag.as_str(), "INSERT 0 1");
        let r = conn
            .exec_params::<&[u8]>("UPDATE t SET a = ' RETURNING b'", &[], &[], &[], &[])
            .unwrap();
        assert_eq!(r.tag.as_str(), "UPDATE 1");
        assert!(r.fields.is_empty(), "{:?}", r.fields);
        let r = conn
            .exec_params::<&[u8]>("DELETE FROM t WHERE a = ' RETURNING b'", &[], &[], &[], &[])
            .unwrap();
        assert_eq!(r.tag.as_str(), "DELETE 1");

        // A real one still does, with the word in a literal of its own, on its own line.
        let s: String = conn
            .query_row("INSERT INTO t VALUES ('x')\nRETURNING ' RETURNING ' || a", &[])
            .scan()
            .unwrap();
        assert_eq!(s, " RETURNING x");
    }
}
