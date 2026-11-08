//! Statements Django's test suite sends that overlite refused or answered wrongly,
//! each held to what a postgres:17 answers.

use crate::common::*;

// An array type's declared sizes are taken and ignored, as Postgres does; a
// subscript in a CHECK keeps its meaning.
#[test]
fn array_column_sizes() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE ra (id int, f double precision[2], g int[3][3], h varchar(10)[2] CHECK (h[1] <> 'x'))",
    );
    must_exec(&conn, "INSERT INTO ra VALUES (1, '{1.5,2}', '{{1,2},{3,4}}', '{a,b}')");
    assert_eq!(scan_str(&conn, "SELECT f FROM ra"), "{1.5,2}");
    assert_eq!(
        scan_str(
            &conn,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'ra'::regclass AND attname = 'f'"
        ),
        "double precision[]"
    );
    assert_sqlstate(
        &conn
            .exec("INSERT INTO ra VALUES (2, '{1}', '{}', '{x}')", &[])
            .unwrap_err(),
        "23514",
    );
    must_exec(&conn, "ALTER TABLE ra ADD COLUMN k text[4]");
    must_exec(&conn, "ALTER TABLE ra ALTER COLUMN k TYPE varchar(5)[3]");
}

// A UNIQUE constraint takes DEFERRABLE and INITIALLY in CREATE TABLE and in ALTER
// TABLE ADD CONSTRAINT, and pg_constraint reports them.
#[test]
fn deferrable_unique_constraints() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE rb (id int, name text, code text, CONSTRAINT rb_u UNIQUE (name) DEFERRABLE INITIALLY DEFERRED)",
    );
    must_exec(
        &conn,
        "ALTER TABLE rb ADD CONSTRAINT rb_c UNIQUE (code) DEFERRABLE INITIALLY IMMEDIATE",
    );
    let rows: Vec<(String, String, bool, bool)> = conn
        .query(
            "SELECT conname, contype, condeferrable, condeferred FROM pg_constraint \
             WHERE conrelid = 'rb'::regclass ORDER BY conname",
            &[],
        )
        .unwrap()
        .collect_rows()
        .unwrap();
    let want = [("rb_c", "u", true, false), ("rb_u", "u", true, true)].map(|(a, b, c, d)| (a.into(), b.into(), c, d));
    assert_eq!(rows, want);
    // The key still holds outside a transaction.
    must_exec(&conn, "INSERT INTO rb VALUES (1, 'a', 'x')");
    assert_sqlstate(
        &conn.exec("INSERT INTO rb VALUES (2, 'b', 'x')", &[]).unwrap_err(),
        "23505",
    );
}

// A covering index: the key alone is unique, and the catalogs list the included
// columns after the key, as Django's introspection reads them.
#[test]
fn covering_index_include() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        r#"CREATE TABLE rc (id int PRIMARY KEY, pink int, weight float8, "Color" text)"#,
    );
    must_exec(
        &conn,
        r#"CREATE UNIQUE INDEX rc_i ON rc (pink) INCLUDE (weight, "Color")"#,
    );
    must_exec(&conn, "CREATE INDEX rc_p ON rc (weight) INCLUDE (pink) WHERE pink > 0");
    must_exec(&conn, "INSERT INTO rc VALUES (1, 1, 1.0, 'a'), (2, 2, 1.0, 'a')");
    assert_sqlstate(
        &conn.exec("INSERT INTO rc VALUES (3, 1, 2.0, 'b')", &[]).unwrap_err(),
        "23505",
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'rc' ORDER BY indexname",
            0
        ),
        [
            r#"CREATE UNIQUE INDEX rc_i ON public.rc USING btree (pink) INCLUDE (weight, "Color")"#,
            "CREATE INDEX rc_p ON public.rc USING btree (weight) INCLUDE (pink) WHERE (pink > 0)",
            "CREATE UNIQUE INDEX rc_pkey ON public.rc USING btree (id)",
        ]
    );
    assert_eq!(
        scan_str(&conn, "SELECT pg_get_indexdef('rc_i'::regclass)"),
        r#"CREATE UNIQUE INDEX rc_i ON public.rc USING btree (pink) INCLUDE (weight, "Color")"#
    );
    let natts: Vec<(String, i64)> = conn
        .query(
            "SELECT c2.relname, i.indnatts FROM pg_index i JOIN pg_class c2 ON c2.oid = i.indexrelid \
             WHERE i.indrelid = 'rc'::regclass ORDER BY 1",
            &[],
        )
        .unwrap()
        .collect_rows()
        .unwrap();
    assert_eq!(natts, [("rc_i".into(), 3), ("rc_p".into(), 2), ("rc_pkey".into(), 1)]);
    // Django's get_constraints.
    let cols = query_column(
        &conn,
        "SELECT indexname, array_agg(attname ORDER BY arridx)::text FROM (SELECT c2.relname AS indexname, \
         attr.attname, idx.arridx FROM (SELECT * FROM pg_index i, unnest(i.indkey, i.indoption) WITH ORDINALITY \
         koi(key, option, arridx)) idx LEFT JOIN pg_class c ON idx.indrelid = c.oid LEFT JOIN pg_class c2 ON \
         idx.indexrelid = c2.oid LEFT JOIN pg_attribute attr ON attr.attrelid = c.oid AND attr.attnum = idx.key \
         WHERE c.relname = 'rc') s2 GROUP BY indexname ORDER BY 1",
        1,
    );
    assert_eq!(cols, ["{pink,weight,Color}", "{weight,pink}", "{id}"]);
}

