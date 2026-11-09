//! Postgres' grammar, called directly.
//!
//! `pg_query::parse` decodes the tree and then walks all of it to build a summary
//! (tables, functions, filter columns) in hash sets, which nothing here reads: on a
//! short statement that walk is a tenth of the parse. This calls libpg_query's own
//! entry point, the one `pg_query::parse` calls, and decodes the protobuf it returns
//! with the same prost types, and nothing more.
//!
//! The declarations are libpg_query's `pg_query.h` (vendored by the pg_query crate,
//! whose build links the library); a pg_query upgrade has to keep them in step.

use pg_query::protobuf::ParseResult;
use prost::Message;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int};
use std::rc::Rc;

#[repr(C)]
struct PgQueryError {
    message: *mut c_char,
    funcname: *mut c_char,
    filename: *mut c_char,
    lineno: c_int,
    cursorpos: c_int,
    context: *mut c_char,
}

#[repr(C)]
struct PgQueryProtobuf {
    len: usize,
    data: *mut c_char,
}

#[repr(C)]
struct PgQueryProtobufParseResult {
    parse_tree: PgQueryProtobuf,
    stderr_buffer: *mut c_char,
    error: *mut PgQueryError,
}

unsafe extern "C" {
    fn pg_query_parse_protobuf_opts(input: *const c_char, parser_options: c_int) -> PgQueryProtobufParseResult;
    fn pg_query_free_protobuf_parse_result(result: PgQueryProtobufParseResult);
}

/// The raw parse tree of sql, by the default (top-level SQL) grammar.
///
/// # Errors
///
/// "parse error" when Postgres would refuse the text (or it holds a NUL byte), and
/// "decode error" if the protobuf does not decode, which would be a version skew.
pub(crate) fn parse(sql: &str) -> Result<ParseResult, &'static str> {
    let input = CString::new(sql).map_err(|_| "parse error")?;
    // SAFETY: input is a NUL-terminated string that outlives the call; 0 is
    // PG_QUERY_PARSE_DEFAULT. The function returns its result by value and owns
    // every pointer in it until pg_query_free_protobuf_parse_result.
    let result = unsafe { pg_query_parse_protobuf_opts(input.as_ptr(), 0) };
    let out = if result.error.is_null() {
        let tree = if result.parse_tree.data.is_null() {
            &[][..]
        } else {
            // SAFETY: on success parse_tree.data points to parse_tree.len bytes that
            // the result owns; they are read before the result is freed below.
            unsafe { std::slice::from_raw_parts(result.parse_tree.data.cast::<u8>(), result.parse_tree.len) }
        };
        ParseResult::decode(tree).map_err(|_| "decode error")
    } else {
        Err("parse error")
    };
    // SAFETY: result came from pg_query_parse_protobuf_opts and is freed once; out
    // owns its own copy of everything decoded.
    unsafe { pg_query_free_protobuf_parse_result(result) };
    out
}

/// Statements parsed, by text, on this thread: a parse is most of what reading a
/// point lookup costs, and a tree depends on the text alone, never on the schema or
/// the session, so a client repeating a statement parses it once, and the session's
/// classifiers, its privilege check and the translation read one parse of it. Bounded
/// as the translation cache is, emptied when full.
const PARSED_ENTRIES: usize = 4096;
const PARSED_MAX_LEN: usize = 16 * 1024;

type Shared = Result<Rc<ParseResult>, &'static str>;

std::thread_local! {
    static PARSED: RefCell<HashMap<String, Shared>> = RefCell::new(HashMap::new());
}

/// `parse`, kept on the thread by text (above).
///
/// # Errors
///
/// As `parse`.
pub(crate) fn shared(sql: &str) -> Shared {
    if sql.len() > PARSED_MAX_LEN {
        return parse(sql).map(Rc::new);
    }
    if let Some(hit) = PARSED.with(|c| c.borrow().get(sql).cloned()) {
        return hit;
    }
    let tree = parse(sql).map(Rc::new);
    PARSED.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= PARSED_ENTRIES {
            c.clear();
        }
        c.insert(sql.to_string(), tree.clone());
    });
    tree
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn the_tree_is_pg_querys() {
        for sql in [
            "SELECT a, b + 1 FROM t WHERE c = $1 ORDER BY a DESC NULLS LAST",
            "INSERT INTO t (a) VALUES (E'x\\ny') RETURNING *",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "",
        ] {
            let ours = parse(sql);
            let theirs = pg_query::parse(sql, 0).map(|r| r.protobuf);
            assert_eq!(ours.ok(), theirs.ok(), "{sql}");
        }
        assert_eq!(parse("SELEC 1"), Err("parse error"));
        assert_eq!(parse("SELECT '\0'"), Err("parse error"));
    }
}
