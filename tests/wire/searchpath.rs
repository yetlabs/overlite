use crate::common::*;

// search_path: SET search_path makes unqualified names resolve per the path
// (single-file mode), SHOW reflects it, and CREATE goes to the first schema.
#[test]
fn search_path() {
    let conn = connect(&start_server());

    conn.exec("CREATE SCHEMA app", &[]).unwrap();
    conn.exec("CREATE TABLE app.widget (id int, name text)", &[]).unwrap();
    conn.exec("INSERT INTO app.widget VALUES (1, 'gizmo')", &[]).unwrap();

    // Without a path, an unqualified name resolves in public only → not found.
    conn.exec("SELECT * FROM widget", &[]).unwrap_err();

    // With the path, the unqualified name resolves in app.
    conn.exec("SET search_path TO app, public", &[]).unwrap();
    let name: String = conn
        .query_row("SELECT name FROM widget WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert_eq!(name, "gizmo");

    // SHOW reflects the path.
    let sp: String = conn.query_row("SHOW search_path", &[]).scan().unwrap();
    assert_contains!(sp, "app");

    // CREATE TABLE (unqualified) lands in the first path schema.
    conn.exec("CREATE TABLE gadget (id int)", &[]).unwrap();
    let ns: String = conn
        .query_row(
            "
		SELECT n.nspname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
		WHERE c.relname = 'gadget'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(ns, "app");

    // A name only in public still resolves (not shadowed in app).
    conn.exec("CREATE TABLE public.pubonly (id int)", &[]).unwrap();
    conn.exec("INSERT INTO pubonly VALUES (5)", &[]).unwrap();
    let id: i64 = conn.query_row("SELECT id FROM pubonly", &[]).scan().unwrap();
    assert_eq!(id, 5);

    // RESET restores the default (public-only resolution).
    conn.exec("RESET search_path", &[]).unwrap();
    conn.exec("SELECT * FROM widget", &[]).unwrap_err();
}
