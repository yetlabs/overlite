use crate::common::*;

// rls_select: a subject role sees only its own rows; the superuser owner sees
// everything.
#[test]
fn rls_select() {
    let addr = rls_setup();

    let alice = connect_as(&addr, "alice", "a");
    let n: i64 = alice.query_row("SELECT count(*) FROM docs", &[]).scan().unwrap();
    assert_eq!(n, 2, "alice sees her 2 rows");

    // A filter on top of RLS still works.
    let n: i64 = alice
        .query_row("SELECT count(*) FROM docs WHERE id > 1", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);

    let bob = connect_as(&addr, "bob", "b");
    let n: i64 = bob.query_row("SELECT count(*) FROM docs", &[]).scan().unwrap();
    assert_eq!(n, 1, "bob sees his 1 row");

    // Superuser owner bypasses RLS.
    let admin = connect_as(&addr, "postgres", "adminpw");
    let n: i64 = admin.query_row("SELECT count(*) FROM docs", &[]).scan().unwrap();
    assert_eq!(n, 3);
}

// rls_update_delete: USING limits which rows a subject may change.
#[test]
fn rls_update_delete() {
    let addr = rls_setup();
    let alice = connect_as(&addr, "alice", "a");

    // alice can't delete bob's row (id 2 invisible under USING).
    let ct = alice.exec("DELETE FROM docs WHERE id = 2", &[]).unwrap();
    assert_eq!(ct.rows_affected(), 0, "bob's row not visible");

    // alice can delete her own.
    let ct = alice.exec("DELETE FROM docs WHERE id = 1", &[]).unwrap();
    assert_eq!(ct.rows_affected(), 1);

    // alice can't update bob's row.
    let ct = alice.exec("UPDATE docs SET body = 'hacked' WHERE id = 2", &[]).unwrap();
    assert_eq!(ct.rows_affected(), 0);

    // Confirm via superuser that bob's row is intact.
    let admin = connect_as(&addr, "postgres", "adminpw");
    let body: String = admin
        .query_row("SELECT body FROM docs WHERE id = 2", &[])
        .scan()
        .unwrap();
    assert_eq!(body, "b1");
}

// rls_insert_with_check: FOR ALL USING doubles as the write check; a row that
// fails it is rejected.
#[test]
fn rls_insert_with_check() {
    let addr = rls_setup();
    let alice = connect_as(&addr, "alice", "a");

    // Inserting a row she owns is fine.
    alice
        .exec("INSERT INTO docs VALUES (10, 'alice', 'mine')", &[])
        .unwrap();

    // Inserting a row owned by someone else violates the policy.
    alice
        .exec("INSERT INTO docs (id, owner, body) VALUES (11, 'bob', 'nope')", &[])
        .expect_err("row violates WITH CHECK");

    // The good row persisted, the bad one didn't.
    let admin = connect_as(&addr, "postgres", "adminpw");
    let n: i64 = admin
        .query_row("SELECT count(*) FROM docs WHERE id IN (10,11)", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);
}

// rls_insert_parameterized: WITH CHECK is enforced for parameterized inserts
// (extended protocol), where the values arrive as bound params.
#[test]
fn rls_insert_parameterized() {
    let addr = rls_setup();
    let alice = connect_as(&addr, "alice", "a");

    // A row she owns, passed as parameters, is accepted.
    alice
        .exec("INSERT INTO docs VALUES ($1, $2, $3)", &[&20, &"alice", &"p"])
        .unwrap();

    // A row owned by someone else is rejected even via params.
    alice
        .exec(
            "INSERT INTO docs (id, owner, body) VALUES ($1, $2, $3)",
            &[&21, &"bob", &"p"],
        )
        .expect_err("parameterized row violates WITH CHECK");

    let admin = connect_as(&addr, "postgres", "adminpw");
    let n: i64 = admin
        .query_row("SELECT count(*) FROM docs WHERE id IN (20,21)", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);
}

// rls_insert_returning: WITH CHECK is enforced even when the INSERT carries a
// RETURNING clause, and the returned row still comes back on success.
#[test]
fn rls_insert_returning() {
    let addr = rls_setup();
    let alice = connect_as(&addr, "alice", "a");

    // Own row: accepted, and RETURNING yields it.
    let id: i64 = alice
        .query_row("INSERT INTO docs VALUES (40, 'alice', 'r') RETURNING id", &[])
        .scan()
        .unwrap();
    assert_eq!(id, 40);

    // Parameterized RETURNING, own row.
    let id: i64 = alice
        .query_row(
            "INSERT INTO docs VALUES ($1, $2, $3) RETURNING id",
            &[&41, &"alice", &"r"],
        )
        .scan()
        .unwrap();
    assert_eq!(id, 41);

    // Foreign row is rejected despite RETURNING.
    alice
        .query_row("INSERT INTO docs VALUES (42, 'bob', 'r') RETURNING id", &[])
        .scan::<i64>()
        .unwrap_err();

    let admin = connect_as(&addr, "postgres", "adminpw");
    let n: i64 = admin
        .query_row("SELECT count(*) FROM docs WHERE id IN (40,41,42)", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 2);
}

