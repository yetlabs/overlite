use crate::common::*;
use std::time::{Duration, Instant};

// DECLARE, FETCH, MOVE and CLOSE, as Postgres 17 answers them: every expected row,
// tag and SQLSTATE here is what it gave for the same statements. Django declares a
// cursor for every .iterator() and dumpdata (NO SCROLL CURSOR WITH HOLD outside a
// transaction, WITHOUT HOLD inside one), and psycopg's named cursors describe the
// cursor as a portal and fetch over the extended protocol.

/// The rows of the statement's result, cells joined with `|`.
fn rows(c: &Conn, sql: &str) -> Vec<String> {
    let r = c.pg_exec(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let last = r.last().unwrap();
    (0..last.rows.len())
        .map(|i| {
            (0..last.fields.len())
                .map(|j| last.text(i, j))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn tag(c: &Conn, sql: &str) -> String {
    let r = c.pg_exec(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.last().unwrap().tag.0.clone()
}

#[track_caller]
fn fails(c: &Conn, sql: &str, code: &str, message: &str) {
    let e = c.exec(sql, &[]).expect_err(sql);
    assert_pg_error(&e, code, message);
}

fn setup(c: &Conn) {
    must_exec(
        c,
        "CREATE TABLE r5 (id int, name text); INSERT INTO r5 VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e')",
    );
}

#[test]
fn django_declares_a_cursor_with_hold_in_a_transaction() {
    let server = start_server();
    let c = connect(&server);
    setup(&c);
    must_exec(&c, "BEGIN");
    assert_eq!(
        tag(
            &c,
            r#"DECLARE "c1" NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM r5 ORDER BY id"#
        ),
        "DECLARE CURSOR"
    );
    assert_eq!(rows(&c, r#"FETCH FORWARD 2 FROM "c1""#), ["1", "2"]);
    assert_eq!(tag(&c, r#"FETCH FORWARD 2 FROM "c1""#), "FETCH 2");
    assert_eq!(rows(&c, "SELECT 1 FROM pg_catalog.pg_cursors WHERE name = 'c1'"), ["1"]);
    assert_eq!(tag(&c, r#"CLOSE "c1""#), "CLOSE CURSOR");
    must_exec(&c, "COMMIT");
}

#[test]
fn a_held_cursor_outlives_its_transaction() {
    let server = start_server();
    let c = connect(&server);
    setup(&c);
    // Outside a transaction, as Django's autocommit declares one.
    must_exec(
        &c,
        "DECLARE h NO SCROLL CURSOR WITH HOLD FOR SELECT id, name FROM r5 ORDER BY id",
    );
    assert_eq!(rows(&c, "FETCH 2 FROM h"), ["1|a", "2|b"]);
    assert_eq!(
        rows(
            &c,
            "SELECT name, statement, is_holdable, is_binary, is_scrollable FROM pg_cursors"
        ),
        ["h|DECLARE h NO SCROLL CURSOR WITH HOLD FOR SELECT id, name FROM r5 ORDER BY id|t|f|f"]
    );
    // A write after the declaration is not seen: the rows were taken when it ended.
    must_exec(&c, "INSERT INTO r5 VALUES (6, 'f')");
    assert_eq!(rows(&c, "FETCH ALL FROM h"), ["3|c", "4|d", "5|e"]);
    assert_eq!(rows(&c, "FETCH NEXT FROM h"), Vec::<String>::new());
    let e = c.exec("FETCH PRIOR FROM h", &[]).unwrap_err();
    assert_pg_error(&e, "55000", "cursor can only scan forward");
    assert_eq!(
        pg_error(&e).hint,
        "Declare it with SCROLL option to enable backward scan."
    );
    must_exec(&c, "CLOSE h");
    assert_eq!(rows(&c, "SELECT name FROM pg_cursors"), Vec::<String>::new());
}

#[test]
fn cursor_errors_are_postgres_errors() {
    let server = start_server();
    let c = connect(&server);
    setup(&c);
    fails(
        &c,
        "DECLARE n CURSOR FOR SELECT 1",
        "25P01",
        "DECLARE CURSOR can only be used in transaction blocks",
    );
    fails(&c, "FETCH NEXT FROM nope", "34000", "cursor \"nope\" does not exist");
    fails(&c, "CLOSE nope", "34000", "cursor \"nope\" does not exist");
    fails(&c, "MOVE NEXT IN nope", "34000", "cursor \"nope\" does not exist");
    must_exec(&c, "BEGIN");
    must_exec(&c, "DECLARE s SCROLL CURSOR FOR SELECT id, name FROM r5 ORDER BY id");
    fails(
        &c,
        "DECLARE s CURSOR FOR SELECT 1",
        "42P03",
        "cursor \"s\" already exists",
    );
    must_exec(&c, "ROLLBACK");
    // A quoted name keeps its case.
    must_exec(&c, "BEGIN");
    must_exec(&c, r#"DECLARE "Mixed Name" CURSOR FOR SELECT 1"#);
    assert_eq!(rows(&c, r#"FETCH FROM "Mixed Name""#), ["1"]);
    fails(
        &c,
        r#"FETCH FROM "mixed name""#,
        "34000",
        "cursor \"mixed name\" does not exist",
    );
    must_exec(&c, "ROLLBACK");
}

#[test]
fn a_scroll_cursor_moves_every_way() {
    let server = start_server();
    let c = connect(&server);
    setup(&c);
    must_exec(&c, "BEGIN");
    must_exec(&c, "DECLARE s SCROLL CURSOR FOR SELECT id, name FROM r5 ORDER BY id");
    let steps: &[(&str, &[&str])] = &[
        ("FETCH LAST FROM s", &["5|e"]),
        ("FETCH PRIOR FROM s", &["4|d"]),
        ("FETCH ABSOLUTE 2 FROM s", &["2|b"]),
        ("FETCH RELATIVE 2 FROM s", &["4|d"]),
        ("FETCH RELATIVE -1 FROM s", &["3|c"]),
        ("FETCH RELATIVE 0 FROM s", &["3|c"]),
        ("FETCH FIRST FROM s", &["1|a"]),
        ("FETCH FORWARD ALL FROM s", &["2|b", "3|c", "4|d", "5|e"]),
        ("FETCH BACKWARD 2 FROM s", &["5|e", "4|d"]),
        ("FETCH BACKWARD ALL FROM s", &["3|c", "2|b", "1|a"]),
        ("FETCH ABSOLUTE -2 FROM s", &["4|d"]),
        ("FETCH ABSOLUTE 9 FROM s", &[]),
        ("FETCH -1 FROM s", &["5|e"]),
    ];
    for (sql, want) in steps {
        assert_eq!(rows(&c, sql), *want, "{sql}");
    }
    let moves = [
        ("MOVE ABSOLUTE 0 IN s", "MOVE 0"),
        ("MOVE FORWARD 3 IN s", "MOVE 3"),
        ("MOVE FORWARD ALL IN s", "MOVE 2"),
        ("MOVE BACKWARD ALL FROM s", "MOVE 5"),
        ("MOVE LAST IN s", "MOVE 1"),
        ("MOVE RELATIVE -2 IN s", "MOVE 1"),
        ("MOVE ABSOLUTE 9 IN s", "MOVE 0"),
        ("MOVE BACKWARD 2 IN s", "MOVE 2"),
    ];
    for (sql, want) in moves {
        assert_eq!(tag(&c, sql), want, "{sql}");
    }
    assert_eq!(rows(&c, "FETCH ALL FROM s"), ["5|e"]);
    assert_eq!(rows(&c, "FETCH ABSOLUTE 3 IN s"), ["3|c"]);
    must_exec(&c, "MOVE FORWARD 3 IN s");
    assert_eq!(rows(&c, "FETCH s"), &[] as &[&str]);
    must_exec(&c, "MOVE BACKWARD ALL FROM s");
    assert_eq!(rows(&c, "FETCH 2 IN s"), ["1|a", "2|b"]);
    assert_eq!(tag(&c, "CLOSE s"), "CLOSE CURSOR");
    must_exec(&c, "ROLLBACK");
}

#[test]
fn a_no_scroll_cursor_only_moves_forward() {
    let server = start_server();
    let c = connect(&server);
    setup(&c);
    must_exec(&c, "BEGIN");
    must_exec(&c, "DECLARE f NO SCROLL CURSOR FOR SELECT id FROM r5 ORDER BY id");
    assert_eq!(rows(&c, "FETCH ABSOLUTE 2 FROM f"), ["2"]);
    assert_eq!(rows(&c, "FETCH ABSOLUTE 4 FROM f"), ["4"]);
    fails(&c, "FETCH RELATIVE 0 FROM f", "55000", "cursor can only scan forward");
    must_exec(&c, "ROLLBACK");
    must_exec(&c, "BEGIN");
    must_exec(&c, "DECLARE f NO SCROLL CURSOR FOR SELECT id FROM r5 ORDER BY id");
    assert_eq!(rows(&c, "FETCH 1 FROM f"), ["1"]);
    fails(&c, "FETCH FIRST FROM f", "55000", "cursor can only scan forward");
    must_exec(&c, "ROLLBACK");
}

#[test]
fn the_end_of_a_transaction_closes_its_cursors() {
    let server = start_server();
    let c = connect(&server);
    setup(&c);
    must_exec(&c, "BEGIN");
    must_exec(&c, "DECLARE g CURSOR WITHOUT HOLD FOR SELECT id FROM r5");
    must_exec(&c, "DECLARE k CURSOR WITH HOLD FOR SELECT id FROM r5 ORDER BY id");
    assert_eq!(rows(&c, "FETCH 1 FROM k"), ["1"]);
    must_exec(&c, "COMMIT");
    fails(&c, "FETCH 1 FROM g", "34000", "cursor \"g\" does not exist");
    assert_eq!(rows(&c, "FETCH 1 FROM k"), ["2"]);
    // A rollback takes the cursors declared in it, and leaves those held before.
    must_exec(&c, "BEGIN");
    must_exec(&c, "DECLARE m CURSOR WITH HOLD FOR SELECT 1");
    must_exec(&c, "ROLLBACK");
    assert_eq!(rows(&c, "FETCH 1 FROM k"), ["3"]);
    fails(&c, "FETCH 1 FROM m", "34000", "cursor \"m\" does not exist");
    assert_eq!(rows(&c, "SELECT name FROM pg_cursors ORDER BY 1"), ["k"]);
    assert_eq!(tag(&c, "CLOSE ALL"), "CLOSE CURSOR ALL");
    assert_eq!(rows(&c, "SELECT count(*) FROM pg_cursors"), ["0"]);
    // A failed transaction ends its cursors too.
    must_exec(&c, "BEGIN");
    must_exec(&c, "DECLARE z CURSOR FOR SELECT id FROM r5");
    assert!(c.exec("SELECT * FROM no_such_table", &[]).is_err());
    must_exec(&c, "ROLLBACK");
    fails(&c, "CLOSE z", "34000", "cursor \"z\" does not exist");
}

#[test]
fn binary_and_sensitivity_options() {
    let server = start_server();
    let c = connect(&server);
    must_exec(&c, "DECLARE b BINARY CURSOR WITH HOLD FOR SELECT 1.5::float8 AS one");
    fails(
        &c,
        "DECLARE x ASENSITIVE SCROLL CURSOR WITHOUT HOLD FOR VALUES (1)",
        "25P01",
        "DECLARE CURSOR can only be used in transaction blocks",
    );
    must_exec(
        &c,
        "DECLARE y INSENSITIVE NO SCROLL CURSOR WITH HOLD FOR VALUES (1), (2)",
    );
    // Postgres calls a cursor that does not say scrollable when its plan can go back,
    // which it can here always: only NO SCROLL's is_scrollable is the same.
    assert_eq!(
        rows(&c, "SELECT name, is_holdable, is_binary FROM pg_cursors ORDER BY 1"),
        ["b|t|t", "y|t|f"]
    );
    assert_eq!(rows(&c, "SELECT is_scrollable FROM pg_cursors WHERE name = 'y'"), ["f"]);
    assert_eq!(rows(&c, "FETCH ALL y"), ["1", "2"]);
    // A binary cursor's rows go out in binary, over the simple protocol too.
    let r = c.pg_exec("FETCH b").unwrap();
    assert_eq!(r[0].fields[0].format, 1);
    assert_eq!(r[0].fields[0].name, "one");
    assert_eq!(r[0].cell(0, 0), 1.5f64.to_be_bytes());
}

/// Every message up to ReadyForQuery.
fn exchange(fe: &mut RawConn) -> Vec<Backend> {
    fe.flush().unwrap();
    let mut out = Vec::new();
    loop {
        let m = fe.receive().unwrap();
        let done = matches!(m, Backend::ReadyForQuery(_));
        out.push(m);
        if done {
            return out;
        }
    }
}

// psycopg's named cursor: DECLARE with a parameter through Parse/Bind/Execute, a
// Describe of the cursor as a portal, then FETCH FORWARD n in binary.
#[test]
fn psycopg_named_cursor_over_the_extended_protocol() {
    let server = start_server();
    let c = connect(&server);
    setup(&c);
    let mut fe = RawConn::dial(&server).unwrap();
    fe.send_startup(&[("user", "postgres"), ("database", "test")]);
    fe.flush().unwrap();
    fe.wait_ready().unwrap();

    fe.send_query("BEGIN");
    exchange(&mut fe);
    fe.send_parse(
        "",
        r#"DECLARE "named" CURSOR WITHOUT HOLD FOR SELECT id, name FROM r5 WHERE id > $1 ORDER BY id"#,
        &[23],
    );
    fe.send_bind("", "", &[1], &[Some(&1i32.to_be_bytes()[..])], &[]);
    fe.send_describe(b'P', "");
    fe.send_execute("", 0);
    fe.send_sync();
    let got = exchange(&mut fe);
    assert!(got.contains(&Backend::NoData), "{got:?}");
    assert!(
        got.contains(&Backend::CommandComplete("DECLARE CURSOR".into())),
        "{got:?}"
    );

    fe.send_describe(b'P', "named");
    fe.send_sync();
    let got = exchange(&mut fe);
    match &got[0] {
        Backend::RowDescription(f) => {
            assert_eq!(f.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["id", "name"]);
        }
        other => panic!("{other:?}"),
    }

    let mut seen = Vec::new();
    loop {
        fe.send_parse("", r#"FETCH FORWARD 2 FROM "named""#, &[]);
        fe.send_bind("", "", &[], &[], &[1]);
        fe.send_describe(b'P', "");
        fe.send_execute("", 0);
        fe.send_sync();
        let got = exchange(&mut fe);
        assert!(matches!(&got[2], Backend::RowDescription(f) if f.len() == 2), "{got:?}");
        let mut n = 0;
        for m in &got {
            if let Backend::DataRow(cells) = m {
                // An int column goes out as this server types it, int4 or int8.
                let raw = cells[0].clone().unwrap();
                let id = match raw.len() {
                    4 => i64::from(i32::from_be_bytes(raw.try_into().unwrap())),
                    _ => i64::from_be_bytes(raw.try_into().unwrap()),
                };
                let name = String::from_utf8(cells[1].clone().unwrap()).unwrap();
                seen.push(format!("{id}|{name}"));
                n += 1;
            }
        }
        assert!(got.contains(&Backend::CommandComplete(format!("FETCH {n}"))), "{got:?}");
        if n < 2 {
            break;
        }
    }
    assert_eq!(seen, ["2|b", "3|c", "4|d", "5|e"]);
    fe.send_query(r#"CLOSE "named"; COMMIT"#);
    let got = exchange(&mut fe);
    assert!(
        got.contains(&Backend::CommandComplete("CLOSE CURSOR".into())),
        "{got:?}"
    );
}

// A cursor without hold reads its rows from SQLite as they are fetched: declaring one
// over fifty million rows and fetching three does not build the fifty million.
#[test]
fn a_forward_cursor_streams() {
    let server = start_server();
    let c = connect(&server);
    must_exec(&c, "BEGIN");
    let started = Instant::now();
    must_exec(
        &c,
        "DECLARE big NO SCROLL CURSOR FOR WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 50000000) SELECT i FROM n",
    );
    assert_eq!(rows(&c, "FETCH 3 FROM big"), ["1", "2", "3"]);
    assert_eq!(rows(&c, "FETCH ABSOLUTE 1000 FROM big"), ["1000"]);
    must_exec(&c, "CLOSE big");
    must_exec(&c, "COMMIT");
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
}
