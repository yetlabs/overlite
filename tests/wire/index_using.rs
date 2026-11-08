use crate::common::*;

// create_index_using: a CREATE INDEX with a "USING btree" clause (as pg_dump
// emits) runs, and pg_get_indexdef schema-qualifies the table for a schema
// index so the dump restores.
#[test]
fn create_index_using() {
    let conn = connect(&start_server());

    // USING btree accepted on a public table.
    conn.exec("CREATE TABLE pub (id int, v text)", &[]).unwrap();
    conn.exec("CREATE INDEX pub_v ON pub USING btree (v)", &[]).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM pg_class WHERE relname='pub_v'", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);

    // A schema index's def qualifies the table (pg_dump restorability).
    conn.exec("CREATE SCHEMA s", &[]).unwrap();
    conn.exec("CREATE TABLE s.t (id int)", &[]).unwrap();
    conn.exec("CREATE INDEX s_t_id ON s.t USING btree (id)", &[]).unwrap();

    let def: String = conn
        .query_row(
            "
		SELECT pg_get_indexdef(ci.oid)
		FROM pg_class ci JOIN pg_class ct ON true
		JOIN pg_index i ON i.indexrelid = ci.oid
		WHERE ci.relname = 's_t_id'",
            &[],
        )
        .scan()
        .unwrap();
    // The def qualifies the table (so a dump restores into the right schema) and
    // keeps USING btree (psql's \d reads it; the CREATE INDEX rewrite strips it
    // only when such a statement is actually executed, as tested above).
    assert_contains!(def, "ON s.t", "schema index def should qualify the table");
    assert_contains!(def, "USING btree");
}
