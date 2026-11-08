//! Translating Postgres SQL to SQLite's, as text: [`rewrite`] takes the fast path or
//! runs the passes of `rewrite_full` in a fixed order. ARCHITECTURE.md holds the rules a
//! pass has to keep.

mod aliases;
pub(crate) mod arrays;
pub(crate) mod blank;
mod bool_cast;
pub(crate) mod casts;
pub(crate) mod catalog;
mod catalog_bool;
pub(crate) mod ctid;
mod datetime;
pub(crate) mod ddl_syntax;
pub(crate) mod default_expr;
pub(crate) mod distinct_on;
mod dml_join;
pub(crate) mod fastpath;
pub(crate) mod fts;
pub(crate) mod functions;
pub(crate) mod json;
pub(crate) mod labels;
pub(crate) mod like;
pub(crate) mod null_order;
mod offset;
pub(crate) mod operators;
pub(crate) mod row_lock;
mod set_ops;
mod srf;
pub(crate) mod strings;
mod table_funcs;
pub(crate) mod tokens;
mod typed_cast;

use crate::rewrite::aliases::{rewrite_drop_cascade, rewrite_keyword_labels};
use crate::rewrite::arrays::{
    rewrite_any_array, rewrite_any_operator, rewrite_array_subquery, rewrite_array_subscript,
    rewrite_array_to_string_subquery, rewrite_overlaps,
};
use crate::rewrite::bool_cast::rewrite_bool_casts;
use crate::rewrite::casts::{rewrite_casts, rewrite_typed_literals};
use crate::rewrite::catalog::{
    rewrite_information_schema, rewrite_obj_description, rewrite_object_defs, rewrite_pg_catalog_prefix,
};
use crate::rewrite::catalog_bool::rewrite_catalog_bool_literals;
use crate::rewrite::ctid::rewrite_ctid;
use crate::rewrite::datetime::{rewrite_extract, rewrite_now};
use crate::rewrite::ddl_syntax::{
    rewrite_alter_table_head, rewrite_citext, rewrite_deferrable_keys, rewrite_drop_trigger, rewrite_index_using,
    rewrite_serial, rewrite_tablespace,
};
use crate::rewrite::default_expr::rewrite_default_expr;
use crate::rewrite::distinct_on::rewrite_distinct_on;
use crate::rewrite::dml_join::{rewrite_delete_using, rewrite_update_from};
pub use crate::rewrite::fastpath::needs_no_rewrite;
use crate::rewrite::fts::rewrite_fts;
use crate::rewrite::functions::{
    rewrite_niladic_funcs, rewrite_sql_json_constructors, rewrite_string_agg, rewrite_trim_from,
};
use crate::rewrite::json::{
    rewrite_json_contains, rewrite_json_exists, rewrite_json_funcs, rewrite_json_path, rewrite_jsonb_collate,
};
use crate::rewrite::labels::{apply_labels, result_labels};
use crate::rewrite::like::rewrite_like;
use crate::rewrite::null_order::rewrite_null_order;
use crate::rewrite::offset::rewrite_offset;
use crate::rewrite::operators::{
    rewrite_collate, rewrite_match_operators, rewrite_math_operators, rewrite_operator_syntax, rewrite_trgm_operators,
};
use crate::rewrite::row_lock::rewrite_row_lock;
use crate::rewrite::set_ops::rewrite_set_members;
use crate::rewrite::srf::rewrite_set_returning;
use crate::rewrite::strings::{holding_literals, rewrite_escape_strings};
use crate::rewrite::table_funcs::{
    rewrite_aliased_table_funcs, rewrite_derived_aliases, rewrite_generate_series, rewrite_lateral,
    rewrite_partition_srf, rewrite_pg_options_to_table,
};
use crate::rewrite::typed_cast::rewrite_typed_casts;
use crate::types::array::{rewrite_any_array_expr, rewrite_array_cast, rewrite_array_cast_drop, rewrite_array_literal};
use crate::types::hstore::rewrite_hstore_cast;
use crate::types::numeric::rewrite_numeric_columns;
use crate::types::timestamptz::{rewrite_at_time_zone, rewrite_timestamptz_literal};
use overlite_core::text::find_from;

