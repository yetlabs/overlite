use crate::common::*;

// The array functions every Postgres has (array_append, array_remove, array_position
// ...) were stubs that answered NULL, and Postgres 18's array_sort and array_reverse
// were missing. Each answer here is Postgres 18's.
#[test]
fn array_functions_answer_as_postgres() {
    let conn = connect(&start_server());
    for (q, want) in [
        ("SELECT array_append(ARRAY[1], 2)", "{1,2}"),
        ("SELECT array_prepend(0, ARRAY[1])", "{0,1}"),
        ("SELECT array_cat(ARRAY[1], ARRAY[2,3])", "{1,2,3}"),
        ("SELECT array_remove(ARRAY[1,2,1], 1)", "{2}"),
        ("SELECT array_remove(ARRAY['x',NULL], NULL)", "{x}"),
        ("SELECT array_replace(ARRAY['a','b','a'], 'a', 'z')", "{z,b,z}"),
        ("SELECT array_position(ARRAY[5,6], 6)", "2"),
        ("SELECT array_position(ARRAY[1,2,3,2], 2, 3)", "4"),
        ("SELECT array_positions(ARRAY[1,2,1], 1)", "{1,3}"),
        ("SELECT array_ndims('{{1,2},{3,4}}'::int[])", "2"),
        ("SELECT array_sort(ARRAY[3,1,NULL,2])", "{1,2,3,NULL}"),
        ("SELECT array_sort(ARRAY[3,1,NULL,2], true)", "{NULL,3,2,1}"),
        ("SELECT array_sort(ARRAY[3,1,NULL,2], true, false)", "{3,2,1,NULL}"),
        ("SELECT array_sort(ARRAY[1.5, 1.25, 10])", "{1.25,1.5,10}"),
        ("SELECT array_sort(ARRAY['b','A','c'])", "{A,b,c}"),
        ("SELECT array_reverse(ARRAY[1,2,3])", "{3,2,1}"),
        ("SELECT array_reverse(ARRAY['a','b'])", "{b,a}"),
    ] {
        assert_eq!(scan_str(&conn, q), want, "{q}");
    }
    let none: Option<String> = conn
        .query_row("SELECT array_position(ARRAY[1], 9)", &[])
        .scan()
        .unwrap();
    assert_eq!(none, None);

    must_exec(&conn, "CREATE TABLE t (id int, tags text[])");
    must_exec(&conn, "INSERT INTO t VALUES (1, '{b,a}'), (2, NULL)");
    assert_eq!(
        scan_str(&conn, "SELECT array_append(tags, 'z') FROM t WHERE id = 1"),
        "{b,a,z}"
    );
    assert_eq!(
        scan_str(&conn, "SELECT array_append(tags, 'z') FROM t WHERE id = 2"),
        "{z}"
    );
    assert_eq!(scan_str(&conn, "SELECT array_sort(tags) FROM t WHERE id = 1"), "{a,b}");
}