// rls_insert_select: WITH CHECK is enforced on INSERT … SELECT — every row
// the source produces must satisfy the policy.
#[test]
fn rls_insert_select() {
    let addr = rls_setup();
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE TABLE incoming (id int, owner text, body text)");
    must_exec(&admin, "INSERT INTO incoming VALUES (50,'alice','s'),(51,'bob','s')");
    must_exec(&admin, "GRANT SELECT ON incoming TO alice");

    let alice = connect_as(&addr, "alice", "a");

    // Source row she owns: accepted.
    alice
        .exec("INSERT INTO docs SELECT * FROM incoming WHERE id = 50", &[])
        .unwrap();

    // A source row owned by someone else is rejected.
    alice
        .exec("INSERT INTO docs SELECT * FROM incoming WHERE id = 51", &[])
        .expect_err("row violates WITH CHECK");

    // A batch containing a bad row rejects the whole statement.
    alice.exec("INSERT INTO docs SELECT * FROM incoming", &[]).unwrap_err();

    // Only the good row landed.
    let n: i64 = admin
        .query_row("SELECT count(*) FROM docs WHERE id IN (50,51)", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);
}

// rls_insert_on_conflict: WITH CHECK is enforced on the INSERT path of an
// upsert (ON CONFLICT DO NOTHING / DO UPDATE).
#[test]
fn rls_insert_on_conflict() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE items (id int PRIMARY KEY, owner text, body text)");
    must_exec(&admin, "INSERT INTO items VALUES (1,'alice','a')");
    must_exec(&admin, "GRANT ALL ON items TO alice");
    must_exec(&admin, "ALTER TABLE items ENABLE ROW LEVEL SECURITY");
    must_exec(
        &admin,
        "CREATE POLICY own ON items FOR ALL USING (owner = current_user)",
    );

    let alice = connect_as(&addr, "alice", "a");

    // New own row, DO NOTHING: accepted.
    alice
        .exec(
            "INSERT INTO items VALUES (2,'alice','x') ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .unwrap();

    // A row owned by someone else is rejected on the INSERT path.
    alice
        .exec(
            "INSERT INTO items VALUES (3,'bob','x') ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .expect_err("insert-path WITH CHECK");

    // Upsert onto her own existing row: source passes the check, DO UPDATE runs.
    alice
        .exec(
            "INSERT INTO items VALUES (1,'alice','updated') ON CONFLICT (id) DO UPDATE SET body = 'updated'",
            &[],
        )
        .unwrap();

    let body: String = admin
        .query_row("SELECT body FROM items WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert_eq!(body, "updated");

    // Bad row didn't land; the DO NOTHING one did.
    let n: i64 = admin
        .query_row("SELECT count(*) FROM items WHERE id IN (2,3)", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 1);
}

// rls_upsert_update_check: a DO UPDATE SET cannot write a row that violates the
// policy — the post-update row is validated, not just the insert source.
#[test]
fn rls_upsert_update_check() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE items (id int PRIMARY KEY, owner text, body text)");
    must_exec(&admin, "INSERT INTO items VALUES (1,'alice','a')");
    must_exec(&admin, "GRANT ALL ON items TO alice");
    must_exec(&admin, "ALTER TABLE items ENABLE ROW LEVEL SECURITY");
    must_exec(
        &admin,
        "CREATE POLICY own ON items FOR ALL USING (owner = current_user)",
    );

    let alice = connect_as(&addr, "alice", "a");

    // Legit update of her own row.
    alice
        .exec(
            "INSERT INTO items VALUES (1,'alice','x') ON CONFLICT (id) DO UPDATE SET body = 'x'",
            &[],
        )
        .unwrap();

    // The hole: try to hand her row to bob via DO UPDATE SET owner.
    alice
        .exec(
            "INSERT INTO items VALUES (1,'alice','y') ON CONFLICT (id) DO UPDATE SET owner = 'bob'",
            &[],
        )
        .expect_err("post-update row must still satisfy the policy");

    // The row is untouched by the rejected upsert.
    let (owner, body): (String, String) = admin
        .query_row("SELECT owner, body FROM items WHERE id = 1", &[])
        .scan()
        .unwrap();
    assert_eq!(owner, "alice");
    assert_eq!(body, "x");

    // A legit upsert with RETURNING still returns its row.
    let id: i64 = alice
        .query_row(
            "INSERT INTO items VALUES (1,'alice','z') ON CONFLICT (id) DO UPDATE SET body = 'z' RETURNING id",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(id, 1);
}

// rls_insert_select_read_filter: the source of an INSERT … SELECT is itself
// RLS-filtered, so only rows the reader may see are copied.
#[test]
fn rls_insert_select_read_filter() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE src (id int, owner text)");
    must_exec(&admin, "INSERT INTO src VALUES (1,'alice'),(2,'bob')");
    must_exec(&admin, "GRANT SELECT ON src TO alice");
    must_exec(&admin, "ALTER TABLE src ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY p ON src FOR SELECT USING (owner = current_user)");
    must_exec(&admin, "CREATE TABLE dst (id int, owner text)"); // no RLS on dst
    must_exec(&admin, "GRANT ALL ON dst TO alice");

    let alice = connect_as(&addr, "alice", "a");
    alice.exec("INSERT INTO dst SELECT * FROM src", &[]).unwrap();

    // Only alice's visible source row was copied.
    let rows = admin.query("SELECT id, owner FROM dst ORDER BY id", &[]).unwrap();
    let mut got = Vec::new();
    for row in &rows {
        let (_id, owner): (i64, String) = row.scan().unwrap();
        got.push(owner);
    }
    assert_eq!(got, ["alice"], "bob's row was filtered out of the source");
}

// rls_insert_default_values: the single all-defaults row of INSERT … DEFAULT
// VALUES is validated against WITH CHECK.
#[test]
fn rls_insert_default_values() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");

    // Default satisfies the policy → accepted.
    must_exec(&admin, "CREATE TABLE ok_t (id int, tag text DEFAULT 'ok')");
    must_exec(&admin, "GRANT ALL ON ok_t TO alice");
    must_exec(&admin, "ALTER TABLE ok_t ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY p ON ok_t FOR ALL USING (tag = 'ok')");

    // Default violates the policy → rejected.
    must_exec(&admin, "CREATE TABLE bad_t (id int, tag text DEFAULT 'nope')");
    must_exec(&admin, "GRANT ALL ON bad_t TO alice");
    must_exec(&admin, "ALTER TABLE bad_t ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY p ON bad_t FOR ALL USING (tag = 'ok')");

    let alice = connect_as(&addr, "alice", "a");

    alice.exec("INSERT INTO ok_t DEFAULT VALUES", &[]).unwrap();
    alice
        .exec("INSERT INTO bad_t DEFAULT VALUES", &[])
        .expect_err("the default row violates the policy");

    let ok_n: i64 = admin.query_row("SELECT count(*) FROM ok_t", &[]).scan().unwrap();
    let bad_n: i64 = admin.query_row("SELECT count(*) FROM bad_t", &[]).scan().unwrap();
    assert_eq!(ok_n, 1);
    assert_eq!(bad_n, 0);
}