/// rewrite translates the handful of Postgres-isms we support into equivalent
/// SQLite. It is intentionally small and syntactic; a full SQL transpiler is a
/// non-goal.
///
/// Where NULLs sort and what LIKE matches are settled after either: a plain lookup
/// with an ORDER BY or a LIKE takes the fast path and still needs them, and each is
/// one scan of the tokens, made only when its word is there.
///
/// A statement off the fast path goes to the AST path when it covers the statement
/// (crate::ast), else to the text passes, and the result is kept per thread by the
/// statement's text: a client repeats its statements, and each thread is one client's.
pub fn rewrite(sql: &str) -> String {
    if needs_no_rewrite(sql) {
        return rewrite_like(&rewrite_null_order(sql));
    }
    cached(sql, || {
        let full = crate::ast::ENABLED
            .then(|| crate::ast::translate(sql))
            .flatten()
            .unwrap_or_else(|| rewrite_full(sql));
        rewrite_like(&rewrite_null_order(&full))
    })
}

/// How many translations a thread keeps; past it the cache starts over.
const CACHE_ENTRIES: usize = 4096;
/// A statement longer than this is translated every time: a bulk INSERT is seldom
/// sent twice, and would pin its text.
const CACHE_MAX_LEN: usize = 16 * 1024;

/// Whether the translation cache is on: OVERLITE_REWRITE_CACHE=0 turns it off.
static CACHE_ON: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("OVERLITE_REWRITE_CACHE").map_or(true, |v| v != "0"));

thread_local! {
    static CACHE: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// f's translation of sql, from this thread's cache when it has it. The translation
/// depends on the text alone (no pass reads session state), so the text is the key.
fn cached(sql: &str, f: impl FnOnce() -> String) -> String {
    if !*CACHE_ON || sql.len() > CACHE_MAX_LEN {
        return f();
    }
    if let Some(hit) = CACHE.with(|c| c.borrow().get(sql).cloned()) {
        return hit;
    }
    let out = f();
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_ENTRIES {
            c.clear();
        }
        c.insert(sql.to_string(), out.clone());
    });
    out
}

