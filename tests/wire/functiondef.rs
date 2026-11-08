use crate::common::*;

// function_introspection: a user LANGUAGE sql function appears in pg_proc and
// its definition renders via pg_get_functiondef / _arguments / _result (what
// \df, \sf, and pg_dump read).
#[test]
fn function_introspection() {
    let conn = connect(&start_server());

    conn.exec(
        "CREATE FUNCTION add_tax(price numeric, rate numeric) RETURNS numeric
		 AS $$ SELECT price * (1 + rate) $$ LANGUAGE sql",
        &[],
    )
    .unwrap();

    // It's listed in pg_proc under public with the right arity.
    let (nargs, lang): (i64, String) = conn
        .query_row(
            "
		SELECT p.pronargs, l.lanname
		FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang
		JOIN pg_namespace n ON n.oid = p.pronamespace
		WHERE p.proname = 'add_tax' AND n.nspname = 'public'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(nargs, 2);
    assert_eq!(lang, "sql");

    // The definition renderers return the real signature and body.
    let (args, result, def): (String, String, String) = conn
        .query_row(
            "
		SELECT pg_get_function_arguments(p.oid), pg_get_function_result(p.oid), pg_get_functiondef(p.oid)
		FROM pg_proc p WHERE p.proname = 'add_tax'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(args, "price numeric, rate numeric");
    assert_eq!(result, "numeric");
    assert_contains!(def, "CREATE FUNCTION");
    assert_contains!(def, "SELECT price * (1 + rate)");

    // pg_get_expr returns a stored expression verbatim (column default text).
    conn.exec("CREATE TABLE d (id int DEFAULT 42)", &[]).unwrap();
    let expr: String = conn
        .query_row(
            "
		SELECT pg_get_expr(a.adbin, a.adrelid) FROM pg_attrdef a
		JOIN pg_class c ON c.oid = a.adrelid WHERE c.relname = 'd'",
            &[],
        )
        .scan()
        .unwrap();
    assert_eq!(expr, "42");
}