// ADD COLUMN with a key or an identity rebuilds the table with the column in it,
// as SQLite's ADD COLUMN takes neither.
#[test]
fn add_column_keys() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE rw (id int PRIMARY KEY, name text)");
    must_exec(&conn, "INSERT INTO rw VALUES (1, 'a')");
    must_exec(&conn, "ALTER TABLE rw ADD COLUMN slug varchar(20) NULL UNIQUE");
    must_exec(&conn, "INSERT INTO rw VALUES (2, 'b', 'x')");
    assert_sqlstate(
        &conn.exec("INSERT INTO rw VALUES (3, 'c', 'x')", &[]).unwrap_err(),
        "23505",
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT conname FROM pg_constraint WHERE conrelid = 'rw'::regclass AND contype = 'u'",
            0
        ),
        ["rw_slug_key"]
    );
    must_exec(&conn, "CREATE TABLE rw3 (name text)");
    must_exec(&conn, "INSERT INTO rw3 VALUES ('a'), ('b')");
    must_exec(
        &conn,
        "ALTER TABLE rw3 ADD COLUMN id integer NOT NULL PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY",
    );
    assert_eq!(
        query_column(&conn, "SELECT name || id FROM rw3 ORDER BY id", 0),
        ["a1", "b2"]
    );
    assert_eq!(seq_val(&conn, "INSERT INTO rw3 (name) VALUES ('c') RETURNING id"), 3);
    assert_eq!(
        scan_str(
            &conn,
            "SELECT attidentity FROM pg_attribute WHERE attrelid = 'rw3'::regclass AND attname = 'id'"
        ),
        "d"
    );
}

// A stored generated column survives a rebuild, and the catalogs list it.
#[test]
fn generated_columns_in_rebuilds_and_catalogs() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE rg (a int, b int GENERATED ALWAYS AS (a * 2) STORED)",
    );
    must_exec(&conn, "INSERT INTO rg (a) VALUES (3)");
    must_exec(&conn, "ALTER TABLE rg ADD COLUMN c text NULL DEFAULT lower('X')");
    must_exec(&conn, "INSERT INTO rg (a) VALUES (4)");
    assert_eq!(
        query_column(&conn, "SELECT a || ' ' || b || ' ' || c FROM rg ORDER BY a", 0),
        ["3 6 x", "4 8 x"]
    );
    must_exec(&conn, "COMMENT ON COLUMN rg.b IS 'gen'");
    assert_eq!(scan_str(&conn, "SELECT col_description('rg'::regclass, 2)"), "gen");
    assert_eq!(
        scan_str(
            &conn,
            "SELECT attgenerated FROM pg_attribute WHERE attrelid = 'rg'::regclass AND attname = 'b'"
        ),
        "s"
    );
}

// NULLS NOT DISTINCT: two NULLs are one key, on an index and on a constraint.
#[test]
fn unique_nulls_not_distinct() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE rn (h int, w int, name text)");
    must_exec(
        &conn,
        "ALTER TABLE rn ADD CONSTRAINT rn_u UNIQUE NULLS NOT DISTINCT (h, w)",
    );
    must_exec(
        &conn,
        "INSERT INTO rn VALUES (NULL, NULL, 'a'), (1, NULL, 'b'), (NULL, 1, 'c')",
    );
    for dup in ["(NULL, NULL, 'd')", "(1, NULL, 'e')"] {
        assert_sqlstate(
            &conn.exec(&format!("INSERT INTO rn VALUES {dup}"), &[]).unwrap_err(),
            "23505",
        );
    }
    must_exec(
        &conn,
        "CREATE UNIQUE INDEX rn_p ON rn (w) NULLS NOT DISTINCT WHERE h > 4",
    );
    must_exec(&conn, "CREATE UNIQUE INDEX rn_d ON rn (name) NULLS DISTINCT");
    must_exec(&conn, "INSERT INTO rn VALUES (5, NULL, 'x')");
    assert_sqlstate(
        &conn.exec("INSERT INTO rn VALUES (6, NULL, 'y')", &[]).unwrap_err(),
        "23505",
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'rn' ORDER BY 1",
            0
        ),
        [
            "CREATE UNIQUE INDEX rn_d ON public.rn USING btree (name)",
            "CREATE UNIQUE INDEX rn_p ON public.rn USING btree (w) NULLS NOT DISTINCT WHERE (h > 4)",
            "CREATE UNIQUE INDEX rn_u ON public.rn USING btree (h, w) NULLS NOT DISTINCT",
        ]
    );
    must_exec(&conn, "ALTER TABLE rn DROP CONSTRAINT rn_u");
    must_exec(&conn, "INSERT INTO rn VALUES (NULL, NULL, 'z')");
}