// rls_default_deny: with RLS enabled and no policy for the command, a subject
// sees nothing.
#[test]
fn rls_default_deny() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE TABLE t (id int)");
    must_exec(&admin, "INSERT INTO t VALUES (1),(2)");
    must_exec(&admin, "GRANT ALL ON t TO alice");
    must_exec(&admin, "ALTER TABLE t ENABLE ROW LEVEL SECURITY"); // no policy at all

    let alice = connect_as(&addr, "alice", "a");
    let n: i64 = alice.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 0, "default deny");

    // Superuser still bypasses.
    let n: i64 = admin.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 2);
}

// is_rls_ddl: the words in a default are not row level security.
#[test]
fn rls_ddl_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (id int)");
    must_exec(&conn, "ALTER TABLE t ADD COLUMN a text DEFAULT 'row level security'");
    conn.exec_params::<&[u8]>(
        "ALTER TABLE t ADD COLUMN b text DEFAULT 'row level security'",
        &[],
        &[],
        &[],
        &[],
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM pragma_table_info('t')", &[])
        .scan()
        .unwrap();
    assert_eq!(n, 3);
}

// alter_table_rls_write: what is switched is the word before ROW, not whatever in
// the statement spells force or disable.
#[test]
fn rls_action_not_in_the_table_name() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE force_log (id int)");
    must_exec(&conn, "CREATE TABLE disable_me (id int)");
    must_exec(&conn, "ALTER TABLE force_log ENABLE ROW LEVEL SECURITY");
    must_exec(&conn, "ALTER TABLE disable_me ENABLE ROW LEVEL SECURITY");
    let state = |t: &str| -> (i64, i64) {
        conn.query_row(
            &format!("SELECT enabled, forced FROM _overlite_rls WHERE tablename = '{t}'"),
            &[],
        )
        .scan()
        .unwrap()
    };
    assert_eq!(state("force_log"), (1, 0));
    assert_eq!(state("disable_me"), (1, 0));
    must_exec(&conn, "ALTER TABLE force_log FORCE ROW LEVEL SECURITY");
    assert_eq!(state("force_log"), (1, 1));
    must_exec(&conn, "ALTER TABLE force_log NO FORCE ROW LEVEL SECURITY");
    assert_eq!(state("force_log"), (1, 0));
    must_exec(&conn, "ALTER TABLE disable_me DISABLE ROW LEVEL SECURITY");
    assert_eq!(state("disable_me"), (0, 0));
}

