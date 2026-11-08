use crate::common::*;

#[test]
fn enum_type_lifecycle() {
    let conn = connect(&start_server());

    must_exec(&conn, "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')");

    // The type and its labels show up in pg_type / pg_enum, in order.
    assert_contains!(
        query_column(&conn, "SELECT typname FROM pg_catalog.pg_type WHERE typtype = 'e'", 0),
        "mood"
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT enumlabel FROM pg_catalog.pg_enum e
		 JOIN pg_catalog.pg_type t ON t.oid = e.enumtypid
		 WHERE t.typname = 'mood' ORDER BY e.enumsortorder",
            0
        ),
        ["sad", "ok", "happy"]
    );

    // A column of the enum type accepts a valid label and rejects an invalid one.
    must_exec(
        &conn,
        "CREATE TABLE person (name text, current_mood mood NOT NULL DEFAULT 'ok')",
    );
    must_exec(&conn, "INSERT INTO person (name, current_mood) VALUES ('ada', 'happy')");
    conn.exec("INSERT INTO person (name, current_mood) VALUES ('bob', 'furious')", &[])
        .expect_err("value outside the enum must be rejected by the CHECK");

    // The DEFAULT applies and the stored value is the plain label text.
    must_exec(&conn, "INSERT INTO person (name) VALUES ('cleo')");
    assert_eq!(
        query_column(&conn, "SELECT current_mood FROM person ORDER BY name", 0),
        ["happy", "ok"]
    );

    // ALTER TYPE ADD VALUE (append and positioned).
    must_exec(&conn, "ALTER TYPE mood ADD VALUE 'excited'");
    must_exec(&conn, "ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok'");
    assert_eq!(
        query_column(
            &conn,
            "SELECT enumlabel FROM pg_catalog.pg_enum e
		 JOIN pg_catalog.pg_type t ON t.oid = e.enumtypid
		 WHERE t.typname = 'mood' ORDER BY e.enumsortorder",
            0
        ),
        ["sad", "meh", "ok", "happy", "excited"]
    );

    // A table created after the ALTER accepts the new label.
    must_exec(&conn, "CREATE TABLE evt (m mood)");
    must_exec(&conn, "INSERT INTO evt VALUES ('excited')");

    // DROP TYPE removes it from the catalog.
    must_exec(&conn, "DROP TYPE mood");
    assert_not_contains!(
        query_column(&conn, "SELECT typname FROM pg_catalog.pg_type WHERE typtype = 'e'", 0),
        "mood"
    );

    // Internal enum tables stay hidden from the catalog.
    for rel in query_column(&conn, "SELECT relname FROM pg_catalog.pg_class WHERE relkind = 'r'", 0) {
        assert!(!rel.starts_with("_overlite"), "internal table {rel:?} leaked");
    }
}

// TestEnumExtendedProtocol exercises the Parse/Bind/Execute path (JDBC/DBeaver).
#[test]
fn enum_extended_protocol() {
    let conn = connect_extended(&start_server());

    must_exec(&conn, "CREATE TYPE color AS ENUM ('red', 'green', 'blue')");
    must_exec(&conn, "CREATE TABLE paint (c color)");
    conn.exec("INSERT INTO paint (c) VALUES ($1)", &[&"green"]).unwrap();
    conn.exec("INSERT INTO paint (c) VALUES ($1)", &[&"purple"])
        .expect_err("invalid enum label must be rejected");

    assert_eq!(query_column(&conn, "SELECT c FROM paint", 0), ["green"]);
}

#[test]
fn non_enum_create_type_is_accepted() {
    let conn = connect(&start_server());
    // Composite/other CREATE TYPE forms are accepted as a no-op (not modeled).
    must_exec(&conn, "CREATE TYPE addr AS (street text, zip text)");
}
