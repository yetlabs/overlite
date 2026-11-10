use crate::common::*;

const UUID_V7: &str = r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";

// Postgres 18's functions an application may send: uuidv7() above all, as a key that
// sorts by when it was made. Each answer here is Postgres 18's.
#[test]
fn postgres_18_functions() {
    let conn = connect(&start_server());
    assert_regexp(UUID_V7, &scan_str(&conn, "SELECT uuidv7()::text"));
    assert_regexp(r"^[0-9a-f]{8}-[0-9a-f]{4}-4", &scan_str(&conn, "SELECT uuidv4()::text"));
    for (q, want) in [
        ("SELECT uuid_extract_version(uuidv7())", "7"),
        ("SELECT uuid_extract_version(uuidv4())", "4"),
        (
            "SELECT uuid_extract_timestamp('019a6bd4-3c6b-7a5e-8f3b-1c2d3e4f5a6b'::uuid)",
            "2025-11-10 03:34:22.571+00",
        ),
        (
            "SELECT uuid_extract_timestamp('C232AB00-9414-11EC-B3C8-9F6BDECED846'::uuid)",
            "2022-02-22 19:22:22+00",
        ),
        (
            "SELECT uuid_extract_timestamp(uuidv7('-1 day')) < now() - interval '23 hours'",
            "t",
        ),
        ("SELECT casefold('HeLLo')", "hello"),
        ("SELECT crc32('abc')", "891568578"),
        ("SELECT crc32c('abc')", "910901175"),
    ] {
        assert_eq!(scan_str(&conn, q), want, "{q}");
    }
    let none: Option<String> = conn
        .query_row(
            "SELECT uuid_extract_version('00000000-0000-0000-0000-000000000000')::text",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(none, None, "the nil UUID has no RFC 9562 variant");

    // The result columns say uuid and timestamptz, as Postgres' do.
    let rows = conn
        .query("SELECT uuidv7(), uuid_extract_timestamp(uuidv7())", &[])
        .unwrap();
    let oids: Vec<u32> = rows.fields.iter().map(|f| f.data_type_oid).collect();
    assert_eq!(oids, [2950, 1184]);

    // A default of uuidv7() keys rows in the order they were made.
    must_exec(&conn, "CREATE TABLE ev (id uuid PRIMARY KEY DEFAULT uuidv7(), n int)");
    for n in 1..=20 {
        must_exec(&conn, &format!("INSERT INTO ev (n) VALUES ({n})"));
    }
    let order = scan_str(&conn, "SELECT string_agg(n::text, ',' ORDER BY id) FROM ev");
    let want: Vec<String> = (1..=20).map(|n| n.to_string()).collect();
    assert_eq!(order, want.join(","));
}

// A generated column without STORED is virtual in Postgres 18, computed when read, as
// SQLite's are: pg_attribute says so, with 'v', so psql and pg_dump 18 write it back
// without STORED.
#[test]
fn virtual_generated_columns() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE gv (a int, b int GENERATED ALWAYS AS (a * 2), c int GENERATED ALWAYS AS (a * 3) VIRTUAL, d int GENERATED ALWAYS AS (a * 4) STORED)",
    );
    must_exec(&conn, "INSERT INTO gv (a) VALUES (5)");
    assert_eq!(scan_str(&conn, "SELECT b || ',' || c || ',' || d FROM gv"), "10,15,20");
    assert_eq!(
        scan_str(
            &conn,
            "SELECT string_agg(attname || '=' || attgenerated, ' ' ORDER BY attnum) FROM pg_attribute WHERE attrelid = 'gv'::regclass AND attnum > 1"
        ),
        "b=v c=v d=s"
    );
}

