//! A client must not be able to reach a panic: it ends the connection's thread, and
//! whatever that thread held. The statements of the suite, cut and bent the way a client
//! can, go down both routes, and each connection has to answer the one after.

use crate::common::*;
use std::time::Duration;

const CORPUS: &str = include_str!("../fixtures/rewrite-corpus.hex");

/// A small deterministic generator: a failure here is the same failure every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// What the corpus holds little of.
const MORE: &[&str] = &[
    r#"COMMENT ON COLUMN "s"."t"."c" IS 'it''s'"#,
    r#"COMMENT ON CONSTRAINT c ON t IS NULL"#,
    r#"CREATE ROLE "a b" LOGIN PASSWORD 'x' IN ROLE b"#,
    r#"ALTER ROLE "a b" RENAME TO "c d""#,
    r#"DROP ROLE IF EXISTS "a b", c"#,
    r#"CREATE SCHEMA IF NOT EXISTS "s" AUTHORIZATION x"#,
    "DROP SCHEMA IF EXISTS a, b CASCADE",
    r#"ALTER SCHEMA "a" RENAME TO "b""#,
    r#"CREATE DATABASE "a b" WITH OWNER x"#,
    r#"ALTER DATABASE "a b" RENAME TO "c d""#,
    r#"GRANT SELECT, INSERT ON "t", s.u TO "a b", PUBLIC WITH GRANT OPTION"#,
    "REVOKE ADMIN OPTION FOR a FROM b CASCADE",
    r#"SET ROLE "a b""#,
    "SET SESSION AUTHORIZATION 'x'",
    r#"ALTER TABLE ONLY "a b"."c" ALTER COLUMN "d e" SET DEFAULT lower('X'), ADD COLUMN f timestamptz DEFAULT now()"#,
    r#"ALTER TABLE t DROP CONSTRAINT IF EXISTS "x y" CASCADE"#,
    "ALTER TABLE t ADD CONSTRAINT u UNIQUE (a, b)",
    r#"CREATE POLICY "p q" ON t FOR SELECT TO a, "b c" USING (a = 1) WITH CHECK (b = 2)"#,
    r#"ALTER POLICY "p" ON t RENAME TO "q""#,
    r#"DROP POLICY IF EXISTS "p q" ON s.t"#,
    "COPY t (id, a) FROM STDIN WITH (FORMAT csv, HEADER true)",
    "COPY (SELECT 1) TO STDOUT",
    "PREPARE p (int) AS SELECT $1",
    "EXECUTE p (1, 'a')",
    "DEALLOCATE PREPARE p",
    "CREATE SEQUENCE s INCREMENT BY 2 START WITH 5 CACHE 3 CYCLE OWNED BY t.id",
    "SELECT nextval('s'), currval('s'), setval('s', 4, false), lastval()",
    "CREATE TYPE e AS ENUM ('a', 'b''c')",
    "ALTER TYPE e ADD VALUE IF NOT EXISTS 'x' BEFORE 'a'",
    "ALTER TYPE e RENAME VALUE 'a' TO 'b'",
    "INSERT INTO t (id, b) VALUES (1, 1.5), (2, $1) ON CONFLICT (id) DO UPDATE SET b = excluded.b RETURNING b",
    "INSERT INTO t (id, b) SELECT 1, 2 FROM t ON CONFLICT DO NOTHING",
    "UPDATE t AS x SET b = 1, a = 'x' FROM s WHERE x.id = s.id RETURNING *",
    "SELECT * FROM (VALUES (1, 2)) AS v(a, b), (SELECT 1, 2) AS u(c, d)",
    "SELECT id, x, o FROM t, unnest(t.e) WITH ORDINALITY AS u(x, o)",
    "SELECT k, v FROM t, jsonb_each(t.d) AS j(k, v) LEFT JOIN LATERAL jsonb_array_elements(j.v) e ON true",
    "SELECT c AT TIME ZONE 'UTC', (c + interval '1 hour') AT TIME ZONE z FROM t WHERE c > '2024-01-02 03:04:05+02'",
    "SELECT x::timestamp(3) with time zone, y::numeric(10, 2), z::character varying(3), TIMESTAMP 'x' FROM t",
    r"SELECT E'a\n\x41\u0041', U&'d\0061' UESCAPE '!', 'a'
       'b'",
];

const INSERTS: &[&str] = &[
    "'",
    "\"",
    "(",
    ")",
    ",",
    ";",
    "::",
    "ã",
    "😀",
    "$1",
    "$$",
    "--",
    "/*",
    "*/",
    "[",
    "]",
    "{",
    "}",
    "\\",
    "E'",
    "U&'",
    " AS ",
    " FROM ",
    " AT TIME ZONE ",
    " ON ",
    " SET ",
    "CAST(",
    "unnest(",
    " LATERAL ",
    " WITH ORDINALITY ",
    "%",
    "\t",
];

