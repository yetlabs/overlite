use crate::common::*;

#[test]
fn sequences_lifecycle() {
    let conn = connect(&start_server());

    // CREATE SEQUENCE: nextval starts at 1 and advances by 1.
    must_exec(&conn, "CREATE SEQUENCE s1");
    assert_eq!(seq_val(&conn, "SELECT nextval('s1')"), 1);
    assert_eq!(seq_val(&conn, "SELECT nextval('s1')"), 2);
    assert_eq!(seq_val(&conn, "SELECT nextval('s1')"), 3);

    // currval / lastval report the session's last value.
    assert_eq!(seq_val(&conn, "SELECT currval('s1')"), 3);
    assert_eq!(seq_val(&conn, "SELECT lastval()"), 3);

    // setval sets the value; next nextval continues from there.
    assert_eq!(seq_val(&conn, "SELECT setval('s1', 100)"), 100);
    assert_eq!(seq_val(&conn, "SELECT nextval('s1')"), 101);
    // setval with is_called=false makes the next nextval return the value itself.
    seq_val(&conn, "SELECT setval('s1', 200, false)");
    assert_eq!(seq_val(&conn, "SELECT nextval('s1')"), 200);

    // START WITH / INCREMENT BY.
    must_exec(&conn, "CREATE SEQUENCE s2 START WITH 10 INCREMENT BY 5");
    assert_eq!(seq_val(&conn, "SELECT nextval('s2')"), 10);
    assert_eq!(seq_val(&conn, "SELECT nextval('s2')"), 15);

    // ALTER SEQUENCE ... RESTART.
    must_exec(&conn, "ALTER SEQUENCE s2 RESTART WITH 100");
    assert_eq!(seq_val(&conn, "SELECT nextval('s2')"), 100);

    // nextval embedded in an INSERT stores plain integers (file stays standalone).
    must_exec(&conn, "CREATE TABLE items (id bigint, name text)");
    must_exec(&conn, "CREATE SEQUENCE items_seq");
    must_exec(&conn, "INSERT INTO items (id, name) VALUES (nextval('items_seq'), 'a')");
    must_exec(&conn, "INSERT INTO items (id, name) VALUES (nextval('items_seq'), 'b')");
    assert_eq!(
        query_column(&conn, "SELECT id::text FROM items ORDER BY id", 0),
        ["1", "2"]
    );

    // IF NOT EXISTS is a no-op on an existing sequence.
    must_exec(&conn, "CREATE SEQUENCE IF NOT EXISTS s1");

    // DROP SEQUENCE.
    must_exec(&conn, "DROP SEQUENCE s1");
    conn.exec("SELECT nextval('s1')", &[])
        .expect_err("nextval on a dropped sequence must fail");
    must_exec(&conn, "DROP SEQUENCE IF EXISTS s1"); // IF EXISTS swallows the missing one

    // The internal bookkeeping table stays hidden from the catalog.
    for rel in query_column(&conn, "SELECT relname FROM pg_catalog.pg_class WHERE relkind = 'r'", 0) {
        assert!(!rel.starts_with("_overlite"), "internal table {rel:?} leaked");
    }
}

// sequence_persists_across_connections verifies the counter lives in the file,
// so a new connection continues where the previous one left off.
#[test]
fn sequence_persists_across_connections() {
    let addr = start_server();
    let c1 = connect(&addr);
    must_exec(&c1, "CREATE SEQUENCE p");
    assert_eq!(seq_val(&c1, "SELECT nextval('p')"), 1);
    assert_eq!(seq_val(&c1, "SELECT nextval('p')"), 2);

    let c2 = connect(&addr);
    assert_eq!(seq_val(&c2, "SELECT nextval('p')"), 3);
}

// sequences_extended_protocol exercises the Parse/Bind/Execute path used by
// JDBC/DBeaver: CREATE SEQUENCE and nextval must work through prepared portals.
#[test]
fn sequences_extended_protocol() {
    let conn = connect_extended(&start_server());

    must_exec(&conn, "CREATE SEQUENCE e START WITH 5");
    assert_eq!(seq_val(&conn, "SELECT nextval('e')"), 5);
    assert_eq!(seq_val(&conn, "SELECT nextval('e')"), 6);

    must_exec(&conn, "CREATE TABLE t (id bigint, v int)");
    conn.exec("INSERT INTO t (id, v) VALUES (nextval('e'), $1)", &[&42])
        .unwrap();
    assert_eq!(seq_val(&conn, "SELECT id FROM t"), 7);
}