// A table's storage parameters (CREATE TABLE ... WITH (...)) are checked as Postgres'
// heap checks them and kept in pg_class.reloptions, which pg_dump writes back; an
// index's too. pgbench's tables are made so.
#[test]
fn storage_parameters() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE tellers (tid int NOT NULL, filler char(84)) WITH (fillfactor=100, autovacuum_enabled = false)",
    );
    must_exec(&conn, "CREATE TABLE plain (a int) WITH (oids = false)");
    must_exec(
        &conn,
        "CREATE INDEX tellers_tid ON tellers (tid) WITH (fillfactor = 70)",
    );
    must_exec(&conn, "INSERT INTO tellers VALUES (1, 'x')");
    for (q, want) in [
        (
            "SELECT reloptions FROM pg_class WHERE relname = 'tellers'",
            "{fillfactor=100,autovacuum_enabled=false}",
        ),
        (
            "SELECT reloptions FROM pg_class WHERE relname = 'tellers_tid'",
            "{fillfactor=70}",
        ),
        (
            "SELECT coalesce(reloptions::text, '-') FROM pg_class WHERE relname = 'plain'",
            "-",
        ),
        (
            "SELECT array_to_string(reloptions, ', ') FROM pg_class WHERE relname = 'tellers'",
            "fillfactor=100, autovacuum_enabled=false",
        ),
        ("SELECT count(*) FROM tellers", "1"),
    ] {
        assert_eq!(scan_str(&conn, q), want, "{q}");
    }
    for (sql, code) in [
        ("CREATE TABLE bad (a int) WITH (fillfactor = 5)", "22023"),
        ("CREATE TABLE bad (a int) WITH (fillfactor = 'x')", "22023"),
        ("CREATE TABLE bad (a int) WITH (nope = 1)", "22023"),
        ("CREATE TABLE bad (a int) WITH (autovacuum_enabled = maybe)", "22023"),
        ("CREATE TABLE bad (a int) WITH (oids = true)", "0A000"),
    ] {
        let err = conn.exec(sql, &[]).expect_err(sql);
        assert_eq!(err.code(), code, "{sql}");
    }
    // pgbench asks for a table's kind with the name as a parameter, and VACUUMs it.
    let kind: String = conn
        .query_row(
            "SELECT relkind FROM pg_catalog.pg_class WHERE oid = $1::pg_catalog.regclass",
            &[&"tellers"],
        )
        .scan()
        .unwrap();
    assert_eq!(kind, "r");
    must_exec(&conn, "VACUUM ANALYZE tellers");
    must_exec(&conn, "VACUUM tellers, plain");
}

// pgbench 18 -i makes its tables (WITH (fillfactor=100)), fills them, adds their keys
// and foreign keys, and a short run of each built-in script then answers.
#[test]
fn pgbench_initializes_and_runs() {
    let Some(bin) = look_path("pgbench") else {
        skip_test!("pgbench not installed");
    };
    let addr = start_server();
    let port = addr.port().to_string();
    let base = ["-h", addr.host(), "-p", port.as_str(), "-U", "postgres"];
    let run = |args: &[&str]| {
        let out = std::process::Command::new(&bin)
            .args(base)
            .args(args)
            .arg("test")
            .output()
            .expect("pgbench must start");
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "pgbench {args:?} failed:\n{text}");
        assert!(!text.contains("error"), "pgbench {args:?}:\n{text}");
        text
    };
    run(&["-i", "-s", "1", "-I", "dtgvpf"]);
    let conn = connect(&addr);
    for (q, want) in [
        ("SELECT count(*) FROM pgbench_accounts", "100000"),
        ("SELECT count(*) FROM pgbench_tellers", "10"),
        (
            "SELECT reloptions FROM pg_class WHERE relname = 'pgbench_accounts'",
            "{fillfactor=100}",
        ),
        (
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'pgbench_history_aid_fkey'",
            "FOREIGN KEY (aid) REFERENCES pgbench_accounts(aid)",
        ),
    ] {
        assert_eq!(scan_str(&conn, q), want, "{q}");
    }
    for script in [&["-b", "tpcb-like"][..], &["-S"], &["-N"], &["-S", "-M", "prepared"]] {
        let mut args = vec!["-n", "-c", "2", "-t", "50"];
        args.extend_from_slice(script);
        let text = run(&args);
        assert!(text.contains("number of failed transactions: 0"), "{text}");
    }
    assert_eq!(scan_str(&conn, "SELECT count(*) FROM pgbench_history"), "200");
}