/// The nearest character boundary at or before at.
fn floor(s: &str, mut at: usize) -> usize {
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn bend(sql: &str, rng: &mut Rng) -> String {
    let at = floor(sql, rng.below(sql.len() + 1));
    match rng.below(4) {
        0 => sql[..at].to_string(),
        1 => sql[at..].to_string(),
        2 => [&sql[..at], INSERTS[rng.below(INSERTS.len())], &sql[at..]].concat(),
        _ => {
            let to = floor(sql, at + rng.below(sql.len() - at + 1));
            [&sql[..at], INSERTS[rng.below(INSERTS.len())], &sql[to..]].concat()
        }
    }
}

fn dial(server: &Server) -> RawConn {
    let mut fe = RawConn::dial(server).unwrap();
    fe.startup("postgres", "test", "").unwrap();
    fe.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    fe
}

/// Reads to ReadyForQuery, refusing a COPY and ending any transaction, for an aborted
/// one would answer the rest without reading them. False if the connection went.
fn settle(fe: &mut RawConn) -> bool {
    loop {
        match fe.receive() {
            Ok(Backend::ReadyForQuery(b'I')) => return true,
            Ok(Backend::ReadyForQuery(_)) => {
                fe.send_query("ROLLBACK");
                if fe.flush().is_err() {
                    return false;
                }
            }
            Ok(Backend::CopyInResponse(..)) => {
                fe.send_copy_fail("no");
                if fe.flush().is_err() {
                    return false;
                }
            }
            Ok(_) => {}
            // A statement that never answers is lost as well.
            Err(_) => return false,
        }
    }
}

fn is_slow(sql: &str) -> bool {
    let low = sql.to_lowercase();
    low.contains("pg_sleep") || low.contains("generate_series") || low.contains("advisory")
}

#[test]
fn a_bent_statement_ends_no_connection() {
    let statements: Vec<String> = CORPUS
        .lines()
        .filter_map(|line| hex::decode(line.trim()).ok())
        .filter_map(|b| String::from_utf8(b).ok())
        .filter(|s| !is_slow(s))
        .chain(MORE.iter().map(|s| (*s).to_string()))
        .collect();
    assert!(statements.len() > 500, "{}", statements.len());
    let rounds: usize = std::env::var("OVERLITE_BEND_ROUNDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1500);

    let server = start_server();
    let setup = connect(&server);
    must_exec(
        &setup,
        "CREATE TABLE t (id int PRIMARY KEY, a text, b numeric(10,2), c timestamptz, d jsonb, e int[])",
    );
    let (mut simple, mut extended) = (dial(&server), dial(&server));
    let seed: u64 = std::env::var("OVERLITE_BEND_SEED")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0x2545_F491_4F6C_DD1D);
    let mut rng = Rng(seed);
    let mut lost = Vec::new();
    for round in 0..rounds {
        let base = &statements[rng.below(statements.len())];
        let sql = bend(base, &mut rng);
        let sql = sql.replace('\0', "");
        if is_slow(&sql) {
            continue;
        }
        simple.send_query(&sql);
        if simple.flush().is_err() || !settle(&mut simple) {
            lost.push(format!("simple, round {round}: {sql}"));
            simple = dial(&server);
        }
        extended.send_parse("", &sql, &[]);
        extended.send_bind("", "", &[], &[], &[]);
        extended.send_describe(b'P', "");
        extended.send_execute("", 0);
        extended.send_sync();
        if extended.flush().is_err() || !settle(&mut extended) {
            lost.push(format!("extended, round {round}: {sql}"));
            extended = dial(&server);
        }
        if lost.len() > 6 {
            break;
        }
    }
    assert!(lost.is_empty(), "{} connections lost:\n{}", lost.len(), lost.join("\n"));
}

// The same for what stands around a statement: the fields of the messages themselves.
#[test]
fn a_bent_message_ends_no_connection() {
    let server = start_server();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let parse = {
        let mut b = b"s\0SELECT $1\0".to_vec();
        b.extend_from_slice(&1i16.to_be_bytes());
        b.extend_from_slice(&23i32.to_be_bytes());
        b
    };
    let bind = {
        let mut b = b"p\0s\0".to_vec();
        b.extend_from_slice(&[0, 1, 0, 0, 0, 1, 0, 0, 0, 1, b'1', 0, 1, 0, 1]);
        b
    };
    let messages: [(u8, &[u8]); 5] = [
        (b'P', &parse),
        (b'B', &bind),
        (b'D', b"Ss\0"),
        (b'E', b"p\0\0\0\0\0"),
        (b'C', b"Ss\0"),
    ];
    let mut fe = dial(&server);
    for round in 0..4000 {
        let (kind, body) = messages[rng.below(messages.len())];
        let mut body = body.to_vec();
        match rng.below(3) {
            0 => body.truncate(rng.below(body.len() + 1)),
            1 => {
                let at = rng.below(body.len());
                body[at] = rng.next() as u8;
            }
            _ => body.extend((0..rng.below(8)).map(|_| 0xFFu8)),
        }
        let mut raw = vec![kind];
        raw.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
        raw.extend_from_slice(&body);
        fe.send_bytes(&raw);
        fe.send_sync();
        fe.flush().unwrap();
        assert!(settle(&mut fe), "round {round}: {kind} {body:?}");
    }
}
