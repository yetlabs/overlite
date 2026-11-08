use crate::common::*;

// A boolean bound as a parameter is one. Parameters were described with no
// type, so pgx sent a bool as the text "t", stored as that string; read
// back in binary it became false — a flag turned on, through GORM, stayed
// off. Parameters set into a column now take its type, and a stored "t"
// reads as true either way.
#[test]
fn boolean_parameters() {
    let conn = connect_extended(&start_server());

    must_exec(
        &conn,
        "CREATE TABLE flags (id text primary key, on_ boolean NOT NULL DEFAULT false, name text)",
    );
    conn.exec(
        r#"INSERT INTO "flags" ("id","on_","name") VALUES ($1,$2,$3),($4,$5,$6)"#,
        &[&"a", &true, &"x", &"b", &false, &"t"],
    )
    .unwrap();
    conn.exec(
        r#"UPDATE "flags" SET "on_"=$1,"name"=$2 WHERE "id" = $3"#,
        &[&true, &"y", &"b"],
    )
    .unwrap();

    let a: bool = conn
        .query_row("SELECT on_ FROM flags WHERE id = 'a'", &[])
        .scan()
        .unwrap();
    let b: bool = conn
        .query_row("SELECT on_ FROM flags WHERE id = 'b'", &[])
        .scan()
        .unwrap();
    assert!(a, "inserted as true");
    assert!(b, "updated to true");

    // Stored as SQLite's 1, so a literal finds them; a text column keeps its "t".
    let n: i64 = conn
        .query_row("SELECT count(*) FROM flags WHERE on_ = true", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 2);
    let name: String = conn
        .query_row("SELECT name FROM flags WHERE id = 'a'", &[])
        .scan()
        .unwrap();
    assert_eq!(name, "x");

    // What older versions stored, "t" as text, reads as true in binary too.
    must_exec(&conn, "INSERT INTO flags (id, on_) VALUES ('c', 't')");
    let c: bool = conn
        .query_row("SELECT on_ FROM flags WHERE id = 'c'", &[])
        .scan()
        .unwrap();
    assert!(c, "a stored 't'");
}
