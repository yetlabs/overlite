use crate::common::*;

// pg_depend_populated: real structural dependencies (index/constraint/trigger/
// policy → their table) show up in pg_depend, with oids that resolve through the
// other catalogs.
#[test]
fn pg_depend_populated() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, r#"CREATE TABLE t (id int PRIMARY KEY, x int)"#);
    must_exec(&admin, r#"CREATE INDEX ix ON t (x)"#);
    must_exec(&admin, r#"ALTER TABLE t ENABLE ROW LEVEL SECURITY"#);
    must_exec(&admin, r#"CREATE POLICY p ON t FOR SELECT USING (id > 0)"#);

    // The index depends on its table.
    let n: i64 = admin
        .query_row(
            r#"
		SELECT count(*) FROM pg_depend d
		JOIN pg_class idx ON idx.oid = d.objid
		JOIN pg_class tbl ON tbl.oid = d.refobjid
		WHERE idx.relname = 'ix' AND tbl.relname = 't' AND d.deptype = 'a'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1, "index -> table dependency");

    // The policy depends on its table (classid = pg_policy).
    let n: i64 = admin
        .query_row(
            r#"
		SELECT count(*) FROM pg_depend d
		JOIN pg_class tbl ON tbl.oid = d.refobjid
		WHERE d.classid = 3256 AND tbl.relname = 't'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(n, 1, "policy -> table dependency");
}

// pg_shdepend_populated: table ownership shows up in pg_shdepend (the shared
// dependency of an object on its owning role).
#[test]
fn pg_shdepend_populated() {
    let addr = start_server_env(Env::password("adminpw"));
    let admin = connect_as(&addr, "postgres", "adminpw");
    must_exec(&admin, r#"CREATE TABLE owned (id int)"#);

    let role: String = admin
        .query_row(
            r#"
		SELECT r.rolname FROM pg_shdepend sd
		JOIN pg_class c ON c.oid = sd.objid
		JOIN pg_roles r ON r.oid = sd.refobjid
		WHERE c.relname = 'owned' AND sd.deptype = 'o'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(role, "postgres");
}