// A materialized view holds its query's rows until REFRESH; partitions and foreign
// data are refused, and a tablespace is taken.
#[test]
fn materialized_views_and_other_objects() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE rm (h int, name text)");
    must_exec(&conn, "INSERT INTO rm VALUES (1, 'a')");
    must_exec(&conn, "CREATE MATERIALIZED VIEW rmv AS SELECT h, name FROM rm");
    must_exec(&conn, "INSERT INTO rm VALUES (2, 'b')");
    assert_eq!(seq_val(&conn, "SELECT count(*) FROM rmv"), 1);
    must_exec(&conn, "REFRESH MATERIALIZED VIEW rmv");
    assert_eq!(seq_val(&conn, "SELECT count(*) FROM rmv"), 2);
    assert_eq!(
        scan_str(&conn, "SELECT relkind FROM pg_class WHERE relname = 'rmv'"),
        "m"
    );
    assert_eq!(query_column(&conn, "SELECT matviewname FROM pg_matviews", 0), ["rmv"]);
    assert_eq!(
        query_column(&conn, "SELECT tablename FROM pg_tables WHERE tablename LIKE 'rm%'", 0),
        ["rm"]
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'rmv'::regclass AND attnum > 0 ORDER BY attnum",
            0
        ),
        ["integer", "text"]
    );
    assert_sqlstate(&conn.exec("DROP MATERIALIZED VIEW rmv, rm", &[]).unwrap_err(), "42809");
    assert_eq!(seq_val(&conn, "SELECT count(*) FROM rmv"), 2);
    must_exec(&conn, "DROP MATERIALIZED VIEW rmv");
    must_exec(&conn, "DROP MATERIALIZED VIEW IF EXISTS rmv");
    assert_sqlstate(&conn.exec("SELECT * FROM rmv", &[]).unwrap_err(), "42P01");
    for sql in [
        "CREATE TABLE rp (name text NOT NULL) PARTITION BY LIST (left(upper(name), 1))",
        "CREATE TABLE rp1 PARTITION OF rm FOR VALUES IN ('A')",
        "CREATE SERVER s FOREIGN DATA WRAPPER file_fdw",
        "CREATE FOREIGN TABLE f (a int) SERVER s",
    ] {
        assert_sqlstate(&conn.exec(sql, &[]).unwrap_err(), "0A000");
    }
    must_exec(
        &conn,
        "CREATE TABLE rt (a int, CONSTRAINT rt_u UNIQUE (a) USING INDEX TABLESPACE pg_default) TABLESPACE pg_default",
    );
    must_exec(&conn, "CREATE INDEX rt_i ON rt (a) TABLESPACE pg_default");
}

