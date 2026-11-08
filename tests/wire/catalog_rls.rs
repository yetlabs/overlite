use crate::common::*;

// pg_auth_members_populated: role membership shows up in pg_auth_members with
// oids that resolve back through pg_roles (this is what psql's \du "Member of"
// reads).
#[test]
fn pg_auth_members_populated() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, r#"CREATE ROLE team"#);
    must_exec(&admin, r#"CREATE ROLE alice LOGIN PASSWORD 'a'"#);
    must_exec(&admin, r#"GRANT team TO alice WITH ADMIN OPTION"#);

    let (member, role, admn): (String, String, i64) = admin
        .query_row(
            r#"
		SELECT mo.rolname, ro.rolname, m.admin_option
		FROM pg_auth_members m
		JOIN pg_roles ro ON ro.oid = m.roleid
		JOIN pg_roles mo ON mo.oid = m.member
		WHERE ro.rolname = 'team'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(member, "alice");
    assert_eq!(role, "team");
    assert_eq!(admn, 1, "granted WITH ADMIN OPTION");
}

// pg_policy_populated: a CREATE POLICY is visible in pg_policy joined to
// pg_class (what psql's \d reads to list policies).
#[test]
fn pg_policy_populated() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, r#"CREATE TABLE t (id int)"#);
    must_exec(&admin, r#"ALTER TABLE t ENABLE ROW LEVEL SECURITY"#);
    must_exec(&admin, r#"CREATE POLICY p ON t FOR SELECT USING (id > 0)"#);

    let (name, cmd, permissive, qual): (String, String, i64, String) = admin
        .query_row(
            r#"
		SELECT pol.polname, pol.polcmd, pol.polpermissive, pol.polqual
		FROM pg_policy pol
		JOIN pg_class c ON c.oid = pol.polrelid
		WHERE c.relname = 't'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(name, "p");
    assert_eq!(cmd, "r", "FOR SELECT -> 'r'");
    assert_eq!(permissive, 1);
    assert_contains!(qual, "id > 0");
}
