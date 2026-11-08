use crate::common::*;

// node-postgres sends every parameter as text with no type, and binds before
// it describes, so a JS true reached a boolean column as the string "true":
// stored as text, `WHERE flag` and `flag = true` never found it again.
#[test]
fn boolean_text_parameters() {
    let conn = connect_extended(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE flags (id text primary key, on_ boolean NOT NULL DEFAULT false, note text)",
    );

    // Parse with no types, Bind in text, no Describe of the statement first:
    // what node-postgres does.
    let insert = |id: &str, on: &str, note: &str| {
        conn.exec_params(
            "INSERT INTO flags (id, on_, note) VALUES ($1, $2, $3)
			 ON CONFLICT (id) DO UPDATE SET on_ = excluded.on_, note = excluded.note",
            &[id, on, note],
            &[],
            &[],
            &[],
        )
        .unwrap();
    };
    for (id, on) in [("a", "true"), ("b", "t"), ("c", "yes"), ("d", " ON "), ("e", "1")] {
        insert(id, on, "true");
    }
    for (id, off) in [("f", "false"), ("g", "f"), ("h", "no"), ("i", "off"), ("j", "0")] {
        insert(id, off, "false");
    }

    let on: i64 = conn
        .query_row("SELECT count(*) FROM flags WHERE on_", &[])
        .scan()
        .unwrap();
    let off: i64 = conn
        .query_row("SELECT count(*) FROM flags WHERE NOT on_", &[])
        .scan()
        .unwrap();
    assert_eq!(on, 5, "every Postgres spelling of true");
    assert_eq!(off, 5, "every Postgres spelling of false");

    let stored: String = conn
        .query_row("SELECT typeof(on_) FROM flags WHERE id = 'a'", &[])
        .scan()
        .unwrap();
    assert_eq!(stored, "integer", "stored as SQLite's 1, not the text");

    // A text column keeps what it was given.
    let note: String = conn
        .query_row("SELECT note FROM flags WHERE id = 'a'", &[])
        .scan()
        .unwrap();
    assert_eq!(note, "true");

    // With no column list the values fill the columns in order.
    conn.exec_params(
        "INSERT INTO flags VALUES ($1, $2, $3)",
        &["k", "true", "true"],
        &[],
        &[],
        &[],
    )
    .unwrap();
    let (k, k_note): (bool, String) = conn
        .query_row("SELECT on_, note FROM flags WHERE id = 'k'", &[])
        .scan()
        .unwrap();
    assert!(k, "positional insert");
    assert_eq!(k_note, "true", "a text column in a positional insert");
    must_exec(&conn, "DELETE FROM flags WHERE id = 'k'");

    // And a text parameter compared with a boolean column finds the rows.
    let res = conn
        .exec_params(
            "SELECT id FROM flags WHERE on_ = $1 ORDER BY id",
            &["true"],
            &[],
            &[],
            &[],
        )
        .unwrap();
    assert_eq!(res.rows.len(), 5);
}