/// The major version of a client binary ("psql (PostgreSQL) 18.1" is 18).
fn client_major(bin: &std::path::Path) -> u32 {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .expect("the client must start");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .find_map(|w| w.split('.').next()?.parse().ok())
        .unwrap_or(0)
}

// psql's meta-commands that read what the catalogs did not have: \dx (a function in
// FROM with a column list, pg_available_extensions), \dd (pg_opfamily_is_visible),
// \dconfig (pg_settings), \dp (pg_class.relacl from the grants) and \da (the
// aggregates of pg_catalog). Each answer is Postgres 18's, but for the version.
#[test]
fn psql_meta_commands() {
    let Some(psql) = look_path("psql") else {
        skip_test!("psql not installed");
    };
    let major = client_major(&psql);
    let addr = start_server();
    let conn = connect(&addr);
    for sql in [
        "CREATE ROLE reader",
        "CREATE TABLE t (id int PRIMARY KEY, v text)",
        "GRANT SELECT ON t TO reader",
        "GRANT INSERT, UPDATE ON t TO PUBLIC",
        "CREATE TABLE u (a int)",
    ] {
        must_exec(&conn, sql);
    }
    let port = addr.port().to_string();
    let run = |meta: &str| {
        let out = std::process::Command::new(&psql)
            .args([
                "-X",
                "-A",
                "-t",
                "-v",
                "ON_ERROR_STOP=1",
                "-h",
                addr.host(),
                "-p",
                &port,
                "-U",
                "postgres",
            ])
            .args(["-d", "test", "-c", meta])
            .output()
            .expect("psql must start");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{meta}: {}", String::from_utf8_lossy(&out.stderr));
        text
    };
    let dx = if major >= 18 {
        "plpgsql|1.0|1.0|pg_catalog|PL/pgSQL procedural language\n"
    } else {
        "plpgsql|1.0|pg_catalog|PL/pgSQL procedural language\n"
    };
    assert_eq!(run("\\dx"), dx);
    assert_eq!(run("\\dd"), "");
    let config = run("\\dconfig server_version*");
    let names: Vec<&str> = config.lines().filter_map(|l| l.split('|').next()).collect();
    assert_eq!(names, ["server_version", "server_version_num"], "{config}");
    // The owner, as relowner names it.
    let o = scan_str(
        &conn,
        "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname = 't'",
    );
    assert_eq!(
        run("\\dp t"),
        format!("public|t|table|{o}=arwdDxtm/{o}\nreader=r/{o}\n=aw/{o}||\n")
    );
    assert_eq!(run("\\dp u"), "public|u|table|||\n");
    assert_eq!(
        run("\\da count"),
        "pg_catalog|count|bigint|\"any\"|number of input rows for which the input expression is not null\n\
         pg_catalog|count|bigint|*|number of input rows\n"
    );
    assert!(
        run("\\daS avg").contains(
            "pg_catalog|avg|interval|interval|the average (arithmetic mean) as interval of all interval values"
        )
    );
    // pg_dump writes the GRANTs back now that relacl holds them.
    assert_eq!(
        scan_str(&conn, "SELECT relacl FROM pg_class WHERE relname = 't'"),
        format!("{{{o}=arwdDxtm/{o},reader=r/{o},=aw/{o}}}")
    );
    assert_eq!(
        scan_str(&conn, "SELECT acldefault('r', 10)"),
        format!("{{{o}=arwdDxtm/{o}}}")
    );
}
