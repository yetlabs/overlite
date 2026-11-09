//! Statements the session routes by their parse tree (docs/adr/0001-sql-ast.md, stage
//! 5b): each was misrouted by its words before the stage, held to Postgres 17.

use crate::common::*;

// SET LOCAL ROLE and SET SESSION ROLE set the role as SET ROLE does, LOCAL until the
// transaction ends; a rollback undoes a SET ROLE. The text read ROLE as the second
// word only: SET LOCAL ROLE was a SET of nothing, and the statements after it ran as
// the session's role, a superuser here, past the table's grants and its row-level
// security.
#[test]
fn set_local_role_holds_for_its_transaction() {
    let conn = connect(&start_server());
    let me = scan_str(&conn, "SELECT current_user");
    for sql in [
        "CREATE ROLE app",
        "CREATE TABLE docs (id int, owner text)",
        "INSERT INTO docs VALUES (1, 'app'), (2, 'other')",
        "CREATE TABLE secret (s text)",
        "GRANT SELECT ON docs TO app",
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY own ON docs FOR SELECT TO app USING (owner = current_user)",
        "BEGIN",
        "SET LOCAL ROLE app",
    ] {
        must_exec(&conn, sql);
    }
    assert_eq!(
        scan_str(&conn, "SELECT current_user || ' ' || (SELECT count(*) FROM docs)"),
        "app 1"
    );
    assert_sqlstate(&conn.exec("SELECT * FROM secret", &[]).unwrap_err(), "42501");
    must_exec(&conn, "ROLLBACK");
    assert_eq!(
        scan_str(&conn, "SELECT current_user || ' ' || (SELECT count(*) FROM docs)"),
        format!("{me} 2")
    );

    must_exec(&conn, "BEGIN");
    must_exec(&conn, "SET LOCAL ROLE app");
    must_exec(&conn, "COMMIT");
    assert_eq!(scan_str(&conn, "SELECT current_user"), me);

    must_exec(&conn, "SET SESSION ROLE app");
    assert_eq!(scan_str(&conn, "SELECT current_user"), "app");
    must_exec(&conn, "RESET ROLE");

    must_exec(&conn, "BEGIN");
    must_exec(&conn, "SET ROLE app");
    must_exec(&conn, "ROLLBACK");
    assert_eq!(scan_str(&conn, "SELECT current_user"), me);

    // Outside a transaction block SET LOCAL warns and changes nothing.
    must_exec(&conn, "SET LOCAL ROLE app");
    assert_eq!(scan_str(&conn, "SELECT current_user"), me);
}

// The same over the extended protocol, which routes a statement at Parse.
#[test]
fn set_local_role_over_the_extended_protocol() {
    let srv = start_server();
    let conn = connect_extended(&srv);
    must_exec(&conn, "CREATE ROLE app");
    must_exec(&conn, "CREATE TABLE secret (s text)");
    // Each prepared, then executed: Parse, Bind, Execute.
    let extended = |sql: &str| {
        conn.prepare(sql, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        conn.exec(sql, &[])
    };
    extended("BEGIN").unwrap();
    extended("SET LOCAL ROLE app").unwrap();
    assert_sqlstate(&extended("SELECT * FROM secret").unwrap_err(), "42501");
    extended("ROLLBACK").unwrap();
    extended("SELECT * FROM secret").unwrap();
}
