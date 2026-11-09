//! What the text passes answered until stage 5c (docs/adr/0001-sql-ast.md), answered
//! from the parse tree: the statements whose typing depends on the catalog, and what
//! Postgres refuses, with Postgres' SQLSTATE, message and position. Each answer is
//! Postgres 17's.

use crate::common::*;

/// The first column of sql's rows as text, whatever its type, NULL as "NULL".
fn texts(conn: &Conn, sql: &str) -> Vec<String> {
    let rows = conn.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.iter()
        .map(|r| {
            if r.cell(0).is_null() {
                "NULL".to_string()
            } else {
                r.cell(0).text()
            }
        })
        .collect()
}

/// The error of sql over the simple protocol and over the extended one.
fn errors(addr: &Server, sql: &str) -> [PgError; 2] {
    let simple = connect(addr);
    let s = simple.exec(sql, &[]).expect_err(sql);
    let mut raw = raw_conn(addr, "overlite", "");
    let e = extended_exec(&mut raw, sql).expect_err(sql);
    [pg_error(&s).clone(), pg_error(&e).clone()]
}

#[track_caller]
fn assert_refused(addr: &Server, sql: &str, code: &str, message: &str, position: &str) {
    for e in errors(addr, sql) {
        assert_eq!(
            (e.code.as_str(), e.message.as_str(), e.position.as_str()),
            (code, message, position),
            "{sql}"
        );
    }
}

// A parse error is Postgres' own: its message, its position (in characters, from 1),
// 42601, over both protocols, where SQLite's message came before.
#[test]
fn parse_errors_are_postgres_own() {
    let addr = start_server();
    assert_refused(&addr, "SELEC 1", "42601", "syntax error at or near \"SELEC\"", "1");
    assert_refused(&addr, "SELECT (1", "42601", "syntax error at end of input", "10");
    assert_refused(&addr, "SELECT 1 +", "42601", "syntax error at end of input", "11");
    assert_refused(
        &addr,
        "SELECT 'abc",
        "42601",
        "unterminated quoted string at or near \"'abc\"",
        "8",
    );
    assert_refused(
        &addr,
        "SELECT 'ação' FROM t WHERE",
        "42601",
        "syntax error at end of input",
        "27",
    );
    assert_refused(
        &addr,
        "CREATE TABLE t (id integer primary key autoincrement, v text)",
        "42601",
        "syntax error at or near \"autoincrement\"",
        "40",
    );
    // A code of the grammar's own.
    assert_refused(
        &addr,
        "CREATE ROLE public",
        "42939",
        "role name \"public\" is reserved",
        "13",
    );
    // In an aborted transaction too, as Postgres parses first.
    let conn = connect(&addr);
    must_exec(&conn, "BEGIN");
    conn.exec("SELECT * FROM nosuch", &[]).unwrap_err();
    assert_pg_error(&conn.exec("SELEC 1", &[]).unwrap_err(), "42601", "SELEC");
    must_exec(&conn, "ROLLBACK");
}

// SQLite's dialect is refused as Postgres refuses it: AUTOINCREMENT (above), EXPLAIN
// QUERY PLAN, `LIMIT a, b` (with Postgres' hint), PRAGMA, SQLite's INSERT OR, and DROP
// TRIGGER without its table.
#[test]
fn sqlite_dialect_is_refused() {
    let addr = start_server();
    let conn = connect(&addr);
    must_exec(&conn, "CREATE TABLE t (a int)");
    assert_refused(
        &addr,
        "EXPLAIN QUERY PLAN SELECT a FROM t",
        "42601",
        "syntax error at or near \"QUERY\"",
        "9",
    );
    for e in errors(&addr, "SELECT a FROM t LIMIT 0, 1") {
        assert_eq!(e.message, "LIMIT #,# syntax is not supported");
        assert_eq!(e.hint, "Use separate LIMIT and OFFSET clauses.");
        assert_eq!(e.position, "17");
    }
    assert_refused(
        &addr,
        "PRAGMA table_info(t)",
        "42601",
        "syntax error at or near \"PRAGMA\"",
        "1",
    );
    assert_refused(
        &addr,
        "INSERT OR IGNORE INTO t VALUES (1)",
        "42601",
        "syntax error at or near \"OR\"",
        "8",
    );
    assert_refused(&addr, "DROP TRIGGER x", "42601", "syntax error at end of input", "15");
    assert_eq!(texts(&conn, "SELECT count(*) FROM t"), ["0"]);
}

