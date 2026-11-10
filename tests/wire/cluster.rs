use std::time::Duration;

use crate::common::*;

// Each database is its own file, and a connection sees only the one it asked
// for. This is the whole point of the arrangement: nothing in SQL reaches
// across, so one tenant cannot read another whatever a query says.
#[test]
fn database_per_file() {
    let addr = start_cluster(8);
    let dir = addr.dir();

    // The database the server was started with is always there to connect to.
    let admin = dial_db(&addr, ENTRY_DATABASE).unwrap();

    let first = must_create_database(&addr, &admin, "acme");
    let second = must_create_database(&addr, &admin, "globex");

    must_exec(&first, "CREATE TABLE incidents (id int primary key, title text)");
    must_exec(&first, "INSERT INTO incidents VALUES (1, 'acme incident')");
    must_exec(&second, "CREATE TABLE incidents (id int primary key, title text)");
    must_exec(&second, "INSERT INTO incidents VALUES (1, 'globex incident')");

    let title: String = first.query_row("SELECT title FROM incidents", &[]).scan().unwrap();
    assert_eq!(title, "acme incident");
    let title: String = second.query_row("SELECT title FROM incidents", &[]).scan().unwrap();
    assert_eq!(title, "globex incident");

    // current_database() answers per connection.
    let name: String = first.query_row("SELECT current_database()", &[]).scan().unwrap();
    assert_eq!(name, "acme");
    let name: String = second.query_row("SELECT current_database()", &[]).scan().unwrap();
    assert_eq!(name, "globex");

    // A file per database, on disk.
    for n in ["acme", "globex"] {
        assert_file_exists(dir.join(format!("{n}.db")));
    }

    // Nothing in SQL crosses over.
    first
        .exec("SELECT * FROM globex.incidents", &[])
        .expect_err("a query reached into another database");
}