// sequence_catalog_visibility checks a sequence shows up in the catalog the
// way psql's \ds and \d <seq> query it.
#[test]
fn sequence_catalog_visibility() {
    let conn = connect(&start_server());

    must_exec(
        &conn,
        "CREATE SEQUENCE cat_seq START WITH 7 INCREMENT BY 2 MAXVALUE 99 CYCLE",
    );

    // pg_class exposes it as relkind 'S' in the public schema.
    assert_contains!(
        query_column(&conn, "SELECT relname FROM pg_catalog.pg_class WHERE relkind = 'S'", 0),
        "cat_seq"
    );

    // pg_sequence carries the parameters (joined by oid to pg_class).
    let (start, incr, max, cycle): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT s.seqstart, s.seqincrement, s.seqmax, s.seqcycle
		 FROM pg_catalog.pg_sequence s JOIN pg_catalog.pg_class c ON c.oid = s.seqrelid
		 WHERE c.relname = 'cat_seq'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(start, 7);
    assert_eq!(incr, 2);
    assert_eq!(max, 99);
    assert_eq!(cycle, 1);
}

#[test]
fn currval_before_nextval_errors() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE SEQUENCE c");
    conn.exec("SELECT currval('c')", &[])
        .expect_err("currval before nextval in the session must error");
}

// nextval_seq: a default that only spells the call is a text default.
#[test]
fn nextval_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int, a text DEFAULT 'nextval(''s'')')");
    let a: String = conn
        .query_row("INSERT INTO t (id) VALUES (1) RETURNING a", &[])
        .scan()
        .unwrap();
    assert_eq!(a, "nextval('s')");
}

fn first_id(conn: &Conn, table: &str) -> Option<i64> {
    must_exec(conn, &format!("INSERT INTO {table} (v) VALUES ('x')"));
    conn.query_row(&format!("SELECT id FROM {table} ORDER BY rowid DESC LIMIT 1"), &[])
        .scan()
        .unwrap()
}

/// Two sequences far apart, and schemas s1 and s2.
fn two_sequences(conn: &Conn) {
    must_exec(conn, "CREATE SEQUENCE a_seq");
    must_exec(conn, "CREATE SEQUENCE b_seq START 100");
    must_exec(conn, "CREATE SCHEMA s1");
    must_exec(conn, "CREATE SCHEMA s2");
}

const NEXTVAL_T: &str = "(id int DEFAULT nextval('{}'), v text)";

fn table_on(seq: &str) -> String {
    NEXTVAL_T.replace("{}", seq)
}

// A nextval default belongs to one table, not to every table of that name.
#[test]
fn nextval_default_is_per_schema() {
    let conn = connect(&start_server());
    two_sequences(&conn);

    // Through the search_path, as a migration tool writes it.
    must_exec(&conn, "SET search_path = s1");
    must_exec(&conn, &format!("CREATE TABLE t {}", table_on("a_seq")));
    must_exec(&conn, "SET search_path = s2");
    must_exec(&conn, &format!("CREATE TABLE t {}", table_on("b_seq")));
    must_exec(&conn, "SET search_path = public");
    assert_eq!(first_id(&conn, "s1.t"), Some(1));
    assert_eq!(first_id(&conn, "s2.t"), Some(100));

    must_exec(&conn, "SET search_path = s2");
    assert_eq!(first_id(&conn, "t"), Some(101), "by the path, the table it finds");
}

#[test]
fn nextval_default_on_qualified_tables() {
    let conn = connect(&start_server());
    two_sequences(&conn);
    must_exec(&conn, &format!("CREATE TABLE s1.t {}", table_on("a_seq")));
    must_exec(&conn, &format!("CREATE TABLE \"s2\".\"t\" {}", table_on("b_seq")));
    must_exec(&conn, "CREATE TABLE t (id int, v text)");
    assert_eq!(first_id(&conn, "s1.t"), Some(1));
    assert_eq!(first_id(&conn, "\"s2\".t"), Some(100));
    assert_eq!(first_id(&conn, "t"), None, "the public table has no default");

    must_exec(&conn, "DROP TABLE t");
    must_exec(&conn, &format!("CREATE TABLE public.t {}", table_on("a_seq")));
    assert_eq!(first_id(&conn, "t"), Some(2));
    assert_eq!(first_id(&conn, "public.t"), Some(3));
}

