//! Answers of statements the AST path now translates (docs/adr/0001-sql-ast.md, stage
//! 5a: the catalog's reads and the row shapes left over from stage 4): each failed
//! before the stage, held to Postgres 17's answer.

use crate::common::*;

/// Each row's columns as text joined by commas, over the simple protocol.
#[track_caller]
fn rows(conn: &Conn, sql: &str) -> Vec<String> {
    raw_rows(conn, sql)
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|c| c.map_or_else(|| "NULL".into(), |b| String::from_utf8_lossy(&b).into_owned()))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

// A reg* literal names its object as Postgres reads a name: an unquoted name folded,
// a quoted one whole, a schema before a dot dropped. The text pass compared an
// unquoted name with its case and cut `public."My T"` at its last quote.
#[test]
fn a_regclass_literal_reads_its_name_as_postgres_does() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE \"My T\" (a int)");
    must_exec(&conn, "CREATE TABLE mytab (a int)");
    for (sql, want) in [
        (
            "SELECT relname FROM pg_class WHERE oid = 'public.\"My T\"'::regclass",
            "My T",
        ),
        ("SELECT relname FROM pg_class WHERE oid = 'MyTab'::regclass", "mytab"),
    ] {
        assert_eq!(rows(&conn, sql), [want], "{sql}");
    }
}

// An array cast to text is Postgres' text form of it, and a slice of any expression
// is the elements it takes: the cast gave the JSON array the value is stored as, and
// a slice of anything but a column reached SQLite as Postgres' syntax.
#[test]
fn arrays_go_to_text_in_postgres_form_and_slice_from_any_expression() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE ta (id int, tags text[])");
    must_exec(&conn, "INSERT INTO ta VALUES (1, '{x,\"y z\"}')");
    for (sql, want) in [
        (
            "SELECT ARRAY[1,2]::text, ARRAY['a b','c']::text, (ARRAY[1,2,3])[2:3], (string_to_array('9;2;3', ';'))[2:3]",
            "{1,2},{\"a b\",c},{2,3},{2,3}",
        ),
        (
            "SELECT tags::text, (tags)[1:1]::text, array_agg(id)::text FROM ta GROUP BY tags",
            "{x,\"y z\"},{x},{1}",
        ),
    ] {
        assert_eq!(rows(&conn, sql), [want], "{sql}");
    }
}

// jsonb_typeof and json_typeof say Postgres' names: the text pass wrote SQLite's
// json_type, which says integer, real, text, true and false.
#[test]
fn json_typeof_names_types_as_postgres_does() {
    let conn = connect(&start_server());
    assert_eq!(
        rows(
            &conn,
            "SELECT jsonb_typeof('1'::jsonb), jsonb_typeof('\"a\"'), jsonb_typeof('true'), json_typeof('1.5'), \
             jsonb_typeof('null'), jsonb_typeof('[]')"
        ),
        ["number,string,boolean,number,null,array"]
    );
}

// Text search queries join with `&&` and `||` and negate with `!!`: the text pass
// took `&&` for an overlap, `||` for a concatenation of their texts, and left `!!`
// to SQLite, a syntax error.
#[test]
fn text_search_queries_combine() {
    let conn = connect(&start_server());
    for (sql, want) in [
        (
            "SELECT (plainto_tsquery('simple','a') && plainto_tsquery('simple','b'))::text, \
             (plainto_tsquery('simple','a') || plainto_tsquery('simple','b'))::text, \
             (!! plainto_tsquery('simple','a'))::text",
            "'a' & 'b','a' | 'b',!'a'",
        ),
        (
            "SELECT to_tsvector('simple','a c') @@ (plainto_tsquery('simple','a') && plainto_tsquery('simple','b')), \
             to_tsvector('simple','a c') @@ (plainto_tsquery('simple','a') || plainto_tsquery('simple','b')), \
             to_tsvector('simple','a c') @@ !! plainto_tsquery('simple','b')",
            "f,t,t",
        ),
    ] {
        assert_eq!(rows(&conn, sql), [want], "{sql}");
    }
}

// The bitwise and math operators of integers keep Postgres' result types: `5 & 3` is
// an integer (int4), a shift of a bigint a bigint, where the text passes' answer went
// out as an int8 whatever its operands.
#[test]
fn bitwise_operators_answer_at_their_operands_width() {
    let server = start_server();
    let conn = connect(&server);
    assert_eq!(
        rows(&conn, "SELECT 5 & 3, 5 | 3, 1 << 4, 5 # 3, 2 ^ 3, |/ 25, @ -5"),
        ["1,7,16,6,8,5,5"]
    );
    let r = conn.pg_exec("SELECT 5 & 3, 1::int8 << 2").unwrap();
    let types: Vec<u32> = r[0].fields.iter().map(|f| f.data_type_oid).collect();
    assert_eq!(types, [23, 20]);
}

// psql's `\d` of a table reads its triggers with pg_partition_ancestors(...) WITH
// ORDINALITY, which the text pass left to SQLite, a syntax error that failed `\d` of
// every table with a foreign key. overlite has no partitions: no ancestors, no rows.
#[test]
fn psql_reads_the_triggers_of_a_table_with_a_foreign_key() {
    let conn = connect(&start_server());
    must_exec(&conn, "CREATE TABLE ta2 (id int PRIMARY KEY)");
    must_exec(&conn, "CREATE TABLE tq (a int REFERENCES ta2(id))");
    let sql = "SELECT t.tgname, pg_catalog.pg_get_triggerdef(t.oid, true), t.tgenabled, t.tgisinternal,
  CASE WHEN t.tgparentid != 0 THEN
    (SELECT u.tgrelid::pg_catalog.regclass
     FROM pg_catalog.pg_trigger AS u,
          pg_catalog.pg_partition_ancestors(t.tgrelid) WITH ORDINALITY AS a(relid, depth)
     WHERE u.tgname = t.tgname AND u.tgrelid = a.relid
           AND u.tgparentid = 0
     ORDER BY a.depth LIMIT 1)
  END AS parent
FROM pg_catalog.pg_trigger t
WHERE t.tgrelid = 'tq'::regclass AND (NOT t.tgisinternal OR (t.tgisinternal AND t.tgenabled = 'D'))
ORDER BY 1";
    assert_eq!(rows(&conn, sql), Vec::<String>::new());
}