// Asking for a database this server does not hold is an error naming what it
// does hold — not a silent landing on whichever database happens to be here.
#[test]
fn unknown_database_is_refused() {
    let addr = start_cluster(8);
    let admin = dial_db(&addr, ENTRY_DATABASE).unwrap();
    must_exec(&admin, "CREATE DATABASE shop");

    let err = connect_config(&addr, &Config::new().database("nope").simple()).unwrap_err();
    assert_contains!(err, r#"database "nope" does not exist"#);
    assert_contains!(err, "shop", "the error should name what is available");
}

#[test]
fn create_and_drop_database() {
    let addr = start_cluster(8);
    let dir = addr.dir();

    let boot = dial_db(&addr, ENTRY_DATABASE).unwrap();
    let conn = must_create_database(&addr, &boot, "tenant_a");

    // pg_database lists what the server holds.
    let names = || -> Vec<String> {
        boot.query("SELECT datname FROM pg_database ORDER BY datname", &[])
            .unwrap()
            .collect_rows()
            .unwrap()
    };
    assert_eq!(names(), [ENTRY_DATABASE, "tenant_a"]);

    // Creating one that exists is an error, and Postgres' grammar has no way out.
    let err = boot.exec("CREATE DATABASE tenant_a", &[]).unwrap_err();
    assert_contains!(err, "already exists");
    let err = boot.exec("CREATE DATABASE IF NOT EXISTS tenant_a", &[]).unwrap_err();
    assert_sqlstate(&err, "42601");

    // A database in use cannot be dropped.
    let err = boot.exec("DROP DATABASE tenant_a", &[]).unwrap_err();
    assert_contains!(err, "being accessed");

    conn.close().unwrap();
    // Give the server a moment to notice the connection went away.
    eventually(Duration::from_secs(3), Duration::from_millis(50), || {
        boot.exec("DROP DATABASE tenant_a", &[]).is_ok()
    });

    assert_eq!(names(), [ENTRY_DATABASE]);
    assert_no_file_exists(dir.join("tenant_a.db"));

    // And it is gone for a client too.
    assert!(dial_db(&addr, "tenant_a").is_none());
}

// Far more databases than the cluster keeps open at once still all work: the
// idle ones are closed and reopened on demand.
#[test]
fn more_databases_than_stay_open() {
    const TOTAL: usize = 12;
    const MAX_OPEN: usize = 3;
    let addr = start_cluster(MAX_OPEN);

    let admin = dial_db(&addr, ENTRY_DATABASE).unwrap();
    for i in 0..TOTAL {
        must_exec(&admin, &format!("CREATE DATABASE t{i:02}"));
    }
    admin.close().unwrap();

    // Write to each in turn, which forces eviction and reopening.
    for i in 0..TOTAL {
        let name = format!("t{i:02}");
        let c = dial_db(&addr, &name).unwrap_or_else(|| panic!("could not reach {name}"));
        must_exec(&c, "CREATE TABLE t (v text)");
        c.exec("INSERT INTO t VALUES ($1)", &[&name]).unwrap();
        c.close().unwrap();
    }

    // Every one kept its own data.
    for i in 0..TOTAL {
        let name = format!("t{i:02}");
        let c = dial_db(&addr, &name).unwrap();
        let v: String = c.query_row("SELECT v FROM t", &[]).scan().unwrap();
        assert_eq!(v, name);
        c.close().unwrap();
    }
}

// The file overlite is pointed at brings its directory with it: a database
// created from a connection lands beside it, and is reachable by name. It is
// also the one database that is always there, so it cannot be dropped.
#[test]
fn entry_database_brings_its_directory() {
    let dir = temp_dir();
    let path = dir.path().join("shop.db");

    let addr = start_server_on(&path, 8);

    // Pointing at a path that did not exist creates it, so there is something
    // to connect to.
    assert_file_exists(&path);

    let shop = dial_db(&addr, "shop").expect("the database the server was started with is not reachable");

    // A database created from here lands beside the file.
    must_exec(&shop, "CREATE DATABASE warehouse");
    assert_file_exists(dir.path().join("warehouse.db"));

    let warehouse = dial_db(&addr, "warehouse").unwrap();
    must_exec(&warehouse, "CREATE TABLE t (v text)");
    warehouse.exec("INSERT INTO t VALUES ('kept')", &[]).unwrap();

    // The entry database cannot be dropped: it is the way back in.
    let err = warehouse.exec("DROP DATABASE shop", &[]).unwrap_err();
    assert_contains!(err, "started with");
}

// A database already sitting beside the file is served without being created
// through overlite, which is what makes the directory the unit.
#[test]
fn existing_sibling_is_a_database() {
    let dir = temp_dir();

    // A database written before the server ever starts: a plain SQLite file.
    let pre = rusqlite::Connection::open(dir.path().join("legacy.db")).unwrap();
    pre.execute("CREATE TABLE t (v text)", []).unwrap();
    pre.execute("INSERT INTO t VALUES ('from before')", []).unwrap();
    drop(pre);

    let addr = start_server_on(dir.path().join("main.db"), 8);
    let conn = dial_db(&addr, "legacy").expect("a database beside the entry file was not served");

    let v: String = conn.query_row("SELECT v FROM t", &[]).scan().unwrap();
    assert_eq!(v, "from before");
}

// Roles belong to the cluster in Postgres, so a role made before CREATE DATABASE logs
// into the new database with its own password and is the same role there: the setup
// of Zitadel and most tools (CREATE USER, CREATE DATABASE, GRANT, then connect as the
// user) depends on it.
#[test]
fn a_new_database_has_the_roles_there_were() {
    let addr = start_cluster_env(8, Env::password("admin-pw"));
    let admin = connect_config(
        &addr,
        &Config::new()
            .user("postgres", "admin-pw")
            .database(ENTRY_DATABASE)
            .simple(),
    )
    .unwrap();
    must_exec(&admin, "CREATE USER app WITH PASSWORD 'app-pw'");
    must_exec(&admin, "CREATE ROLE readers");
    must_exec(&admin, "GRANT readers TO app");
    must_exec(&admin, "CREATE DATABASE appdb");
    must_exec(&admin, r#"GRANT ALL ON DATABASE appdb TO app"#);

    let app = connect_config(&addr, &Config::new().user("app", "app-pw").database("appdb").simple())
        .expect("the role's own password opens the new database");
    let who: String = app.query_row("SELECT current_user", &[]).scan().unwrap();
    assert_eq!(who, "app");
    let roles: String = app
        .query_row(
            "SELECT string_agg(rolname || ':' || CASE WHEN rolcanlogin THEN 'login' ELSE 'nologin' END, ',' ORDER BY rolname) FROM pg_roles WHERE rolname IN ('app', 'readers')",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(roles, "app:login,readers:nologin");
    let member: String = app
        .query_row(
            "SELECT CASE WHEN pg_has_role('app', 'readers', 'MEMBER') THEN 'member' ELSE 'not' END",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(member, "member", "the membership comes along with the roles");
    connect_config(&addr, &Config::new().user("app", "admin-pw").database("appdb").simple())
        .expect_err("the global password is not the role's");
}