// The default stays with the table where it goes, and leaves with it.
#[test]
fn nextval_default_moves_with_the_table() {
    let conn = connect(&start_server());
    two_sequences(&conn);
    must_exec(&conn, &format!("CREATE TABLE s1.t {}", table_on("a_seq")));
    must_exec(&conn, "ALTER TABLE s1.t RENAME TO u");
    assert_eq!(first_id(&conn, "s1.u"), Some(1));
    must_exec(&conn, "ALTER TABLE s1.u SET SCHEMA s2");
    assert_eq!(first_id(&conn, "s2.u"), Some(2));
    must_exec(&conn, "ALTER SCHEMA s2 RENAME TO s3");
    assert_eq!(first_id(&conn, "s3.u"), Some(3));

    must_exec(&conn, "DROP TABLE s3.u");
    must_exec(&conn, "CREATE TABLE s3.u (id int, v text)");
    assert_eq!(first_id(&conn, "s3.u"), None, "a new table of that name starts clean");

    must_exec(&conn, &format!("CREATE TABLE s1.t {}", table_on("a_seq")));
    must_exec(&conn, "DROP SCHEMA s1 CASCADE");
    must_exec(&conn, "CREATE SCHEMA s1");
    must_exec(&conn, "CREATE TABLE s1.t (id int, v text)");
    assert_eq!(first_id(&conn, "s1.t"), None);
}

// Repeating CREATE TABLE IF NOT EXISTS does not repeat the default.
#[test]
fn nextval_default_recorded_once() {
    let conn = connect(&start_server());
    two_sequences(&conn);
    for _ in 0..2 {
        must_exec(&conn, &format!("CREATE TABLE IF NOT EXISTS s1.t {}", table_on("a_seq")));
    }
    assert_eq!(first_id(&conn, "s1.t"), Some(1));
}

/// A database as an older server wrote it: default rows by the name the statement
/// spelled, and the upgrade not yet run.
fn stop_as_old(server: Server, db: &std::path::Path, sql: &str) {
    signal_and_wait(&server, libc::SIGTERM).0.unwrap();
    let sql = format!("{sql} DROP TABLE _overlite_defaults_stored_names;");
    rusqlite::Connection::open(db).unwrap().execute_batch(&sql).unwrap();
}

// An older server reached every namesake from a bare row, the first of them winning.
// A bare row now means the public table, and a schema table relying on it would write
// NULL keys without a word, so each keeps a copy. Sharing a sequence leaves ids unique.
#[test]
fn old_default_rows_keep_serving() {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    let first = start_proc(&db);
    let conn = connect(&first);
    two_sequences(&conn);
    must_exec(&conn, "CREATE SCHEMA s3");
    must_exec(&conn, &format!("CREATE TABLE s1.t {}", table_on("a_seq")));
    must_exec(&conn, &format!("CREATE TABLE s2.t {}", table_on("b_seq")));
    must_exec(&conn, "CREATE TABLE s3.t (other int, v text)");
    drop(conn);
    stop_as_old(first, &db, "UPDATE _overlite_defaults SET tablename = 't';");

    let server = start_proc(&db);
    let conn = connect(&server);
    assert_eq!(first_id(&conn, "s1.t"), Some(1));
    assert_eq!(first_id(&conn, "s2.t"), Some(2), "as the older server answered");
    must_exec(&conn, "INSERT INTO s3.t (v) VALUES ('no id column here')");

    // Once: a row taken away after that stays away.
    must_exec(&conn, "DELETE FROM _overlite_defaults WHERE tablename = 's2.t'");
    drop(conn);
    signal_and_wait(&server, libc::SIGTERM).0.unwrap();
    let again = connect(&start_proc(&db));
    assert_eq!(first_id(&again, "s2.t"), None);
}

// A row spelled with its schema matched nothing, so the default never applied. It
// does now, as the statement asked; a new table of the bare name owes nothing to it.
#[test]
fn old_default_row_in_qualified_spelling() {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    let first = start_proc(&db);
    let conn = connect(&first);
    two_sequences(&conn);
    must_exec(&conn, &format!("CREATE TABLE s1.t {}", table_on("a_seq")));
    must_exec(&conn, &format!("CREATE TABLE s2.t {}", table_on("b_seq")));
    must_exec(&conn, &format!("CREATE TABLE t {}", table_on("b_seq")));
    drop(conn);
    stop_as_old(
        first,
        &db,
        r#"UPDATE _overlite_defaults SET tablename = 's2"."t' WHERE tablename = 's2.t';
           UPDATE _overlite_defaults SET tablename = 'public.t' WHERE tablename = 't';"#,
    );
    let conn = connect(&start_proc(&db));
    assert_eq!(first_id(&conn, "s1.t"), Some(1));
    assert_eq!(first_id(&conn, "s2.t"), Some(100));
    assert_eq!(first_id(&conn, "t"), Some(101));
}
