use crate::common::*;

// create_role_privilege: only a role with CREATEROLE (or a superuser) may
// CREATE/DROP roles; the creator gets ADMIN OPTION on what it creates.
#[test]
fn create_role_privilege() {
    let addr = start_server_env(Env::password("adminpw"));

    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN CREATEROLE PASSWORD 'a'");
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'b'");

    let alice = connect_as(&addr, "alice", "a");
    let bob = connect_as(&addr, "bob", "b");

    // alice has CREATEROLE.
    alice.exec("CREATE ROLE r1", &[]).unwrap();

    // bob does not.
    bob.exec("CREATE ROLE r2", &[]).expect_err("no CREATEROLE");
    bob.exec("DROP ROLE alice", &[]).expect_err("no CREATEROLE");

    // The creator gets ADMIN OPTION on r1, so alice may grant it on.
    alice.exec("GRANT r1 TO bob", &[]).expect("creator holds admin option");
}

// grant_admin_option: membership can only be granted by a superuser or a role
// holding ADMIN OPTION on the target role; WITH ADMIN OPTION delegates that.
#[test]
fn grant_admin_option() {
    let addr = start_server_env(Env::password("adminpw"));

    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, "CREATE ROLE alice LOGIN CREATEROLE PASSWORD 'a'");
    must_exec(&admin, "CREATE ROLE bob LOGIN PASSWORD 'b'");
    must_exec(&admin, "CREATE ROLE team");
    must_exec(&admin, "CREATE TABLE t (id int)");
    must_exec(&admin, "INSERT INTO t VALUES (1)");
    must_exec(&admin, "GRANT SELECT ON t TO team");

    let alice = connect_as(&addr, "alice", "a");

    // alice has CREATEROLE but no ADMIN OPTION on team (postgres owns it).
    alice
        .exec("GRANT team TO bob", &[])
        .expect_err("no admin option on team");

    // Delegate admin option to alice; now she can grant team.
    must_exec(&admin, "GRANT team TO alice WITH ADMIN OPTION");
    alice.exec("GRANT team TO bob", &[]).unwrap();

    // bob now inherits team's SELECT.
    let bob = connect_as(&addr, "bob", "b");
    let n: i64 = bob.query_row("SELECT count(*) FROM t", &[]).scan().unwrap();
    assert_eq!(n, 1);

    // alice can revoke too (admin option covers it).
    must_exec(&alice, "REVOKE team FROM bob");
    bob.query_row("SELECT count(*) FROM t", &[]).scan::<i64>().unwrap_err();

    // A plain member (no admin option) can't grant membership on.
    must_exec(&admin, "CREATE ROLE carol LOGIN PASSWORD 'c'");
    must_exec(&admin, "GRANT team TO carol"); // no WITH ADMIN OPTION
    let carol = connect_as(&addr, "carol", "c");
    carol
        .exec("GRANT team TO bob", &[])
        .expect_err("plain member lacks admin option");
}
