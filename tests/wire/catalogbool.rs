use crate::common::*;

// A catalog boolean compared with a string literal behaves as in Postgres, and
// the booleans introspection selects come back typed bool, not 0/1.
#[test]
fn catalog_booleans() {
    let conn = connect_extended(&start_server());
    conn.exec(r#"CREATE TABLE t (id int PRIMARY KEY)"#, &[]).unwrap();

    // Prisma's table listing matched no table while relispartition = 'f'
    // compared 0 with 'f'.
    let (name, is_partition, has_subclass): (String, bool, bool) = conn
        .query_row(
            r#"SELECT tbl.relname,
		(tbl.relhassubclass and tbl.relkind = 'p') as is_partition,
		(tbl.relhassubclass and tbl.relkind = 'r') as has_subclass
		FROM pg_class AS tbl
		WHERE (tbl.relkind = 'r' AND tbl.relispartition = 'f') OR tbl.relkind = 'p'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(name, "t");
    assert!(!is_partition);
    assert!(!has_subclass);

    let (unique, primary): (bool, bool) = conn
        .query_row(
            r#"SELECT indisunique AS is_unique, indisprimary AS is_primary_key
		FROM pg_index WHERE indisprimary = 't'"#,
            &[],
        )
        .scan()
        .unwrap();
    assert!(unique);
    assert!(primary);
}