// The string, hashing, math, network and statistics functions Django calls, each
// held to what postgres:17 answers.
#[test]
fn functions_postgres_has() {
    let conn = connect(&start_server());
    let row = |sql: &str| -> Vec<String> {
        let rows = raw_rows(&conn, sql);
        rows[0]
            .iter()
            .map(|c| {
                c.as_ref()
                    .map_or("NULL".to_string(), |b| String::from_utf8_lossy(b).into_owned())
            })
            .collect()
    };
    assert_eq!(
        row(
            "SELECT LEFT('abc', 1), RIGHT('abc', 2), STRPOS('abc', 'b'), REVERSE('abc'), ASCII('a'), MD5('a'), \
             LPAD('a', 3, 'x'), RPAD('a', 3, 'x'), REPEAT('ab', 2), chr(201), initcap('hello wORLD'), \
             split_part('a,b,c', ',', -1), translate('abcb', 'ab', 'x'), btrim('xxaxx', 'x'), to_hex(255), \
             starts_with('abc', 'a'), left('abc', -1), right('abc', -2)"
        ),
        [
            "a",
            "bc",
            "2",
            "cba",
            "97",
            "0cc175b9c0f1b6a831c399e269772661",
            "xxa",
            "axx",
            "abab",
            "É",
            "Hello World",
            "c",
            "xc",
            "a",
            "ff",
            "t",
            "ab",
            "c"
        ]
    );
    assert_eq!(
        row(
            "SELECT ENCODE(DIGEST('abc', 'sha1'), 'hex'), encode(sha256('abc'), 'hex'), encode('abc'::bytea, 'base64')"
        ),
        [
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "YWJj"
        ]
    );
    assert_eq!(
        row(
            "SELECT round(STDDEV_POP(x)::numeric, 6), round(VAR_SAMP(x)::numeric, 6), regr_count(x, x), \
             bit_and(x::int), bit_or(x::int), bit_xor(x::int) FROM (VALUES (1.0), (2.0), (4.0)) v(x)"
        ),
        ["1.247219", "2.333333", "3", "0", "7", "7"]
    );
    assert_eq!(
        row("SELECT round(COT(1.0)::numeric, 10), sind(30), gcd(12, 18), lcm(4, 6), factorial(5), div(9, 4)"),
        ["0.6420926159", "0.5", "6", "12", "120", "2"]
    );
    assert_eq!(
        row("SELECT STRING_AGG(DISTINCT (x)::varchar, ',') FROM (VALUES (1), (1), (2)) v(x)"),
        ["1,2"]
    );
    assert_eq!(
        row("SELECT host('192.168.1.5/24'::inet), netmask('192.168.1.5/24'::inet), network('192.168.1.5/24'::inet)"),
        ["192.168.1.5", "255.255.255.0", "192.168.1.0/24"]
    );
    assert_eq!(
        row(
            "SELECT regexp_replace('abcabc', '(b)', '[\\1]', 'g'), regexp_match('foobarbequebaz', '(bar)(beque)'), \
             string_to_array('a,b,,c', ',', '')"
        ),
        ["a[b]ca[b]c", "{bar,beque}", "{a,b,NULL,c}"]
    );
    assert_eq!(row("SELECT any_value(x) FROM (VALUES (NULL), (3)) v(x)"), ["3"]);
    assert_sqlstate(&conn.exec("SELECT split_part('a', ',', 0)", &[]).unwrap_err(), "22023");
}

// CREATE EXTENSION takes what this server provides, lists it in pg_extension, and
// refuses any other as Postgres refuses one it has not got.
#[test]
fn extensions_provided_or_refused() {
    let conn = connect(&start_server());
    for ext in [
        "pgcrypto",
        r#""uuid-ossp""#,
        "hstore",
        "citext",
        "unaccent",
        "pg_trgm",
        "btree_gin",
        "btree_gist",
        "bloom",
    ] {
        must_exec(&conn, &format!("CREATE EXTENSION IF NOT EXISTS {ext}"));
    }
    assert_sqlstate(&conn.exec("CREATE EXTENSION pg_trgm", &[]).unwrap_err(), "42710");
    assert_sqlstate(&conn.exec("CREATE EXTENSION postgis", &[]).unwrap_err(), "0A000");
    assert_eq!(
        seq_val(
            &conn,
            "SELECT count(*) FROM pg_extension WHERE extname IN ('pg_trgm', 'plpgsql', 'uuid-ossp')"
        ),
        3
    );
    assert_eq!(
        seq_val(
            &conn,
            "SELECT count(*) FROM pg_type WHERE typname IN ('citext', 'hstore')"
        ),
        2
    );
    must_exec(&conn, "DROP EXTENSION citext");
    assert_eq!(
        seq_val(&conn, "SELECT count(*) FROM pg_type WHERE typname = 'citext'"),
        0
    );
    assert_sqlstate(&conn.exec("DROP EXTENSION citext", &[]).unwrap_err(), "42704");
    must_exec(&conn, "DROP EXTENSION IF EXISTS citext");
    // unaccent and pg_trgm, with the values of their documentation.
    assert_eq!(scan_str(&conn, "SELECT unaccent('Hôtel')"), "Hotel");
    let w: f64 = conn
        .query_row("SELECT word_similarity('word', 'two words')", &[])
        .scan()
        .unwrap();
    assert!((w - 0.8).abs() < 1e-6);
    let s: f64 = conn
        .query_row("SELECT strict_word_similarity('word', 'two words')", &[])
        .scan()
        .unwrap();
    assert!((s - 0.571_428_571).abs() < 1e-6);
    must_exec(&conn, "CREATE TABLE tg (name text)");
    must_exec(
        &conn,
        "INSERT INTO tg VALUES ('Matthew'), ('Cat sat on mat'), ('Dog sat on rug')",
    );
    assert_eq!(
        query_column(&conn, "SELECT name FROM tg WHERE name % 'Mathew'", 0),
        ["Matthew"]
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT name FROM tg WHERE 'Cat' <% name ORDER BY name <-> 'cat' LIMIT 1",
            0
        ),
        ["Cat sat on mat"]
    );
    assert_eq!(seq_val(&conn, "SELECT 7 % 3"), 1);
    // citext compares without case.
    must_exec(&conn, "CREATE EXTENSION citext");
    must_exec(&conn, "CREATE TABLE ci (e citext)");
    must_exec(&conn, "INSERT INTO ci VALUES ('Alice@Example.com')");
    assert_eq!(
        seq_val(&conn, "SELECT count(*) FROM ci WHERE e = 'alice@example.COM'"),
        1
    );
}

