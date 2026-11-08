use crate::common::*;
use std::path::Path;

/// A file as an older server left it, written with SQLite alone: booleans as the text a
/// client sent, a timestamp(3) with a trailing zero, a timestamptz with an offset and a
/// T, an upper-case uuid in braces, an array in Postgres' text form, bytea as escaped
/// text, a numeric(10,2) unrounded. `Flag.on` holds a value no boolean reads as.
fn old_format_db(path: &Path) {
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch(
        r#"
        CREATE TABLE "Answer" ("id" INTEGER PRIMARY KEY, "ok" BOOLEAN NOT NULL, "at" TIMESTAMP(3),
            "ts" TIMESTAMPTZ, "u" UUID, "tags" text[], "raw" BYTEA, "amount" DECIMALTEXT(10,2), "note" TEXT);
        CREATE INDEX "Answer_ok_idx" ON "Answer"("ok");
        CREATE TABLE "log" ("n" INTEGER);
        CREATE TRIGGER "answer_touched" AFTER UPDATE ON "Answer" BEGIN INSERT INTO "log" VALUES (1); END;
        INSERT INTO "Answer" VALUES
            (1, 'true', '2026-08-09 05:21:01.750', '2024-01-02T03:04:05+02:00',
             '{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}', '{a,"b c"}', '\xdeadbeef', '1.555', 'true'),
            (2, 'false', '2026-08-09 05:21:02', '2024-01-02 03:04:05', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a12',
             '["x"]', X'00', '2.00', 'f'),
            (3, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
            (4, 'f', NULL, NULL, NULL, NULL, NULL, NULL, NULL);
        CREATE TABLE "Flag" ("id" INTEGER PRIMARY KEY, "on" BOOLEAN);
        CREATE INDEX "Flag_on_idx" ON "Flag"("on");
        INSERT INTO "Flag" VALUES (1, 'yes'), (2, 'maybe'), (3, 0);
        "#,
    )
    .unwrap();
}

fn one(conn: &Conn, sql: &str) -> String {
    let r = conn
        .exec_params(sql, &[] as &[&str], &[], &[], &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    (0..r.rows.len())
        .map(|i| (0..r.fields.len()).map(|j| r.text(i, j)).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join(",")
}

/// What SQLite holds, read with SQLite alone.
fn stored(path: &Path, sql: &str) -> String {
    let db = rusqlite::Connection::open(path).unwrap();
    db.query_row(sql, [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

// The first open of a file from before the storage format backs it up, converts every
// value of a known type to its canonical form in one transaction, and marks the file.
// A value its type cannot read stays, its column listed mixed and still read both ways.
#[test]
fn an_old_file_is_converted_once() {
    let dir = temp_dir();
    let path = dir.path().join("test.db");
    old_format_db(&path);
    let backup = dir.path().join("test.db.pre-v1.bak");

    let server = start_server_on(&path, 0);
    let conn = connect_extended(&server);
    // The backup holds the file as it was.
    assert!(backup.is_file());
    assert_eq!(
        stored(
            &backup,
            r#"SELECT group_concat(k, ',') FROM (SELECT typeof("ok") || ':' || "ok" AS k FROM "Answer" ORDER BY id)"#
        ),
        "text:true,text:false,integer:1,text:f"
    );
    // Converted, as the server reads it and as SQLite holds it.
    assert_eq!(one(&conn, r#"SELECT id FROM "Answer" WHERE "ok" ORDER BY id"#), "1,3");
    assert_eq!(one(&conn, r#"SELECT version FROM _overlite_storage_format"#), "1");
    assert_eq!(
        one(&conn, r#"SELECT typeof("ok") || ':' || "ok" FROM "Answer" ORDER BY id"#),
        "integer:1,integer:0,integer:1,integer:0"
    );
    assert_eq!(
        stored(
            &path,
            r#"SELECT "at" || '|' || "ts" || '|' || "u" || '|' || "tags" || '|' || typeof("raw") || '|' || "amount" || '|' || "note" FROM "Answer" WHERE id = 1"#
        ),
        r#"2026-08-09 05:21:01.75|2024-01-02 01:04:05|a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11|["a","b c"]|blob|1.56|true"#
    );
    // No trigger fired, and each is still there.
    assert_eq!(one(&conn, r#"SELECT count(*) FROM "log""#), "0");
    assert_eq!(
        one(&conn, "SELECT count(*) FROM sqlite_master WHERE type = 'trigger'"),
        "1"
    );
    // A boolean column in the format is compared plainly, by its index.
    let plan = conn
        .exec_params(
            r#"EXPLAIN QUERY PLAN SELECT id FROM "Answer" WHERE "ok" = $1"#,
            &["true"],
            &[],
            &[],
            &[],
        )
        .unwrap();
    let detail: Vec<String> = (0..plan.rows.len()).map(|i| plan.text(i, 3)).collect();
    assert!(
        detail.iter().any(|d| d.contains(r#"INDEX Answer_ok_idx (ok=?)"#)),
        "{detail:?}"
    );
    let r = conn
        .exec_params(
            r#"SELECT id FROM "Answer" WHERE "ok" = $1 ORDER BY id"#,
            &["false"],
            &[],
            &[],
            &[],
        )
        .unwrap();
    assert_eq!((0..r.rows.len()).map(|i| r.text(i, 0)).collect::<Vec<_>>(), ["2", "4"]);
    // The mixed column: its bad value stays, listed with its count, and the rest reads.
    assert_eq!(
        one(
            &conn,
            "SELECT tablename || '.' || colname || ':' || invalid FROM _overlite_mixed_columns"
        ),
        "Flag.on:1"
    );
    assert_eq!(
        stored(
            &path,
            r#"SELECT group_concat(k, ',') FROM (SELECT typeof("on") || ':' || "on" AS k FROM "Flag" ORDER BY id)"#
        ),
        "integer:1,text:maybe,integer:0"
    );
    assert_eq!(one(&conn, r#"SELECT id FROM "Flag" WHERE "on" = true"#), "1");
    assert_eq!(one(&conn, r#"SELECT id FROM "Flag" WHERE NOT "on""#), "3");
    drop(conn);
    drop(server);

    // A second open converts nothing: no backup is taken again.
    std::fs::remove_file(&backup).unwrap();
    let server = start_server_on(&path, 0);
    let conn = connect(&server);
    assert_eq!(one(&conn, r#"SELECT id FROM "Answer" WHERE "ok" ORDER BY id"#), "1,3");
    drop(conn);
    drop(server);
    assert!(!backup.exists(), "a file in the format is not backed up again");

    // An operator fixes the bad value: the next open converts the column, which is no
    // longer mixed.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(r#"UPDATE "Flag" SET "on" = 'no' WHERE id = 2"#)
        .unwrap();
    drop(db);
    let server = start_server_on(&path, 0);
    let conn = connect(&server);
    assert_eq!(one(&conn, "SELECT count(*) FROM _overlite_mixed_columns"), "0");
    assert_eq!(
        one(&conn, r#"SELECT typeof("on") || ':' || "on" FROM "Flag" ORDER BY id"#),
        "integer:1,integer:0,integer:0"
    );
}

// A file whose backup cannot be written is not converted, and the server does not
// start: here a directory stands where the backup is written first.
#[test]
fn no_backup_no_start() {
    let dir = temp_dir();
    let path = dir.path().join("test.db");
    old_format_db(&path);
    std::fs::create_dir(dir.path().join("test.db.pre-v1.bak.tmp")).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_overlite"))
        .args(["--host", "127.0.0.1", "--port", "0", "--db"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot write the backup"), "{err}");
    assert_eq!(
        stored(
            &path,
            r#"SELECT group_concat(k, ',') FROM (SELECT typeof("ok") AS k FROM "Answer" ORDER BY id)"#
        ),
        "text,text,integer,text"
    );
}

// A new database starts in the format, and a boolean written in any spelling is
// stored as 1 or 0: a quoted literal, an untyped parameter, COPY.
#[test]
fn new_writes_are_canonical() {
    let server = start_server();
    let conn = connect(&server);
    must_exec(&conn, "CREATE TABLE b (id int, v boolean)");
    must_exec(&conn, "INSERT INTO b VALUES (1, 't'), (2, 'no'), (3, true)");
    conn.copy_from("4\ttrue\n5\tf\n".as_bytes(), "COPY b FROM STDIN")
        .unwrap();
    let ext = connect_extended(&server);
    ext.exec_params("INSERT INTO b VALUES (6, $1)", &["yes"], &[], &[], &[])
        .unwrap();
    ext.exec_params("UPDATE b SET v = $1 WHERE id = 3", &["off"], &[], &[], &[])
        .unwrap();
    assert_eq!(one(&conn, "SELECT version FROM _overlite_storage_format"), "1");
    assert_eq!(
        one(&conn, "SELECT typeof(v) || ':' || v FROM b ORDER BY id"),
        "integer:1,integer:0,integer:0,integer:1,integer:0,integer:1"
    );
    let err = conn.query("INSERT INTO b VALUES (7, 'maybe')", &[]).unwrap_err();
    assert_sqlstate(&err, "22P02");
}