// parse_policy: its clauses are found in the code, not in the expression's literals.
// A policy on the wrong command or for the wrong roles fails open or shut.
#[test]
fn policy_clauses_not_in_a_literal() {
    let conn = connect_extended(&start_server());
    must_exec(&conn, "CREATE TABLE t (who text)");
    must_exec(
        &conn,
        "CREATE POLICY p ON t USING (who = 'restrictive for update to bob with check (0)')",
    );
    let row: (String, String, i64, String, Option<String>) = conn
        .query_row(
            "SELECT command, roles, permissive, using_expr, check_expr FROM _overlite_policies WHERE polname = 'p'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(row.0, "ALL");
    assert_eq!(row.1, "");
    assert_eq!(row.2, 1);
    assert_eq!(row.3, "who = 'restrictive for update to bob with check (0)'");
    assert_eq!(row.4.unwrap_or_default(), "");
}

// has_do_update: a DO NOTHING upsert with those words after it is checked as an
// insert, so the row a policy refuses is refused even where it conflicts. Taken for
// a DO UPDATE, the check looks at the rows written, and there are none.
#[test]
fn do_update_not_in_a_literal() {
    let addr = rls_setup();
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE TABLE notes (id int PRIMARY KEY, owner text)");
    must_exec(&admin, "INSERT INTO notes VALUES (1, 'bob')");
    must_exec(&admin, "GRANT ALL ON notes TO alice");
    must_exec(&admin, "ALTER TABLE notes ENABLE ROW LEVEL SECURITY");
    must_exec(
        &admin,
        "CREATE POLICY own ON notes FOR ALL USING (owner = current_user)",
    );

    let alice = connect_as(&addr, "alice", "a");
    let err = alice
        .exec(
            "INSERT INTO notes VALUES (1, 'bob') ON CONFLICT (id) DO NOTHING RETURNING 'do update'",
            &[],
        )
        .unwrap_err();
    assert_sqlstate(&err, "42501");
}

fn extended(conn: &Conn, sql: &str) -> Result<PgResult> {
    conn.exec_params::<&[u8]>(sql, &[], &[], &[], &[])
}

fn count(conn: &Conn, sql: &str) -> i64 {
    conn.query_row(sql, &[]).scan().unwrap()
}

/// Roles and schemas, with a table of alice's and bob's rows in `table`, readable by
/// them. The table is admin's unless `owner` creates it.
fn rls_table(admin: &Conn, owner: &Conn, table: &str) {
    must_exec(owner, &format!("CREATE TABLE {table} (id int, who text)"));
    must_exec(
        owner,
        &format!("INSERT INTO {table} VALUES (1, 'alice'), (2, 'bob'), (3, 'alice')"),
    );
    // Postgres refuses a grant to a role that is not there, and some callers made none.
    let _ = admin.exec("CREATE ROLE alice", &[]);
    let _ = admin.exec("CREATE ROLE bob", &[]);
    must_exec(owner, &format!("GRANT ALL ON {table} TO alice"));
    must_exec(owner, &format!("GRANT ALL ON {table} TO bob"));
}

fn rls_server() -> (Server, Conn) {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN PASSWORD 'a'");
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'b'");
    must_exec(&admin, "CREATE SCHEMA s1");
    must_exec(&admin, "CREATE SCHEMA s2");
    (addr, admin)
}

const OWN: &str = "USING (who = current_user)";

// RLS on a table in a schema applies to it, however the statements spell the table.
#[test]
fn rls_in_a_schema_applies() {
    let (addr, admin) = rls_server();
    rls_table(&admin, &admin, "s1.t");
    must_exec(&admin, "ALTER TABLE s1.t ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, &format!("CREATE POLICY own ON s1.t {OWN}"));

    let alice = connect_as(&addr, "alice", "a");
    assert_eq!(count(&alice, "SELECT count(*) FROM s1.t"), 2);
    assert_eq!(count(&alice, r#"SELECT count(*) FROM "s1"."t""#), 2);
    assert_eq!(alice.exec("UPDATE s1.t SET id = id", &[]).unwrap().rows_affected(), 2);
    assert_eq!(
        alice
            .exec("DELETE FROM s1.t WHERE id = 1", &[])
            .unwrap()
            .rows_affected(),
        1
    );
    assert_eq!(
        alice
            .exec("DELETE FROM s1.t WHERE id = 2", &[])
            .unwrap()
            .rows_affected(),
        0
    );
    assert_sqlstate(
        &alice.exec("INSERT INTO s1.t VALUES (9, 'bob')", &[]).unwrap_err(),
        "42501",
    );
    must_exec(&alice, "INSERT INTO s1.t VALUES (9, 'alice')");

    must_exec(&alice, "SET search_path TO s1");
    assert_eq!(count(&alice, "SELECT count(*) FROM t"), 2);
    assert_eq!(
        alice.exec("DELETE FROM t WHERE id = 2", &[]).unwrap().rows_affected(),
        0
    );

    // Set up through the search_path and read qualified, and the other way round.
    must_exec(&admin, "SET search_path TO s2");
    rls_table(&admin, &admin, "u");
    must_exec(&admin, "ALTER TABLE u ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, &format!("CREATE POLICY own ON u {OWN}"));
    must_exec(&admin, "RESET search_path");
    let bob = connect_as(&addr, "bob", "b");
    assert_eq!(count(&bob, "SELECT count(*) FROM s2.u"), 1);
    must_exec(&admin, "DROP POLICY own ON s2.u");
    assert_eq!(count(&bob, "SELECT count(*) FROM s2.u"), 0, "enabled, with no policy");
    must_exec(&admin, r#"ALTER TABLE "s2"."u" DISABLE ROW LEVEL SECURITY"#);
    assert_eq!(count(&bob, "SELECT count(*) FROM s2.u"), 3);
}

// A policy belongs to one table. Neither way does it reach a table of the same name.
#[test]
fn rls_stays_on_its_table() {
    let (addr, admin) = rls_server();
    rls_table(&admin, &admin, "t");
    rls_table(&admin, &admin, "s1.t");
    rls_table(&admin, &admin, "s2.t");
    must_exec(&admin, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, &format!("CREATE POLICY own ON t {OWN}"));
    must_exec(&admin, "ALTER TABLE s2.t ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY none ON s2.t USING (false)");

    let alice = connect_as(&addr, "alice", "a");
    assert_eq!(count(&alice, "SELECT count(*) FROM t"), 2);
    assert_eq!(count(&alice, "SELECT count(*) FROM s1.t"), 3, "no RLS there");
    assert_eq!(count(&alice, "SELECT count(*) FROM s2.t"), 0);
    assert_eq!(alice.exec("UPDATE s1.t SET id = id", &[]).unwrap().rows_affected(), 3);
    must_exec(&alice, "INSERT INTO s1.t VALUES (9, 'bob')");
}

// The table the owner bypasses is the one she owns.
#[test]
fn rls_owner_of_a_namesake_does_not_bypass() {
    let (addr, admin) = rls_server();
    let alice = connect_as(&addr, "alice", "a");
    rls_table(&admin, &alice, "t");
    rls_table(&admin, &admin, "s1.t");
    must_exec(&admin, "ALTER TABLE s1.t ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, "CREATE POLICY none ON s1.t USING (false)");
    must_exec(&alice, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    must_exec(&alice, "CREATE POLICY none ON t USING (false)");

    assert_eq!(count(&alice, "SELECT count(*) FROM t"), 3, "her own: she bypasses");
    assert_eq!(count(&alice, "SELECT count(*) FROM s1.t"), 0, "not hers");
}

// A spelling of the table must not be a way around its policy.
#[test]
fn rls_every_spelling_of_a_public_table() {
    let (addr, admin) = rls_server();
    rls_table(&admin, &admin, "t");
    must_exec(&admin, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, &format!("CREATE POLICY own ON t {OWN}"));
    let alice = connect_as(&addr, "alice", "a");
    for t in ["t", "\"t\"", "public.t", "\"public\".\"t\""] {
        assert_eq!(count(&alice, &format!("SELECT count(*) FROM {t}")), 2, "{t}");
        let n = alice.exec(&format!("UPDATE {t} SET id = id"), &[]).unwrap();
        assert_eq!(n.rows_affected(), 2, "UPDATE {t}");
        let n = extended(&alice, &format!("DELETE FROM {t} WHERE id = 2")).unwrap();
        assert_eq!(n.tag.rows_affected(), 0, "DELETE {t}");
        assert_sqlstate(
            &alice
                .exec(&format!("INSERT INTO {t} VALUES (9, 'bob')"), &[])
                .unwrap_err(),
            "42501",
        );
    }
}

// The flags and the policies go where the table goes, at once, in every session.
#[test]
fn rls_follows_the_table() {
    let (addr, admin) = rls_server();
    rls_table(&admin, &admin, "t");
    must_exec(&admin, "ALTER TABLE t ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, &format!("CREATE POLICY own ON t {OWN}"));
    let alice = connect_as(&addr, "alice", "a");
    assert_eq!(count(&alice, "SELECT count(*) FROM t"), 2); // the answer is cached
    let polrel = |t: &str, ns: &str| -> i64 {
        count(
            &admin,
            &format!(
                "SELECT count(*) FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE p.polname = 'own' AND c.relname = '{t}' AND n.nspname = '{ns}' AND c.relrowsecurity = 1"
            ),
        )
    };
    assert_eq!(polrel("t", "public"), 1);

    must_exec(&admin, "ALTER TABLE t SET SCHEMA s1");
    assert_eq!(count(&alice, "SELECT count(*) FROM s1.t"), 2, "SET SCHEMA");
    assert_eq!(polrel("t", "s1"), 1);

    must_exec(&admin, "ALTER TABLE s1.t RENAME TO r");
    assert_eq!(count(&alice, "SELECT count(*) FROM s1.r"), 2, "RENAME TO");
    assert_eq!(polrel("r", "s1"), 1);

    must_exec(&admin, "ALTER SCHEMA s1 RENAME TO s9");
    assert_eq!(count(&alice, "SELECT count(*) FROM s9.r"), 2, "RENAME SCHEMA");
    assert_eq!(polrel("r", "s9"), 1);

    must_exec(&admin, "ALTER TABLE s9.r SET SCHEMA public");
    assert_eq!(count(&alice, "SELECT count(*) FROM r"), 2);
    assert_eq!(polrel("r", "public"), 1);

    // Nothing is left under the old names, and a new table there starts plain.
    rls_table(&admin, &admin, "s9.t");
    assert_eq!(count(&alice, "SELECT count(*) FROM s9.t"), 3);

    // Dropping forgets it: the name is free of the policy.
    must_exec(&admin, "DROP TABLE r");
    rls_table(&admin, &admin, "r");
    assert_eq!(count(&alice, "SELECT count(*) FROM r"), 3);
    must_exec(&admin, "ALTER TABLE s9.t ENABLE ROW LEVEL SECURITY");
    assert_eq!(count(&alice, "SELECT count(*) FROM s9.t"), 0);
    must_exec(&admin, "DROP SCHEMA s9 CASCADE");
    must_exec(&admin, "CREATE SCHEMA s9");
    rls_table(&admin, &admin, "s9.t");
    assert_eq!(count(&alice, "SELECT count(*) FROM s9.t"), 3);
}

// Postgres filters COPY TO by the policies, and refuses COPY FROM to a role under RLS.
#[test]
fn rls_copy() {
    let (addr, admin) = rls_server();
    for t in ["t", "s1.t"] {
        rls_table(&admin, &admin, t);
        must_exec(&admin, &format!("ALTER TABLE {t} ENABLE ROW LEVEL SECURITY"));
        must_exec(&admin, &format!("CREATE POLICY own ON {t} {OWN}"));
        let alice = connect_as(&addr, "alice", "a");

        let mut out = Vec::new();
        alice.copy_to(&mut out, &format!("COPY {t} TO STDOUT")).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "1\talice\n3\talice\n");
        let mut out = Vec::new();
        alice
            .copy_to(&mut out, &format!("COPY (SELECT id FROM {t}) TO STDOUT"))
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "1\n3\n");
        let err = alice
            .copy_from("9\talice\n".as_bytes(), &format!("COPY {t} FROM STDIN"))
            .unwrap_err();
        assert_pg_error(&err, "0A000", "COPY FROM not supported with row-level security");

        let mut out = Vec::new();
        admin.copy_to(&mut out, &format!("COPY {t} TO STDOUT")).unwrap();
        assert_eq!(
            out.iter().filter(|c| **c == b'\n').count(),
            3,
            "the admin is not subject"
        );
        admin
            .copy_from("9\tbob\n".as_bytes(), &format!("COPY {t} FROM STDIN"))
            .unwrap();
    }
}

/// A database as an older server wrote it: RLS rows under the name as the statement
/// spelled it, and the upgrade not yet run.
fn stop_as_old(server: Server, db: &std::path::Path, sql: &str) {
    signal_and_wait(&server, libc::SIGTERM).0.unwrap();
    rusqlite::Connection::open(db).unwrap().execute_batch(sql).unwrap();
}

const OLD_RLS: &str = "UPDATE _overlite_rls SET tablename = 'docs' WHERE tablename = 's1.docs';
    UPDATE _overlite_policies SET tablename = 'docs' WHERE tablename = 's1.docs';
    DROP TABLE IF EXISTS _overlite_rls_stored_names;";

// An older server looked RLS up by bare name, so what was set through the search_path
// did protect s1.docs. A bare row now means the public table, which would have opened
// it: the first start copies such rows onto the tables they protected.
#[test]
fn old_rls_rows_keep_protecting() {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    let first = start_proc(&db);
    let admin = connect(&first);
    must_exec(&admin, "CREATE ROLE alice LOGIN");
    must_exec(&admin, "CREATE SCHEMA s1");
    rls_table(&admin, &admin, "s1.docs");
    must_exec(&admin, "ALTER TABLE s1.docs ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, &format!("CREATE POLICY own ON s1.docs {OWN}"));
    drop(admin);
    stop_as_old(first, &db, OLD_RLS);

    let server = start_proc(&db);
    let alice = connect_as(&server, "alice", "");
    assert_eq!(count(&alice, "SELECT count(*) FROM s1.docs"), 2);
    assert_eq!(
        alice.exec("UPDATE s1.docs SET id = id", &[]).unwrap().rows_affected(),
        2
    );
    assert_sqlstate(
        &alice.exec("INSERT INTO s1.docs VALUES (9, 'bob')", &[]).unwrap_err(),
        "42501",
    );

    // Run once: after it, a bare row is the public table's and reaches nothing else.
    let admin = connect(&server);
    must_exec(&admin, "CREATE SCHEMA s2");
    rls_table(&admin, &admin, "s2.docs");
    must_exec(&admin, "CREATE TABLE docs (id int, who text)");
    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    drop((alice, admin));
    signal_and_wait(&server, libc::SIGTERM).0.unwrap();
    let again = start_proc(&db);
    assert_eq!(
        count(&connect_as(&again, "alice", ""), "SELECT count(*) FROM s2.docs"),
        3
    );
}

// The old row covered every table of that name, the public one too: all stay covered,
// and if the table's own row says less, what protected it wins.
#[test]
fn old_rls_row_shared_by_namesakes() {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    let first = start_proc(&db);
    let admin = connect(&first);
    must_exec(&admin, "CREATE ROLE alice LOGIN");
    must_exec(&admin, "CREATE SCHEMA s1");
    must_exec(&admin, "CREATE SCHEMA s2");
    rls_table(&admin, &admin, "docs");
    rls_table(&admin, &admin, "s1.docs");
    rls_table(&admin, &admin, "s2.docs");
    must_exec(&admin, "ALTER TABLE docs ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, &format!("CREATE POLICY own ON docs {OWN}"));
    must_exec(&admin, "ALTER TABLE s2.docs DISABLE ROW LEVEL SECURITY");
    drop(admin);
    stop_as_old(first, &db, "DROP TABLE IF EXISTS _overlite_rls_stored_names;");

    let server = start_proc(&db);
    let alice = connect_as(&server, "alice", "");
    for t in ["docs", "s1.docs", "s2.docs"] {
        assert_eq!(count(&alice, &format!("SELECT count(*) FROM {t}")), 2, "{t}");
    }
}

// A table named as "s1"."t" left a row that matched nothing, so nothing was enforced.
// What that statement asked for now holds, and a qualified row starts to count too.
#[test]
fn old_rls_row_in_quoted_spelling() {
    let dir = temp_dir();
    let db = dir.path().join("test.db");
    let first = start_proc(&db);
    let admin = connect(&first);
    must_exec(&admin, "CREATE ROLE alice LOGIN");
    must_exec(&admin, "CREATE SCHEMA s1");
    rls_table(&admin, &admin, "s1.docs");
    must_exec(&admin, "ALTER TABLE s1.docs ENABLE ROW LEVEL SECURITY");
    must_exec(&admin, &format!("CREATE POLICY own ON s1.docs {OWN}"));
    drop(admin);
    stop_as_old(
        first,
        &db,
        r#"UPDATE _overlite_rls SET tablename = 's1"."docs';
           UPDATE _overlite_policies SET tablename = 's1"."docs';
           DROP TABLE IF EXISTS _overlite_rls_stored_names;"#,
    );
    let server = start_proc(&db);
    assert_eq!(
        count(&connect_as(&server, "alice", ""), "SELECT count(*) FROM s1.docs"),
        2
    );
}

// ALTER POLICY changes what it names and leaves the rest. Answers and codes checked
// against PostgreSQL 17.
#[test]
fn alter_policy_using() {
    let addr = rls_setup();
    let admin = connect_as(&addr, "postgres", "adminpw");
    let alice = connect_as(&addr, "alice", "a");
    let count = |c: &Conn| -> i64 { c.query_row("SELECT count(*) FROM docs", &[]).scan().unwrap() };
    assert_eq!(count(&alice), 2);

    let tag = admin.exec("ALTER POLICY own ON docs USING (id = 2)", &[]).unwrap();
    assert_eq!(tag.as_str(), "ALTER POLICY");
    assert_eq!(count(&alice), 1, "the new USING reads rows");
    let (roles, using, check): (String, String, Option<String>) = admin
        .query_row(
            "SELECT roles, using_expr, nullif(check_expr, '') FROM _overlite_policies",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!((roles.as_str(), using.as_str(), check), ("", "id = 2", None));

    // The extended protocol takes the same route.
    let mut raw = raw_conn(&addr, "postgres", "adminpw");
    let tag = extended_exec(&mut raw, "ALTER POLICY own ON public.docs USING (owner = current_user)").unwrap();
    assert_eq!(tag, "ALTER POLICY");
    assert_eq!(count(&alice), 2);
}

#[test]
fn alter_policy_with_check() {
    let addr = rls_setup();
    let admin = connect_as(&addr, "postgres", "adminpw");
    let alice = connect_as(&addr, "alice", "a");
    alice
        .exec("INSERT INTO docs (id, owner, body) VALUES (10, 'alice', 'x')", &[])
        .unwrap();
    admin
        .exec("ALTER POLICY own ON docs WITH CHECK (id < 10)", &[])
        .unwrap();
    let err = alice
        .exec("INSERT INTO docs (id, owner, body) VALUES (11, 'alice', 'x')", &[])
        .unwrap_err();
    assert_sqlstate(&err, "42501");
    alice
        .exec("INSERT INTO docs (id, owner, body) VALUES (9, 'bob', 'x')", &[])
        .expect("only the check changed");

    // USING is untouched by a WITH CHECK, and the other way round.
    let (using, check): (String, String) = admin
        .query_row("SELECT using_expr, check_expr FROM _overlite_policies", &[])
        .scan()
        .unwrap();
    assert_eq!(using, "owner = current_user");
    assert_eq!(check, "id < 10");
}

#[test]
fn alter_policy_to_roles() {
    let addr = rls_setup();
    let admin = connect_as(&addr, "postgres", "adminpw");
    let alice = connect_as(&addr, "alice", "a");
    let count = |c: &Conn| -> i64 { c.query_row("SELECT count(*) FROM docs", &[]).scan().unwrap() };

    admin.exec("ALTER POLICY own ON docs TO bob", &[]).unwrap();
    assert_eq!(count(&alice), 0, "no policy for alice any more: default deny");

    admin
        .exec("ALTER POLICY own ON docs TO alice, bob USING (true)", &[])
        .unwrap();
    assert_eq!(count(&alice), 3);

    admin.exec("ALTER POLICY own ON docs TO PUBLIC", &[]).unwrap();
    assert_eq!(count(&alice), 3);

    let err = admin.exec("ALTER POLICY own ON docs TO ghost", &[]).unwrap_err();
    assert_pg_error(&err, "42704", "role \"ghost\" does not exist");
    assert_eq!(count(&alice), 3, "the failed statement changed nothing");
}

#[test]
fn alter_policy_rename() {
    let addr = rls_setup();
    let admin = connect_as(&addr, "postgres", "adminpw");
    admin
        .exec("CREATE POLICY second ON docs FOR SELECT USING (true)", &[])
        .unwrap();

    let err = admin
        .exec("ALTER POLICY own ON docs RENAME TO second", &[])
        .unwrap_err();
    assert_pg_error(&err, "42710", "policy \"second\" for table \"docs\" already exists");

    admin
        .exec("ALTER POLICY own ON docs RENAME TO \"Mine Now\"", &[])
        .unwrap();
    let names = query_column(&admin, "SELECT polname FROM _overlite_policies ORDER BY polname", 0);
    assert_eq!(names, ["Mine Now", "second"]);
    admin
        .exec("ALTER POLICY \"Mine Now\" ON docs USING (true)", &[])
        .unwrap();
    admin.exec("DROP POLICY \"Mine Now\" ON docs", &[]).unwrap();
}

#[test]
fn alter_policy_errors() {
    let addr = rls_setup();
    let admin = connect_as(&addr, "postgres", "adminpw");
    admin
        .exec("CREATE POLICY reader ON docs FOR SELECT USING (true)", &[])
        .unwrap();
    admin
        .exec("CREATE POLICY writer ON docs FOR INSERT WITH CHECK (true)", &[])
        .unwrap();

    let mut raw = raw_conn(&addr, "postgres", "adminpw");
    for (sql, code, message) in [
        (
            "ALTER POLICY nope ON docs USING (true)",
            "42704",
            "policy \"nope\" for table \"docs\" does not exist",
        ),
        (
            "ALTER POLICY nope ON docs",
            "42704",
            "policy \"nope\" for table \"docs\" does not exist",
        ),
        (
            "ALTER POLICY reader ON missing USING (true)",
            "42P01",
            "relation \"missing\" does not exist",
        ),
        (
            "ALTER POLICY reader ON docs WITH CHECK (true)",
            "42601",
            "only USING expression allowed for SELECT, DELETE",
        ),
        (
            "ALTER POLICY writer ON docs USING (true)",
            "42601",
            "only WITH CHECK expression allowed for INSERT",
        ),
    ] {
        let err = admin.exec(sql, &[]).unwrap_err();
        assert_pg_error(&err, code, message);
        let err = extended_exec(&mut raw, sql).unwrap_err();
        assert_pg_error(&err, code, message);
    }

    // What it cannot read is refused, never taken for done.
    for sql in [
        "ALTER POLICY reader ON docs FOO (true)",
        "ALTER POLICY reader ON docs USING (true) TO alice",
        "ALTER POLICY reader ON docs USING ()",
        "ALTER POLICY reader ON docs RENAME",
        "ALTER POLICY reader docs USING (true)",
    ] {
        let err = admin.exec(sql, &[]).expect_err(sql);
        assert_sqlstate(&err, "42601");
    }

    let alice = connect_as(&addr, "alice", "a");
    let err = alice.exec("ALTER POLICY own ON docs USING (true)", &[]).unwrap_err();
    assert_sqlstate(&err, "42501");
    let using: String = admin
        .query_row("SELECT using_expr FROM _overlite_policies WHERE polname = 'own'", &[])
        .scan()
        .unwrap();
    assert_eq!(
        using, "owner = current_user",
        "a role that does not own the table changed nothing"
    );
}

// A policy is found by the stored name of its table, in its schema.
#[test]
fn alter_policy_in_a_schema() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE SCHEMA s1");
    must_exec(&conn, "CREATE TABLE t (id int)");
    must_exec(&conn, "CREATE TABLE s1.t (id int)");
    must_exec(&conn, "CREATE POLICY p ON t USING (id = 1)");
    must_exec(&conn, "CREATE POLICY p ON s1.t USING (id = 2)");
    must_exec(&conn, "ALTER POLICY p ON s1.t USING (id = 3)");
    let exprs = query_column(&conn, "SELECT using_expr FROM _overlite_policies ORDER BY tablename", 0);
    assert_eq!(exprs, ["id = 3", "id = 1"], "s1.t sorts before t");
}