// Postgres' math operators: # (xor), ^ (power), |/ (square root), ||/ (cube root)
// and @ (absolute value).
#[test]
fn math_operators() {
    let conn = connect(&start_server());
    let rows = raw_rows(
        &conn,
        "SELECT 5 # 3, 2 ^ 3, 2 ^ 3 ^ 2, |/ 25, ||/ 27, @ -5, (2 * 3 ^ 2)",
    );
    let got: Vec<String> = rows[0]
        .iter()
        .map(|c| String::from_utf8_lossy(c.as_ref().unwrap()).into_owned())
        .collect();
    assert_eq!(got, ["6", "8", "64", "5", "3", "5", "18"]);
    must_exec(&conn, "CREATE TABLE ro (a int)");
    must_exec(&conn, "INSERT INTO ro VALUES (42), (NULL)");
    assert_eq!(
        query_column(&conn, "SELECT (\"ro\".\"a\" # 23)::text FROM ro ORDER BY 1", 0),
        ["61"]
    );
}

// DISTINCT ON with qualified names, an output name in ORDER BY, and in a subquery.
#[test]
fn distinct_on_qualified_and_nested() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE rn (id int, name text)");
    must_exec(&conn, "INSERT INTO rn VALUES (1, 'a'), (2, 'a'), (3, 'b')");
    let rows: Vec<(i64, String)> = conn
        .query(
            r#"SELECT DISTINCT ON ("rn"."name") "rn"."id", "rn"."name" FROM "rn" ORDER BY "rn"."name", "rn"."id""#,
            &[],
        )
        .unwrap()
        .collect_rows()
        .unwrap();
    assert_eq!(rows, [(1, "a".into()), (3, "b".into())]);
    let fields: Vec<String> = conn
        .query(r#"SELECT DISTINCT ON ("rn"."name") "rn"."id", upper(name) AS up FROM "rn" ORDER BY "rn"."name", "rn"."id" DESC"#, &[])
        .unwrap()
        .field_descriptions()
        .iter()
        .map(|f| f.name.clone())
        .collect();
    assert_eq!(fields, ["id", "up"]);
    assert_eq!(
        query_column(
            &conn,
            "SELECT id::text FROM rn WHERE id IN (SELECT DISTINCT ON (name) id FROM rn ORDER BY name, id DESC) ORDER BY id",
            0
        ),
        ["2", "3"]
    );
    assert_eq!(
        query_column(
            &conn,
            "SELECT DISTINCT ON (2) id::text, name FROM rn ORDER BY 2, 1 DESC",
            0
        ),
        ["2", "3"]
    );
}

// The server says it is Postgres 16, whose ANY_VALUE it has, so Django uses it.
#[test]
fn server_version_16_and_any_value() {
    let conn = connect(&start_server());
    assert_eq!(scan_str(&conn, "SHOW server_version_num"), "160000");
    assert_eq!(
        scan_str(&conn, "SELECT current_setting('server_version_num')"),
        "160000"
    );
    must_exec(&conn, "CREATE TABLE av (g int, v text)");
    must_exec(&conn, "INSERT INTO av VALUES (1, NULL), (1, 'x'), (2, 'y')");
    assert_eq!(
        query_column(&conn, "SELECT ANY_VALUE(v) FROM av GROUP BY g ORDER BY g", 0),
        ["x", "y"]
    );
}

// A write from another connection while one holds a write transaction waits for it,
// as an INSERT waits for a conflicting row lock on Postgres, whatever that
// connection ran before.
#[test]
fn writer_waits_for_open_write_transaction() {
    let addr = start_server();
    let a = connect(&addr);
    let b = connect_extended(&addr);
    must_exec(&a, "CREATE TABLE ru (id int)");
    b.query("SELECT set_config($1, $2, false)", &[&"TimeZone", &"UTC"])
        .unwrap();
    must_exec(&a, "BEGIN");
    must_exec(&a, "INSERT INTO ru VALUES (1)");
    let n: i64 = b
        .query_row("SELECT count(*) FROM ru WHERE id > $1", &[&0i64])
        .scan()
        .unwrap();
    assert_eq!(n, 0);
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        must_exec(&a, "COMMIT");
    });
    b.exec("INSERT INTO ru VALUES ($1)", &[&2i64]).unwrap();
    t.join().unwrap();
    assert_eq!(seq_val(&b, "SELECT count(*) FROM ru"), 2);
}

