use crate::common::*;

// A SERIAL PRIMARY KEY column must auto-increment like Postgres: inserting rows
// without the id assigns 1, 2, 3, ... and RETURNING gives the generated id.
#[test]
fn serial_auto_increment() {
    let conn = connect(&start_server());

    conn.exec("CREATE TABLE users (id SERIAL PRIMARY KEY, nome TEXT NOT NULL)", &[])
        .unwrap();

    conn.exec("INSERT INTO users (nome) VALUES ('ana')", &[]).unwrap();
    conn.exec("INSERT INTO users (nome) VALUES ('bob')", &[]).unwrap();

    let rows = conn.query("SELECT id, nome FROM users ORDER BY id", &[]).unwrap();
    let got = rows.collect_rows::<(i64, String)>().unwrap();
    assert_eq!(got, [(1, "ana".to_string()), (2, "bob".to_string())]);

    // RETURNING surfaces the generated id.
    let id: i64 = conn
        .query_row("INSERT INTO users (nome) VALUES ('carol') RETURNING id", &[])
        .scan()
        .unwrap();
    assert_eq!(id, 3);

    // The catalog reports the column as integer.
    let typ: String = conn
        .query_row(
            "
		SELECT format_type(a.atttypid, a.atttypmod)
		FROM pg_catalog.pg_attribute a
		JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
		WHERE c.relname = 'users' AND a.attname = 'id'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(typ, "integer");
}