/// Runs every pass. A pass reads the statement as text, so one that finds its words by
/// a raw scan would find them in a string literal too. A pass that has no need for what
/// a literal says runs through holding_literals, which keeps it from seeing any;
/// literals_survive_the_whole_rewrite holds every pass to that, by the words that set
/// it off.
pub fn rewrite_full(sql: &str) -> String {
    // What Postgres calls the result columns, read before any pass changes the text.
    let labels = result_labels(sql);
    // Strip the pg_catalog. qualifier first, so schema-qualified type names in
    // casts (e.g. "x::pg_catalog.regtype") reduce to a bare, castable type.
    let sql = rewrite_pg_catalog_prefix(sql);
    let sql = rewrite_trgm_operators(&sql); // a % 'b', a <-> b: before casts take the literals apart
    let sql = rewrite_math_operators(&sql); // a # b, a ^ b, |/ a, ||/ a, @ a
    let sql = rewrite_set_members(&sql); // (SELECT ...) UNION (SELECT ... LIMIT 1), before what reads a select list
    let sql = rewrite_alter_table_head(&sql);
    let sql = rewrite_delete_using(&sql); // DELETE ... USING -> DELETE over the rowids the join finds
    let sql = rewrite_update_from(&sql); // UPDATE t alias -> UPDATE t AS alias, and its RETURNING
    let sql = rewrite_catalog_bool_literals(&sql); // relispartition = 'f' -> = 0
    let sql = rewrite_drop_cascade(&sql);
    let sql = rewrite_drop_trigger(&sql);
    let sql = rewrite_distinct_on(&sql);
    let sql = rewrite_serial(&sql);
    let sql = rewrite_deferrable_keys(&sql); // UNIQUE (a) DEFERRABLE -> a comment the catalog reads
    let sql = rewrite_tablespace(&sql); // TABLESPACE x: a table lives in its database's file
    let sql = rewrite_citext(&sql); // a citext column compares without case
    let sql = rewrite_numeric_columns(&sql);
    let sql = rewrite_lateral(&sql);
    let sql = holding_literals(&sql, rewrite_derived_aliases);
    let sql = rewrite_now(&sql);
    let sql = rewrite_timestamptz_literal(&sql); // offset-bearing timestamp literal -> UTC
    let sql = rewrite_at_time_zone(&sql); // expr AT TIME ZONE 'zone'
    let sql = rewrite_extract(&sql);
    let sql = rewrite_hstore_cast(&sql);
    let sql = rewrite_sql_json_constructors(&sql); // JSON_ARRAY(... RETURNING jsonb), before the build functions
    let sql = rewrite_json_funcs(&sql);
    let sql = rewrite_json_path(&sql);
    let sql = rewrite_escape_strings(&sql);
    let sql = rewrite_niladic_funcs(&sql);
    let sql = rewrite_information_schema(&sql);
    let sql = holding_literals(&sql, rewrite_obj_description); // obj_description/col_description -> pg_description lookup
    let sql = holding_literals(&sql, rewrite_index_using); // strip "USING <method>" from CREATE INDEX (pg_dump emits it)
    let sql = rewrite_operator_syntax(&sql); // unwrap OPERATOR(=)/OPERATOR(~) before ANY/regex handling
    let sql = holding_literals(&sql, rewrite_array_to_string_subquery); // before ARRAY(subquery) is unwrapped
    let sql = rewrite_set_returning(&sql); // unnest and the JSON sets, before what writes a json_each
    let sql = rewrite_array_subquery(&sql);
    let sql = holding_literals(&sql, rewrite_any_array); // literal "= ANY(ARRAY[…])" -> IN (…)
    let sql = rewrite_any_array_expr(&sql); // x = ANY(arr) -> json_each membership (before generic ANY)
    let sql = rewrite_any_operator(&sql);
    let sql = rewrite_string_agg(&sql);
    let sql = rewrite_generate_series(&sql);
    let sql = holding_literals(&sql, rewrite_pg_options_to_table);
    let sql = rewrite_aliased_table_funcs(&sql); // alias(cols) on generate_subscripts
    let sql = holding_literals(&sql, rewrite_partition_srf);
    let sql = rewrite_array_literal(&sql); // ARRAY[…] -> json_array(…)
    let sql = rewrite_array_cast(&sql); // '{…}'::type[] -> JSON literal (after unnest)
    let sql = rewrite_array_cast_drop(&sql); // expr::type[] -> expr (value already JSON)
    let sql = rewrite_typed_literals(&sql);
    let sql = rewrite_casts(&sql);
    let sql = rewrite_bool_casts(&sql); // CAST(x AS boolean) -> pg_bool(x), after :: became CAST
    let sql = rewrite_fts(&sql); // @@, a tsvector's ||, CAST(x AS tsvector), after :: became CAST
    let sql = rewrite_json_contains(&sql); // after casts, so "x::jsonb @> ..." operands are whole
    let sql = rewrite_overlaps(&sql); // "a && b" range/array overlap
    let sql = rewrite_json_exists(&sql); // ? / ?| / ?& -> jsonb_exists*(), after arrays are json_array(…)
    let sql = rewrite_array_subscript(&sql);
    let sql = holding_literals(&sql, rewrite_object_defs);
    let sql = rewrite_collate(&sql);
    let sql = rewrite_trim_from(&sql);
    let sql = rewrite_match_operators(&sql);
    let sql = rewrite_row_lock(&sql);
    let sql = rewrite_offset(&sql); // bare OFFSET -> LIMIT -1 OFFSET, after FOR UPDATE is gone
    let sql = rewrite_typed_casts(&sql); // CAST(x AS date) -> overlite_cast(x, 'date')
    let sql = rewrite_jsonb_collate(&sql); // jsonb's equality and order for ->, #> and ::jsonb, once casts are calls
    // After the function and cast rewrites, so the final expression is what gets
    // wrapped: a cast in a DEFAULT is a CAST(...) or a call by now.
    let sql = rewrite_default_expr(&sql);
    let sql = rewrite_ctid(&sql); // ctid -> rowid, the row identifier SQLite has
    let sql = rewrite_keyword_labels(&sql); // earlier rules write AS of their own (CAST)
    apply_labels(&sql, &labels) // last: the labels of what the client did not name
}