// A transaction that has not written holds no lock: another's BEGIN and reads go on.
// FOR UPDATE takes the write lock; NOWAIT fails at once with 55P03, SKIP LOCKED
// returns no row, and OF takes a list of tables.
#[test]
fn row_locks() {
    let addr = start_server();
    let a = connect(&addr);
    let b = connect(&addr);
    must_exec(&a, "CREATE TABLE ru (id int)");
    must_exec(&a, "CREATE TABLE rv (id int, u int)");
    must_exec(&a, "INSERT INTO ru VALUES (5), (6)");
    // a writes in an open transaction; b's transaction can still begin and read.
    must_exec(&a, "BEGIN");
    must_exec(&a, "INSERT INTO rv VALUES (1, 5)");
    must_exec(&b, "BEGIN");
    assert_eq!(seq_val(&b, "SELECT count(*) FROM ru"), 2);
    let e = b
        .exec("SELECT * FROM ru WHERE id = 5 FOR UPDATE NOWAIT", &[])
        .unwrap_err();
    assert_pg_error(&e, "55P03", r#"could not obtain lock on row in relation "ru""#);
    must_exec(&b, "ROLLBACK");
    must_exec(&b, "BEGIN");
    assert_eq!(
        query_column(&b, "SELECT id::text FROM ru FOR UPDATE SKIP LOCKED", 0),
        Vec::<String>::new()
    );
    must_exec(&b, "ROLLBACK");
    must_exec(&a, "COMMIT");
    // With the lock free, the lock is had, and OF names several tables.
    must_exec(&b, "BEGIN");
    assert_eq!(
        query_column(
            &b,
            r#"SELECT "ru"."id"::text FROM "ru" INNER JOIN "rv" ON ("rv"."u" = "ru"."id") FOR UPDATE OF "ru", "rv" NOWAIT"#,
            0
        ),
        ["5"]
    );
    // b holds it now: a waits for it, and goes on when b commits.
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        must_exec(&b, "COMMIT");
    });
    must_exec(&a, "INSERT INTO ru VALUES (7)");
    t.join().unwrap();
    assert_eq!(seq_val(&a, "SELECT count(*) FROM ru"), 3);
}

// EXPLAIN with options answers a plan in Postgres' shape, in each format.
#[test]
fn explain_formats() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE ex (id int PRIMARY KEY, a text)");
    must_exec(&conn, "INSERT INTO ex VALUES (1, 'x')");
    let plan = query_column(&conn, "EXPLAIN SELECT * FROM ex WHERE a = 'x'", 0);
    assert_eq!(plan, ["Seq Scan on ex  (cost=0.00..0.00 rows=0 width=0)"]);
    let plan = query_column(&conn, "EXPLAIN (COSTS OFF) SELECT * FROM ex ORDER BY a", 0);
    assert_eq!(plan, ["Sort", "  ->  Seq Scan on ex"]);
    let rows = conn
        .query("EXPLAIN (FORMAT JSON) SELECT * FROM ex WHERE id = 1", &[])
        .unwrap();
    assert_eq!(rows.field_descriptions()[0].name, "QUERY PLAN");
    let json: serde_json::Value = serde_json::from_str(&rows.iter().next().unwrap().get::<String>(0).unwrap()).unwrap();
    assert_eq!(json[0]["Plan"]["Node Type"], "Index Scan");
    assert_eq!(json[0]["Plan"]["Relation Name"], "ex");
    let yaml = scan_str(&conn, "EXPLAIN (FORMAT YAML) SELECT 1");
    assert!(yaml.starts_with("- Plan: \n    Node Type: \"Result\""), "{yaml}");
    let xml = scan_str(&conn, "EXPLAIN (FORMAT XML, COSTS false) SELECT 1");
    assert!(xml.contains("<Node-Type>Result</Node-Type>"), "{xml}");
    let analyzed = query_column(&conn, "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) SELECT * FROM ex", 0);
    assert_eq!(analyzed[0], "Seq Scan on ex (actual rows=1 loops=1)");
    assert!(analyzed[1].starts_with("Planning Time: "));
    assert_sqlstate(&conn.exec("EXPLAIN (FORMAT CSV) SELECT 1", &[]).unwrap_err(), "22023");
}