// SQLite's CREATE TRIGGER ... BEGIN ... END is the one statement of its dialect that is
// still taken, as written: while no PL/pgSQL runs, it is the only trigger there is.
#[test]
fn a_sqlite_trigger_still_runs() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, v text)");
    must_exec(&conn, "CREATE TABLE audit (n int, what text)");
    must_exec(
        &conn,
        "CREATE TRIGGER t_ai AFTER INSERT ON t FOR EACH ROW WHEN NEW.id > 1 \
         BEGIN INSERT INTO audit VALUES (NEW.id, 'in: ' || NEW.v); END",
    );
    must_exec(&conn, "INSERT INTO t VALUES (1, 'a'), (2, 'b')");
    assert_eq!(texts(&conn, "SELECT what FROM audit"), ["in: b"]);
    must_exec(&conn, "DROP TRIGGER t_ai ON t");
    must_exec(&conn, "INSERT INTO t VALUES (3, 'c')");
    assert_eq!(texts(&conn, "SELECT count(*) FROM audit"), ["1"]);
}

// A tree deeper than the server reads is refused as Postgres refuses one past its
// stack (54001); a 200-member UNION, which prost's recursion limit stopped at 99,
// is read whole.
#[test]
fn deep_trees() {
    let addr = start_server();
    let conn = connect(&addr);
    let union = (0..200)
        .map(|i| format!("(SELECT {i})"))
        .collect::<Vec<_>>()
        .join(" UNION ");
    assert_eq!(texts(&conn, &format!("SELECT count(*) FROM ({union}) u")), ["200"]);
    let sum = format!("SELECT 0{}", " + 1".repeat(400));
    assert_eq!(texts(&conn, &sum), ["400"]);
    let deep = format!("SELECT {}1{}", "(SELECT ".repeat(1500), ")".repeat(1500));
    for e in errors(&addr, &deep) {
        assert_eq!(
            (e.code.as_str(), e.message.as_str()),
            ("54001", "stack depth limit exceeded")
        );
    }
    // The connection answers the next statement.
    assert_eq!(texts(&conn, "SELECT 1"), ["1"]);
}

// A derived column list longer than its relation's columns: Postgres' 42P10.
#[test]
fn derived_column_list_too_long() {
    let addr = start_server();
    for (sql, msg) in [
        (
            "SELECT * FROM (SELECT 1 AS x) AS v(a, b)",
            "table \"v\" has 1 columns available but 2 columns specified",
        ),
        (
            "SELECT * FROM (VALUES (1, 2)) AS v(a, b, c)",
            "table \"v\" has 2 columns available but 3 columns specified",
        ),
    ] {
        for e in errors(&addr, sql) {
            assert_eq!((e.code.as_str(), e.message.as_str()), ("42P10", msg), "{sql}");
        }
    }
}

// hstore's operators on an hstore column, from the tree: `->` its value as text, `?`,
// `?|`, `?&`, `@>`; `->>`, which hstore has not, is Postgres' 42883.
#[test]
fn hstore_operators() {
    let addr = start_server();
    let conn = connect(&addr);
    must_exec(&conn, "CREATE TABLE t (id int, h hstore)");
    must_exec(
        &conn,
        "INSERT INTO t VALUES (1, 'a=>1, b=>2, \"$x.y\"=>3'::hstore), (2, 'x=>10'::hstore)",
    );
    for (sql, want) in [
        ("SELECT h -> 'a' FROM t ORDER BY id", vec!["1", "NULL"]),
        ("SELECT h -> '$x.y' FROM t WHERE id = 1", vec!["3"]),
        ("SELECT h -> 'zz' IS NULL FROM t WHERE id = 1", vec!["t"]),
        ("SELECT h -> ARRAY['a', 'b'] FROM t WHERE id = 1", vec!["{1,2}"]),
        ("SELECT id FROM t WHERE h ? 'b'", vec!["1"]),
        (
            "SELECT id FROM t WHERE h ?| ARRAY['b', 'x'] ORDER BY id",
            vec!["1", "2"],
        ),
        ("SELECT id FROM t WHERE h ?& ARRAY['a', 'b']", vec!["1"]),
        ("SELECT id FROM t WHERE h @> 'a=>1'", vec!["1"]),
    ] {
        assert_eq!(texts(&conn, sql), want, "{sql}");
    }
    assert_refused(
        &addr,
        "SELECT h ->> 'a' FROM t",
        "42883",
        "operator does not exist: hstore ->> unknown",
        "10",
    );
}

// A json operator over an array is Postgres' 42883, where it read the array's JSON.
#[test]
fn json_operator_over_an_array() {
    let addr = start_server();
    let conn = connect(&addr);
    must_exec(&conn, "CREATE TABLE arr (a int[], t text[])");
    must_exec(&conn, "INSERT INTO arr VALUES ('{1,2}', '{x}')");
    assert_refused(
        &addr,
        "SELECT a -> 0 FROM arr",
        "42883",
        "operator does not exist: integer[] -> integer",
        "10",
    );
    assert_refused(
        &addr,
        "SELECT array_length(t, 1), t -> 0 IS NULL FROM arr",
        "42883",
        "operator does not exist: text[] -> integer",
        "30",
    );
    // A subscript is the array's own.
    assert_eq!(texts(&conn, "SELECT a[2] FROM arr"), ["2"]);
}

