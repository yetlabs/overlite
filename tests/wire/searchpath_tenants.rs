use std::sync::mpsc;
use std::thread;

use crate::common::*;

// Two tenants holding a table of the same name must not end up sharing one.
// This is the regression that matters: a leak here mixes one customer's rows
// into another's, with no error to notice.
#[test]
fn search_path_isolates_tenants() {
    let addr = start_server();

    let setup = connect(&addr);
    must_exec(&setup, "CREATE SCHEMA acme");
    must_exec(&setup, "CREATE SCHEMA globex");
    must_exec(&setup, "CREATE TABLE acme.users (id int primary key, name text)");
    must_exec(&setup, "CREATE TABLE globex.users (id int primary key, name text)");

    // Each tenant writes through an unqualified name, under its own path.
    for tenant in ["acme", "globex"] {
        let conn = connect(&addr);
        must_exec(&conn, &format!("SET search_path TO {tenant}"));
        conn.exec("INSERT INTO users VALUES (1, $1)", &[&format!("{tenant}-user")])
            .unwrap_or_else(|e| panic!("insert as {tenant}: {e}"));
    }

    // Each row landed in its own tenant's table.
    for tenant in ["acme", "globex"] {
        let name: String = setup
            .query_row(&format!("SELECT name FROM {tenant}.users WHERE id = 1"), &[])
            .scan()
            .unwrap();
        assert_eq!(
            name,
            format!("{tenant}-user"),
            "{tenant}.users holds another tenant's row"
        );

        let n: i64 = setup
            .query_row(&format!("SELECT count(*) FROM {tenant}.users"), &[])
            .scan()
            .unwrap();
        assert_eq!(n, 1, "{tenant}.users has the wrong number of rows");
    }

    // And each tenant reads back only its own.
    for tenant in ["acme", "globex"] {
        let conn = connect(&addr);
        must_exec(&conn, &format!("SET search_path TO {tenant}"));

        let name: String = conn
            .query_row("SELECT name FROM users WHERE id = 1", &[])
            .scan()
            .unwrap();
        assert_eq!(name, format!("{tenant}-user"), "{tenant} read another tenant's row");
    }
}

// A path-qualified CREATE lands in the path's first schema, not in public.
#[test]
fn search_path_create() {
    let addr = start_server();

    let conn = connect(&addr);
    must_exec(&conn, "CREATE SCHEMA app");
    must_exec(&conn, "SET search_path TO app");
    must_exec(&conn, "CREATE TABLE widget (id int primary key)");
    must_exec(&conn, "INSERT INTO widget VALUES (1)");

    let other = connect(&addr);
    let n: i64 = other.query_row("SELECT count(*) FROM app.widget", &[]).scan().unwrap();
    assert_eq!(n, 1, "the table was not created in the path's schema");

    // public did not get one.
    let in_public: i64 = other
        .query_row(
            "SELECT count(*) FROM information_schema.tables
		 WHERE table_schema = 'public' AND table_name = 'widget'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(in_public, 0, "the table leaked into public");
}

// An unqualified name still finds a public table when no path schema has one.
#[test]
fn search_path_falls_back_to_public() {
    let addr = start_server();

    let conn = connect(&addr);
    must_exec(&conn, "CREATE SCHEMA app");
    must_exec(&conn, "CREATE TABLE shared (id int primary key)");
    must_exec(&conn, "INSERT INTO shared VALUES (7)");
    must_exec(&conn, "SET search_path TO app, public");

    let id: i64 = conn.query_row("SELECT id FROM shared", &[]).scan().unwrap();
    assert_eq!(id, 7);
}

// Two tenants writing at the same time must not fail on each other.
#[test]
fn concurrent_tenant_writes() {
    let addr = start_server();

    let setup = connect(&addr);
    for s in ["acme", "globex"] {
        must_exec(&setup, &format!("CREATE SCHEMA {s}"));
        must_exec(
            &setup,
            &format!("CREATE TABLE {s}.t (id integer primary key autoincrement, v text)"),
        );
    }

    const PER_TENANT: i64 = 40;
    let (tx, errs) = mpsc::channel();
    let mut handles = Vec::new();
    for schema in ["acme", "globex"] {
        let addr = addr.clone();
        let tx = tx.clone();
        handles.push(thread::spawn(move || {
            let c = connect(&addr);
            for _ in 0..PER_TENANT {
                if let Err(err) = c.exec(&format!("INSERT INTO {schema}.t (v) VALUES ($1)"), &[&"x"]) {
                    tx.send(err).unwrap();
                    return;
                }
            }
        }));
    }
    drop(tx);
    for h in handles {
        h.join().unwrap();
    }
    if let Some(err) = errs.iter().next() {
        panic!("a tenant's write failed while another was writing: {err}");
    }

    for s in ["acme", "globex"] {
        let n: i64 = setup
            .query_row(&format!("SELECT count(*) FROM {s}.t"), &[])
            .scan()
            .unwrap();
        assert_eq!(n, PER_TENANT, "{s} lost writes");
    }
}