// DROP TABLE takes a list.
#[test]
fn drop_lists() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE rd1 (id int)");
    must_exec(&conn, "CREATE TABLE rd2 (id int)");
    must_exec(&conn, "DROP TABLE rd1, rd2 CASCADE");
    assert_eq!(
        seq_val(&conn, "SELECT count(*) FROM pg_class WHERE relname IN ('rd1', 'rd2')"),
        0
    );
}

// ON CONFLICT without a key that matches its columns is Postgres' 42P10; one whose
// columns a key has in another order is that key.
#[test]
fn on_conflict_targets() {
    let conn = connect(&start_server());
    must_exec(
        &conn,
        "CREATE TABLE rtf (id int, f1 int UNIQUE, f2 int UNIQUE, name text)",
    );
    must_exec(&conn, "INSERT INTO rtf VALUES (1, 1, 1, 'a')");
    let e = conn
        .exec(
            "INSERT INTO rtf (id, f1, f2, name) VALUES (2, 1, 1, 'c') ON CONFLICT (f1, f2) DO UPDATE SET name = EXCLUDED.name",
            &[],
        )
        .unwrap_err();
    assert_sqlstate(&e, "42P10");
    must_exec(
        &conn,
        "CREATE TABLE rco (id int, name text, iso text, CONSTRAINT rco_u UNIQUE (iso, name))",
    );
    must_exec(&conn, "INSERT INTO rco VALUES (1, 'Germany', 'DE')");
    assert_eq!(
        seq_val(
            &conn,
            "INSERT INTO rco (id, name, iso) VALUES (2, 'Germany', 'DE') ON CONFLICT (name, iso) DO UPDATE SET id = EXCLUDED.id RETURNING id"
        ),
        2
    );
}

// An index column's operator class is in pg_index.indclass, as Django reads it.
#[test]
fn index_operator_classes() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE oc (a varchar(10), b text)");
    must_exec(&conn, "CREATE INDEX oc_i ON oc (a varchar_pattern_ops, b DESC)");
    assert_eq!(
        query_column(
            &conn,
            "SELECT opcname FROM pg_opclass AS oc JOIN pg_index AS i ON oc.oid = ANY(i.indclass) \
             JOIN pg_class AS c ON c.oid = i.indexrelid WHERE c.relname = 'oc_i' ORDER BY 1",
            0
        ),
        ["text_ops", "varchar_pattern_ops"]
    );
}

// CREATE/DROP INDEX CONCURRENTLY and storage parameters are taken, and an index's
// access method is reported though every index is a btree here.
#[test]
fn index_methods_and_options() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE im (a int, tags text[])");
    must_exec(
        &conn,
        r#"CREATE INDEX CONCURRENTLY "im_g" ON "im" USING gin ("tags") WITH (fastupdate = off)"#,
    );
    must_exec(
        &conn,
        "CREATE INDEX im_b ON im USING bloom (a) WITH (length = 80, col1 = 4)",
    );
    must_exec(&conn, "CREATE INDEX im_f ON im (a) WITH (fillfactor = 70)");
    assert_eq!(
        query_column(
            &conn,
            "SELECT c.relname || ' ' || am.amname FROM pg_class c JOIN pg_am am ON am.oid = c.relam \
             WHERE c.relname LIKE 'im_%' ORDER BY 1",
            0
        ),
        ["im_b bloom", "im_f btree", "im_g gin"]
    );
    assert_eq!(
        scan_str(&conn, "SELECT indexdef FROM pg_indexes WHERE indexname = 'im_g'"),
        "CREATE INDEX im_g ON public.im USING gin (tags)"
    );
    must_exec(&conn, r#"DROP INDEX CONCURRENTLY IF EXISTS "im_g""#);
    must_exec(&conn, "ALTER TABLE im ADD CONSTRAINT im_c CHECK (a > 0) NOT VALID");
    must_exec(&conn, "ALTER TABLE im VALIDATE CONSTRAINT im_c");
}