/// `sql[:a] + repl + sql[b:]`.
pub(crate) fn splice(sql: &str, a: usize, b: usize, repl: &str) -> String {
    [&sql[..a], repl, &sql[b..]].concat()
}

/// A buffer built byte by byte from whole strings back into a String. All cuts
/// happen at ASCII delimiters, so the bytes stay valid UTF-8.
pub(crate) fn utf8_string(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// The first needle at or after from, as an absolute offset.
pub(crate) fn index_from(s: &str, needle: &str, from: usize) -> Option<usize> {
    match s.get(from..) {
        Some(t) => t.find(needle).map(|p| p + from),
        None => find_from(s, needle, from),
    }
}

/// Runs f over (input, want) pairs.
#[cfg(test)]
fn assert_rewrites(f: fn(&str) -> String, cases: &[(&str, &str)]) {
    for (input, want) in cases {
        assert_eq!(f(input), *want, "input {input:?}");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// For each pass of rewrite_full, text that would set it off as code. The test
    /// below reads the passes from rewrite_full, so a new one has to come here.
    type Pass = (&'static str, fn(&str) -> String, &'static [&'static str]);

    const PASS_TRIGGERS: &[Pass] = &[
        (
            "rewrite_pg_catalog_prefix",
            rewrite_pg_catalog_prefix,
            &["pg_catalog.pg_class", "x::pg_catalog.text"],
        ),
        (
            "rewrite_trgm_operators",
            rewrite_trgm_operators,
            &["name % 'x'", "name <-> 'x'", "'x' <% name"],
        ),
        (
            "rewrite_math_operators",
            rewrite_math_operators,
            &["a # b", "a ^ 2", "|/ a", "||/ a", "@ a"],
        ),
        (
            "rewrite_set_members",
            rewrite_set_members,
            &[
                "(SELECT 1) UNION (SELECT 2 LIMIT 1)",
                "IN ((SELECT 1) EXCEPT (SELECT 2))",
            ],
        ),
        (
            "rewrite_jsonb_collate",
            rewrite_jsonb_collate,
            &["v -> 'a'", "overlite_cast(x, 'jsonb')"],
        ),
        (
            "rewrite_fts",
            rewrite_fts,
            &[
                "a @@ to_tsquery('b')",
                "setweight(v, 'A') || v",
                "CAST(x AS tsvector)",
                "CAST(x AS regconfig)",
            ],
        ),
        (
            "rewrite_alter_table_head",
            rewrite_alter_table_head,
            &["ALTER TABLE IF EXISTS ONLY t"],
        ),
        (
            "rewrite_delete_using",
            rewrite_delete_using,
            &["DELETE FROM t USING u WHERE t.a = u.a"],
        ),
        (
            "rewrite_update_from",
            rewrite_update_from,
            &["UPDATE t x SET a = u.b FROM u WHERE u.id = x.id RETURNING x.id, u.b"],
        ),
        (
            "rewrite_catalog_bool_literals",
            rewrite_catalog_bool_literals,
            &["relispartition = 'f'", "attnotnull = 't'"],
        ),
        (
            "rewrite_drop_cascade",
            rewrite_drop_cascade,
            &["DROP TABLE t CASCADE", "DROP TABLE IF EXISTS t CASCADE"],
        ),
        (
            "rewrite_drop_trigger",
            rewrite_drop_trigger,
            &["DROP TRIGGER x ON t", "DROP TRIGGER IF EXISTS x ON t"],
        ),
        (
            "rewrite_distinct_on",
            rewrite_distinct_on,
            &["SELECT DISTINCT ON (a) a, b FROM t ORDER BY a"],
        ),
        (
            "rewrite_serial",
            rewrite_serial,
            &[
                "CREATE TABLE t (id SERIAL PRIMARY KEY)",
                "ALTER TABLE t ADD c bigserial",
            ],
        ),
        ("rewrite_citext", rewrite_citext, &["CREATE TABLE t (a citext)"]),
        (
            "rewrite_tablespace",
            rewrite_tablespace,
            &["CREATE INDEX i ON t (a) TABLESPACE pg_default"],
        ),
        (
            "rewrite_deferrable_keys",
            rewrite_deferrable_keys,
            &["CREATE TABLE t (a int UNIQUE DEFERRABLE INITIALLY DEFERRED)"],
        ),
        (
            "rewrite_numeric_columns",
            rewrite_numeric_columns,
            &["CREATE TABLE t (n numeric(10,2))", "CREATE TABLE t (n numeric(10,2)[])"],
        ),
        (
            "rewrite_lateral",
            rewrite_lateral,
            &["JOIN LATERAL json_each(x)", "LATERAL"],
        ),
        (
            "rewrite_derived_aliases",
            rewrite_derived_aliases,
            &[
                "SELECT * FROM (VALUES (1, 2)) AS v(a, b)",
                "SELECT * FROM (SELECT 1, 2) v(a, b)",
            ],
        ),
        ("rewrite_now", rewrite_now, &["now()", "clock_timestamp()", "NOW( )"]),
        (
            "rewrite_timestamptz_literal",
            rewrite_timestamptz_literal,
            &["CREATE TABLE t (a timestamptz DEFAULT '2024-01-02 03:04:05+02')"],
        ),
        (
            "rewrite_at_time_zone",
            rewrite_at_time_zone,
            &["x AT TIME ZONE 'UTC'", "'2024-01-02'::timestamp at time zone z"],
        ),
        (
            "rewrite_extract",
            rewrite_extract,
            &["extract(year FROM d)", "EXTRACT (dow from now())"],
        ),
        (
            "rewrite_hstore_cast",
            rewrite_hstore_cast,
            &["'a=>1'::hstore", "x::hstore"],
        ),
        (
            "rewrite_sql_json_constructors",
            rewrite_sql_json_constructors,
            &[
                "JSON_ARRAY(1 NULL ON NULL RETURNING JSONB)",
                "JSON_OBJECT('a' VALUE 1 RETURNING JSONB)",
            ],
        ),
        (
            "rewrite_json_funcs",
            rewrite_json_funcs,
            &["jsonb_build_object('a', 1)", "json_agg(x)", "to_jsonb(x)"],
        ),
        ("rewrite_json_path", rewrite_json_path, &["x #> '{a}'", "x #>> '{a}'"]),
        ("rewrite_escape_strings", rewrite_escape_strings, &[r"E'\n'", r"e'x'"]),
        (
            "rewrite_niladic_funcs",
            rewrite_niladic_funcs,
            &["current_user", "session_user"],
        ),
        (
            "rewrite_default_expr",
            rewrite_default_expr,
            &[
                "CREATE TABLE t (a text DEFAULT lower('X'))",
                "ALTER TABLE t ADD COLUMN a text DEFAULT datetime('now')",
            ],
        ),
        (
            "rewrite_information_schema",
            rewrite_information_schema,
            &["information_schema.tables", "information_schema.columns"],
        ),
        (
            "rewrite_obj_description",
            rewrite_obj_description,
            &["obj_description(c.oid, 'pg_class')", "col_description(a, 1)"],
        ),
        (
            "rewrite_index_using",
            rewrite_index_using,
            &[
                "CREATE INDEX i ON t USING btree (a)",
                "CREATE INDEX i ON t USING gin (a)",
            ],
        ),
        (
            "rewrite_operator_syntax",
            rewrite_operator_syntax,
            &["a OPERATOR(pg_catalog.=) b", "OPERATOR(~)"],
        ),
        (
            "rewrite_array_to_string_subquery",
            rewrite_array_to_string_subquery,
            &["array_to_string(array(SELECT a FROM t), ',')"],
        ),
        (
            "rewrite_set_returning",
            rewrite_set_returning,
            &[
                "SELECT unnest(a) FROM t",
                "SELECT generate_subscripts(a, 1) FROM t",
                "SELECT * FROM unnest(ARRAY[1, 2]) AS u(x)",
                "SELECT * FROM t, jsonb_array_elements(t.j) AS e",
                "SELECT regexp_split_to_table('a', 'b')",
            ],
        ),
        (
            "rewrite_array_subquery",
            rewrite_array_subquery,
            &["ARRAY(SELECT a FROM t)", "array (select 1)"],
        ),
        (
            "rewrite_any_array",
            rewrite_any_array,
            &["x = ANY (ARRAY['a', 'b'])", "x <> ALL (ARRAY[1, 2])"],
        ),
        (
            "rewrite_any_array_expr",
            rewrite_any_array_expr,
            &["x = ANY(tags)", "x <> ALL(tags)"],
        ),
        (
            "rewrite_any_operator",
            rewrite_any_operator,
            &["= ANY (SELECT 1)", "x = any (y)"],
        ),
        (
            "rewrite_string_agg",
            rewrite_string_agg,
            &["string_agg(a, ',')", "STRING_AGG(a, ',' ORDER BY a)"],
        ),
        (
            "rewrite_generate_series",
            rewrite_generate_series,
            &["generate_series(1, 3)", "generate_series(1, 2) AS s(i)"],
        ),
        (
            "rewrite_pg_options_to_table",
            rewrite_pg_options_to_table,
            &["pg_options_to_table(x)"],
        ),
        (
            "rewrite_aliased_table_funcs",
            rewrite_aliased_table_funcs,
            &["generate_subscripts(a, 1) AS g(i)"],
        ),
        (
            "rewrite_partition_srf",
            rewrite_partition_srf,
            &["pg_partition_tree(x)", "pg_partition_ancestors(x) AS a(b, c)"],
        ),
        (
            "rewrite_array_literal",
            rewrite_array_literal,
            &["ARRAY[1, 2]", "array['a']"],
        ),
        (
            "rewrite_array_cast",
            rewrite_array_cast,
            &["'{1,2}'::int[]", "'{a}'::text[]"],
        ),
        (
            "rewrite_array_cast_drop",
            rewrite_array_cast_drop,
            &["x::int[]", "a.b::text[][]"],
        ),
        (
            "rewrite_casts",
            rewrite_casts,
            &["x::int", "'1'::regclass", "1::text::int"],
        ),
        (
            "rewrite_typed_literals",
            rewrite_typed_literals,
            &["DATE 'x'", "timestamp with time zone 'x'"],
        ),
        (
            "rewrite_typed_casts",
            rewrite_typed_casts,
            &["CAST(x AS date)", "CAST(x AS numeric(10, 2))"],
        ),
        (
            "rewrite_bool_casts",
            rewrite_bool_casts,
            &["CAST(x AS boolean)", "CAST(x AS bool)"],
        ),
        (
            "rewrite_json_contains",
            rewrite_json_contains,
            &["a @> b", "x::jsonb @> '{}'", "a <@ b"],
        ),
        ("rewrite_overlaps", rewrite_overlaps, &["a && b", "x && y"]),
        (
            "rewrite_json_exists",
            rewrite_json_exists,
            &["a ? 'k'", "a ?| b", "a ?& b"],
        ),
        (
            "rewrite_array_subscript",
            rewrite_array_subscript,
            &["arr[1]", "a[i + 1]"],
        ),
        (
            "rewrite_object_defs",
            rewrite_object_defs,
            &["pg_get_indexdef(i.oid, 0, true)", "pg_get_constraintdef(c.oid)"],
        ),
        (
            "rewrite_collate",
            rewrite_collate,
            &["a COLLATE \"C\"", "a COLLATE pg_catalog.\"C\""],
        ),
        (
            "rewrite_trim_from",
            rewrite_trim_from,
            &["trim(both ' ' from a)", "TRIM(LEADING 'x' FROM a)"],
        ),
        (
            "rewrite_match_operators",
            rewrite_match_operators,
            &["a ~ b", "a ~* 'x'", "a !~ b", "a !~* b"],
        ),
        (
            "rewrite_row_lock",
            rewrite_row_lock,
            &[
                "SELECT a FROM t FOR UPDATE",
                "SELECT a FROM t FOR NO KEY UPDATE SKIP LOCKED",
                "SELECT a FROM t FOR SHARE",
            ],
        ),
        (
            "rewrite_offset",
            rewrite_offset,
            &["SELECT a FROM t OFFSET 5", "OFFSET 1"],
        ),
        ("rewrite_ctid", rewrite_ctid, &["ctid", "t.ctid", "SELECT ctid FROM t"]),
        (
            "rewrite_keyword_labels",
            rewrite_keyword_labels,
            &["SELECT 1 AS order", "x AS group"],
        ),
    ];

    /// What each trigger is put in. The literal is an ordinary value, in an ordinary
    /// place, in a statement other passes have work on, so nothing is skipped; `@` is
    /// the literal. Each sits in a clause some pass reads.
    const LITERAL_PLACES: &[&str] = &[
        "SELECT @",
        "SELECT @, a::text FROM t WHERE b = 1",
        "SELECT a FROM t WHERE b = @ AND c::int = 1",
        "SELECT a FROM t WHERE b LIKE @ ORDER BY a OFFSET 1",
        "SELECT coalesce(a, @), ARRAY[1] FROM t",
        "SELECT a, (SELECT @) FROM t GROUP BY a HAVING a <> @",
        "INSERT INTO t (a, b) VALUES (@, now()) RETURNING a",
        "INSERT INTO t VALUES (@, 1), (@, 2) ON CONFLICT DO NOTHING",
        "UPDATE t SET a = @, b = now() WHERE c = 1",
        "DELETE FROM t WHERE a = @ AND b::int = 1",
        "WITH c AS (SELECT @ AS x) SELECT x FROM c",
        "SELECT CASE WHEN a = @ THEN 1 ELSE 2 END::text FROM t",
        "CREATE TABLE t (a text DEFAULT @, n numeric(10,2))",
        "ALTER TABLE t ADD COLUMN c text DEFAULT @",
        "CREATE VIEW v AS SELECT @ AS x",
        "SELECT @::text, ARRAY[@, 'b'] FROM t",
        "SELECT a FROM t WHERE a = ANY (ARRAY[@])",
        "SELECT a FROM t WHERE a IN (@, 'x') AND b ILIKE @",
        "SELECT string_agg(a, @), array_to_string(b, @) FROM t",
        "SELECT * FROM unnest(ARRAY[@]) AS u(x)",
        "SELECT * FROM generate_series(1, 2) AS s(i) WHERE @ = 'x'",
        "SELECT a FROM t JOIN u ON t.a = @ FOR UPDATE",
        "SELECT a /* c */ FROM t WHERE b = @ -- d",
    ];

    /// The passes of rewrite_full, in order, from its source.
    fn passes() -> Vec<String> {
        let source = include_str!("mod.rs");
        let start = source.find("fn rewrite_full").unwrap();
        let body = &source[start..source[start..].find("\n}\n").unwrap() + start];
        let mut out = Vec::new();
        for call in body.split("rewrite_").skip(2) {
            let name: String = call.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if call[name.len()..].starts_with('(') || call[name.len()..].starts_with(");") {
                out.push(format!("rewrite_{name}"));
            }
        }
        out
    }

    #[test]
    fn every_pass_has_triggers() {
        let passes = passes();
        assert!(passes.len() > 50, "{passes:?}");
        for pass in &passes {
            assert!(
                PASS_TRIGGERS.iter().any(|(p, ..)| p == pass),
                "{pass} has no triggers in PASS_TRIGGERS"
            );
        }
        let mut idle = Vec::new();
        for (pass, run, triggers) in PASS_TRIGGERS {
            assert!(passes.iter().any(|p| p == pass), "{pass} is not a pass any more");
            // A trigger that sets nothing off would prove nothing about its pass.
            for trigger in *triggers {
                if run(trigger) == *trigger {
                    idle.push(format!("{pass}: {trigger}"));
                }
            }
        }
        assert!(idle.is_empty(), "triggers that do nothing:\n{}", idle.join("\n"));
    }

    // Text inside a string literal is data. Whatever it spells, the whole rewrite
    // gives it back byte for byte.
    #[test]
    fn literals_survive_the_whole_rewrite() {
        let mut broken = Vec::new();
        let mut checked = 0;
        for (pass, _, triggers) in PASS_TRIGGERS {
            for trigger in *triggers {
                // As it is, with a quote inside, and with words around it.
                let body = trigger.replace('\'', "''");
                for lit in [format!("'{body}'"), format!("' {body} '"), format!("'x''{body}''y'")] {
                    for place in LITERAL_PLACES {
                        let sql = place.replace('@', &lit);
                        let got = rewrite(&sql);
                        checked += 1;
                        // A pass may repeat an operand; each copy still has to be whole.
                        if got.matches(lit.as_str()).count() < place.matches('@').count() {
                            broken.push(format!("{pass}: {sql}\n  -> {got}"));
                        }
                    }
                }
            }
        }
        assert!(checked > 3000, "{checked}");
        assert!(
            broken.is_empty(),
            "{} literals changed:\n{}",
            broken.len(),
            broken.join("\n")
        );
    }

    #[test]
    fn rewrite_string_literal_aware() {
        assert_rewrites(
            rewrite,
            &[
                ("SELECT nextval('e')", r#"SELECT nextval('e') AS "nextval""#),
                ("SELECT 'public.users'", r#"SELECT 'public.users' AS "?column?""#),
                ("SELECT 'now()'", r#"SELECT 'now()' AS "?column?""#),
                ("SELECT 'pg_catalog.x'", r#"SELECT 'pg_catalog.x' AS "?column?""#),
                (
                    "INSERT INTO t VALUES ('a serial number')",
                    "INSERT INTO t VALUES ('a serial number')",
                ),
            ],
        );
    }

    #[test]
    fn non_ascii_survives_every_byte_scanner() {
        assert_rewrites(
            rewrite,
            &[
                (
                    "SELECT 'ação', nome FROM clientes WHERE cidade = 'São Paulo'",
                    r#"SELECT 'ação' AS "?column?", nome FROM clientes WHERE cidade = 'São Paulo'"#,
                ),
                // The AST path reads the whole name; the text passes' scan stops at the
                // first non-ASCII byte (below).
                ("SELECT coração::text", r#"SELECT CAST("coração" AS text) AS "coração""#),
                (
                    "INSERT INTO t VALUES (E'çã', ARRAY['é','ü'])",
                    "INSERT INTO t VALUES ('çã', json_array('é','ü'))",
                ),
            ],
        );
        // Only ASCII is an operand character to the text passes: the scan stops inside
        // the word. The AST path above is what fixes it.
        assert_eq!(
            rewrite_full("SELECT coração::text"),
            r#"SELECT coraçãCAST(o AS text) AS "coração""#
        );
    }
}