// AT TIME ZONE of a date is of its midnight in the session's zone, as Postgres casts it
// to a timestamptz: a timestamp without time zone.
#[test]
fn date_at_time_zone() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE dd (d date)");
    must_exec(&conn, "INSERT INTO dd VALUES ('2024-01-02')");
    assert_eq!(
        texts(&conn, "SELECT d AT TIME ZONE 'Asia/Tokyo' FROM dd"),
        ["2024-01-02 09:00:00"]
    );
    must_exec(&conn, "SET TIME ZONE 'America/Sao_Paulo'");
    assert_eq!(
        texts(&conn, "SELECT d AT TIME ZONE 'Asia/Tokyo' FROM dd"),
        ["2024-01-02 12:00:00"]
    );
    assert_eq!(
        texts(&conn, "SELECT '2024-03-01'::date AT TIME ZONE 'UTC'"),
        ["2024-03-01 03:00:00"]
    );
}

// A boolean column that holds text (a file from before the storage format, a value no
// boolean reads as left in it) is read in both forms by the tree, as
// resolve/booleans.rs read it: a comparison over every spelling of its truth value,
// any other reading through overlite_bool, a whole select item as it is.
#[test]
fn a_boolean_column_holding_text() {
    let dir = temp_dir();
    let path = dir.path().join("test.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        r#"CREATE TABLE "Flag" ("id" INTEGER PRIMARY KEY, "on" BOOLEAN);
           INSERT INTO "Flag" VALUES (1, 'yes'), (2, 'maybe'), (3, 0), (4, 'TRUE'), (5, 'off');"#,
    )
    .unwrap();
    drop(db);
    let conn = connect(&start_server_on(&path, 0));
    for (sql, want) in [
        (r#"SELECT id FROM "Flag" WHERE "on" = true ORDER BY id"#, vec!["1", "4"]),
        (r#"SELECT id FROM "Flag" WHERE "on" = 'f' ORDER BY id"#, vec!["3", "5"]),
        (
            r#"SELECT id FROM "Flag" WHERE "on" <> false ORDER BY id"#,
            vec!["1", "2", "4"],
        ),
        (r#"SELECT id FROM "Flag" WHERE "on" ORDER BY id"#, vec!["1", "4"]),
        (r#"SELECT id FROM "Flag" WHERE NOT "on" ORDER BY id"#, vec!["3", "5"]),
        (
            r#"SELECT id FROM "Flag" WHERE "on" IS TRUE ORDER BY id"#,
            vec!["1", "4"],
        ),
        (r#"SELECT count(*) FILTER (WHERE "on") FROM "Flag""#, vec!["2"]),
        (r#"SELECT bool_or("on") FROM "Flag" WHERE id IN (3, 5)"#, vec!["f"]),
        (
            r#"SELECT "on" FROM "Flag" WHERE id IN (1, 3, 5) ORDER BY id"#,
            vec!["t", "f", "f"],
        ),
    ] {
        assert_eq!(texts(&conn, sql), want, "{sql}");
    }
}

// LISTEN, NOTIFY and PREPARE/EXECUTE are read from their trees: a channel's name as
// the grammar folds it, a payload as its literal means it (E'' and a quote inside),
// a prepared query after AS whatever its argument types hold, EXECUTE's arguments by
// the scanner's commas (a comma or a parenthesis in a literal is the literal's).
#[test]
fn notify_and_prepare_by_their_trees() {
    let addr = start_server();
    let listener = connect(&addr);
    let notifier = connect(&addr);
    listener.exec("LISTEN \"Chan\"", &[]).unwrap();
    notifier.exec("NOTIFY \"Chan\", E'it''s\\tx, (y)'", &[]).unwrap();
    let n = listener
        .wait_for_notification(std::time::Duration::from_secs(3))
        .unwrap();
    assert_eq!((n.channel.as_str(), n.payload.as_str()), ("Chan", "it's\tx, (y)"));
    listener.exec("UNLISTEN *", &[]).unwrap();

    let conn = connect(&addr);
    must_exec(&conn, "CREATE TABLE t (a text, b int)");
    must_exec(&conn, "PREPARE \"Ins\" (text, int) AS INSERT INTO t VALUES ($1, $2)");
    must_exec(&conn, "EXECUTE \"Ins\" ('a, (b)', 2)");
    must_exec(&conn, "PREPARE q AS SELECT b FROM t WHERE a = $1 -- $2");
    assert_eq!(texts(&conn, "EXECUTE q ('a, (b)')"), ["2"]);
    must_exec(&conn, "DEALLOCATE q");
    assert_sqlstate(&conn.exec("EXECUTE q ('x')", &[]).unwrap_err(), "26000");
    assert_sqlstate(&conn.exec("DEALLOCATE q", &[]).unwrap_err(), "26000");
    must_exec(&conn, "DEALLOCATE ALL");
}