// Language collations order as their language does, CREATE COLLATION makes a
// case-insensitive one, and the catalogs report a column's collation.
#[test]
fn collations() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE co (alias text, name text COLLATE \"sv-x-icu\")");
    must_exec(
        &conn,
        "INSERT INTO co VALUES ('a', 'x'), ('B', 'x'), ('O', 'x'), ('Ö', 'x'), ('P', 'x')",
    );
    assert_eq!(
        query_column(&conn, r#"SELECT alias FROM co ORDER BY alias COLLATE "sv-x-icu""#, 0),
        ["a", "B", "O", "P", "Ö"]
    );
    assert_eq!(
        query_column(&conn, r#"SELECT alias FROM co ORDER BY alias COLLATE "en-x-icu""#, 0),
        ["a", "B", "O", "Ö", "P"]
    );
    must_exec(
        &conn,
        "CREATE COLLATION IF NOT EXISTS case_insensitive (provider = icu, locale = 'und-u-ks-level2', deterministic = false)",
    );
    must_exec(
        &conn,
        "CREATE TABLE ci2 (name varchar(20) COLLATE case_insensitive UNIQUE)",
    );
    must_exec(&conn, "INSERT INTO ci2 VALUES ('ANDREW')");
    assert_eq!(seq_val(&conn, "SELECT count(*) FROM ci2 WHERE name = 'Andrew'"), 1);
    assert_sqlstate(
        &conn.exec("INSERT INTO ci2 VALUES ('andrew')", &[]).unwrap_err(),
        "23505",
    );
    let rows: Vec<(String, bool)> = conn
        .query(
            "SELECT collname, collisdeterministic FROM pg_collation WHERE collname IN ('case_insensitive', 'sv-x-icu') ORDER BY 1",
            &[],
        )
        .unwrap()
        .collect_rows()
        .unwrap();
    assert_eq!(rows, [("case_insensitive".into(), false), ("sv-x-icu".into(), true)]);
    // Django's get_table_description.
    assert_eq!(
        query_column(
            &conn,
            "SELECT a.attname || ':' || COALESCE(CASE WHEN collname = 'default' THEN NULL ELSE collname END, '-') \
             FROM pg_attribute a LEFT JOIN pg_collation co ON a.attcollation = co.oid \
             JOIN pg_class c ON a.attrelid = c.oid WHERE c.relname = 'co' AND a.attnum > 0 ORDER BY a.attnum",
            0
        ),
        ["alias:-", "name:sv-x-icu"]
    );
    assert_sqlstate(
        &conn
            .exec(
                "CREATE COLLATION case_insensitive (provider = icu, locale = 'und')",
                &[],
            )
            .unwrap_err(),
        "42710",
    );
    must_exec(&conn, "DROP COLLATION IF EXISTS nope");
    must_exec(&conn, "DROP TABLE ci2");
    must_exec(&conn, "DROP COLLATION case_insensitive");
    assert_sqlstate(&conn.exec("DROP COLLATION case_insensitive", &[]).unwrap_err(), "42704");
}

// A SERIAL column that is not the key numbers its rows from a sequence of its own.
#[test]
fn serial_column_not_key() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE testing (serial_field SERIAL, name text)");
    must_exec(&conn, "INSERT INTO testing (name) VALUES ('a'), ('b')");
    assert_eq!(
        query_column(&conn, "SELECT serial_field::text FROM testing ORDER BY 1", 0),
        ["1", "2"]
    );
    assert_eq!(
        scan_str(&conn, "SELECT pg_get_serial_sequence('testing', 'serial_field')"),
        "public.testing_serial_field_seq"
    );
    assert_eq!(seq_val(&conn, "SELECT nextval('testing_serial_field_seq')"), 3);
}

// Django's get_sequences over that column.
#[test]
fn serial_column_not_key_owned() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE testing (serial_field SERIAL)");
    assert_eq!(
        query_column(
            &conn,
            "SELECT s.relname || ' ' || a.attname FROM pg_class s JOIN pg_depend d ON d.objid = s.oid \
             AND d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass \
             JOIN pg_attribute a ON d.refobjid = a.attrelid AND d.refobjsubid = a.attnum \
             JOIN pg_class tbl ON tbl.oid = d.refobjid AND tbl.relname = 'testing' \
             WHERE s.relkind = 'S'",
            0
        ),
        ["testing_serial_field_seq serial_field"]
    );
}

// Django on Postgres 16 writes the SQL/JSON constructors.
#[test]
fn sql_json_constructors() {
    let conn = connect(&start_server());
    assert_eq!(
        scan_str(
            &conn,
            "SELECT JSON_ARRAY(('a')::text, (NULL)::integer NULL ON NULL RETURNING JSONB)::text"
        ),
        r#"["a",null]"#
    );
    assert_eq!(scan_str(&conn, "SELECT JSON_ARRAY(1, NULL, 2)::text"), "[1,2]");
    assert_eq!(
        scan_str(
            &conn,
            "SELECT JSON_OBJECT(('k')::text VALUE 1, 'n' : JSON_ARRAY( RETURNING JSONB) RETURNING JSONB)::text"
        ),
        r#"{"k":1,"n":[]}"#
    );
}

// An index on an expression lists 0 for it in indkey, as Postgres does.
#[test]
fn expression_index_keys() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE ek (a int, t text, d date)");
    must_exec(&conn, "CREATE INDEX ek_i ON ek (a DESC, lower(t), d)");
    assert_eq!(
        scan_str(
            &conn,
            "SELECT indkey::text FROM pg_index WHERE indexrelid = 'ek_i'::regclass"
        ),
        "1 0 3"
    );
}
