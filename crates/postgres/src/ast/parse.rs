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
        if nesting_exceeds(tree, MAX_NESTING) {
            Err(TOO_DEEP)
        } else {
            ParseResult::decode(tree).map_err(|_| "decode error")
        }
    } else {
        Err("parse error")
    };
    // SAFETY: result came from pg_query_parse_protobuf_opts and is freed once; out
    // owns its own copy of everything decoded.
    unsafe { pg_query_free_protobuf_parse_result(result) };
    out
}

/// Postgres' own error for sql, which `parse` refused ("parse error" or [`TOO_DEEP`]):
/// its message, its SQLSTATE (42601, 54001), and the position it points at, in
/// characters from 1 as the ErrorResponse's P field counts them. None when sql parses.
pub(crate) fn error_of(sql: &str) -> Option<crate::error::Error> {
    use crate::error::Error;
    use overlite_core::SqlState;
    let Ok(input) = CString::new(sql) else {
        return Some(Error::with_state(
            SqlState::CHARACTER_NOT_IN_REPERTOIRE,
            "invalid byte sequence for encoding \"UTF8\": 0x00",
        ));
    };
    // SAFETY: as in `parse`.
    let result = unsafe { pg_query_parse_protobuf_opts(input.as_ptr(), 0) };
    let out = if result.error.is_null() {
        let tree = if result.parse_tree.data.is_null() {
            &[][..]
        } else {
            // SAFETY: as in `parse`.
            unsafe { std::slice::from_raw_parts(result.parse_tree.data.cast::<u8>(), result.parse_tree.len) }
        };
        nesting_exceeds(tree, MAX_NESTING).then(too_deep)
    } else {
        // SAFETY: error is non-null and points to the result's PgQueryError, whose
        // message is a NUL-terminated string; both live until the result is freed.
        let (message, cursor) = unsafe {
            let e = &*result.error;
            let message = if e.message.is_null() {
                "syntax error".to_string()
            } else {
                std::ffi::CStr::from_ptr(e.message).to_string_lossy().into_owned()
            };
            (message, e.cursorpos)
        };
        // The grammar's own hint for the one refusal that has one.
        let hint = (message == "LIMIT #,# syntax is not supported").then_some("Use separate LIMIT and OFFSET clauses.");
        if message.starts_with(TOO_DEEP) {
            // The grammar's own stack check, past what the protobuf bound sees.
            return_after_free(result);
            return Some(too_deep());
        }
        let mut err = Error::with_state(grammar_state(&message), message);
        if let Ok(at) = u32::try_from(cursor)
            && at > 0
        {
            err = err.at_position(at);
        }
        if let Some(h) = hint {
            err = err.with_hint(h);
        }
        Some(err)
    };
    return_after_free(result);
    out
}

/// The SQLSTATE of an error Postgres' grammar or scanner raises, by its message:
/// libpg_query hands over the message and position, not the code. All but these
/// (gram.y's and scan.l's other codes) are 42601.
fn grammar_state(message: &str) -> overlite_core::SqlState {
    use overlite_core::SqlState as S;
    // A message, its `%s` read as any text: (before it, after it, the code). One
    // without a `%s` (nothing after it, and no space ending what is before) is whole.
    const CODES: &[(&str, &str, S)] = &[
        ("role name \"", "\" is reserved", S::RESERVED_NAME),
        ("", " cannot be used as a role name here", S::RESERVED_NAME),
        ("", " constraints cannot be marked DEFERRABLE", S::FEATURE_NOT_SUPPORTED),
        ("", " constraints cannot be marked ENFORCED", S::FEATURE_NOT_SUPPORTED),
        (
            "",
            " constraints cannot be marked NOT ENFORCED",
            S::FEATURE_NOT_SUPPORTED,
        ),
        ("", " constraints cannot be marked NO INHERIT", S::FEATURE_NOT_SUPPORTED),
        ("", " constraints cannot be marked NOT VALID", S::FEATURE_NOT_SUPPORTED),
        ("CREATE ASSERTION is not yet implemented", "", S::FEATURE_NOT_SUPPORTED),
        (
            "CREATE EXTENSION ... FROM is no longer supported",
            "",
            S::FEATURE_NOT_SUPPORTED,
        ),
        (
            "CREATE OR REPLACE CONSTRAINT TRIGGER is not supported",
            "",
            S::FEATURE_NOT_SUPPORTED,
        ),
        (
            "CREATE SCHEMA IF NOT EXISTS cannot include schema elements",
            "",
            S::FEATURE_NOT_SUPPORTED,
        ),
        ("MATCH PARTIAL not yet implemented", "", S::FEATURE_NOT_SUPPORTED),
        (
            "UNENCRYPTED PASSWORD is no longer supported",
            "",
            S::FEATURE_NOT_SUPPORTED,
        ),
        ("UNIQUE predicate is not yet implemented", "", S::FEATURE_NOT_SUPPORTED),
        (
            "WITH CHECK OPTION not supported on recursive views",
            "",
            S::FEATURE_NOT_SUPPORTED,
        ),
        (
            "a column list with ",
            " is only supported for ON DELETE actions",
            S::FEATURE_NOT_SUPPORTED,
        ),
        ("aggregates cannot have output arguments", "", S::FEATURE_NOT_SUPPORTED),
        (
            "an ordered-set aggregate with a VARIADIC direct argument must have one VARIADIC aggregated argument of the same data type",
            "",
            S::FEATURE_NOT_SUPPORTED,
        ),
        ("current database cannot be changed", "", S::FEATURE_NOT_SUPPORTED),
        (
            "dropping an enum value is not implemented",
            "",
            S::FEATURE_NOT_SUPPORTED,
        ),
        (
            "unsafe use of string constant with Unicode escapes",
            "",
            S::FEATURE_NOT_SUPPORTED,
        ),
        (
            "column number must be in range from 1 to ",
            "",
            S::INVALID_PARAMETER_VALUE,
        ),
        ("precision for type float must be ", "", S::INVALID_PARAMETER_VALUE),
        ("unrecognized JSON encoding: ", "", S::INVALID_PARAMETER_VALUE),
        ("unrecognized partitioning strategy ", "", S::INVALID_PARAMETER_VALUE),
        ("frame end cannot be UNBOUNDED PRECEDING", "", S::WINDOWING_ERROR),
        ("frame start cannot be UNBOUNDED FOLLOWING", "", S::WINDOWING_ERROR),
        ("frame starting from ", "", S::WINDOWING_ERROR),
        ("invalid Unicode escape", "", S::INVALID_ESCAPE_SEQUENCE),
        (
            "unsafe use of \\' in a string literal",
            "",
            S::NONSTANDARD_USE_OF_ESCAPE_CHARACTER,
        ),
        (
            "modulus for hash partition provided more than once",
            "",
            S::DUPLICATE_OBJECT,
        ),
        (
            "remainder for hash partition provided more than once",
            "",
            S::DUPLICATE_OBJECT,
        ),
    ];
    CODES
        .iter()
        .find(|(a, b, _)| {
            if b.is_empty() && !a.ends_with(' ') {
                return message == *a;
            }
            message.starts_with(a) && message.ends_with(b) && message.len() >= a.len() + b.len()
        })
        .map_or(S::SYNTAX_ERROR, |c| c.2)
}

fn return_after_free(result: PgQueryProtobufParseResult) {
    // SAFETY: result came from pg_query_parse_protobuf_opts and is freed once.
    unsafe { pg_query_free_protobuf_parse_result(result) };
}

/// Postgres' 54001 for a statement nested too deep.
fn too_deep() -> crate::error::Error {
    crate::error::Error::with_state(overlite_core::SqlState::STATEMENT_TOO_COMPLEX, TOO_DEEP).with_hint(
        "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), \
         after ensuring the platform's stack depth limit is adequate.",
    )
}

/// What `parse` answers for a tree nested deeper than [`MAX_NESTING`]: Postgres'
/// own words for it (54001), which the session sends as they are.
pub(crate) const TOO_DEEP: &str = "stack depth limit exceeded";

/// The deepest nesting of messages a tree may have, in protobuf messages (a level of
/// an expression is two or three of them). prost decodes without its own limit of
/// 100 (`no-recursion-limit`), which stopped a 99-member UNION; this bound is what
/// keeps the decode and every walk of the tree after it (the printer is the deepest:
/// some 8 KiB of stack a level) on a connection's 8 MiB stack, with room left.
pub(crate) const MAX_NESTING: usize = 1000;

/// Whether the protobuf message buf nests more than limit messages deep, read without
/// recursion and without its schema: every length-delimited field is taken for a
/// message as far as it reads as one, so a string can only make the answer larger.
/// Each level costs at least two bytes (a tag and a length), so a shorter buffer is
/// not read at all.
fn nesting_exceeds(buf: &[u8], limit: usize) -> bool {
    if buf.len() < 2 * limit {
        return false;
    }
    fn varint(buf: &[u8], i: &mut usize) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *buf.get(*i)?;
            *i += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Some(v);
            }
        }
        None
    }
    // The ends of the messages being read, innermost last.
    let mut ends: Vec<usize> = vec![buf.len()];
    let mut i = 0;
    while let Some(&end) = ends.last() {
        if i >= end {
            i = end;
            ends.pop();
            continue;
        }
        let Some(tag) = varint(buf, &mut i) else {
            // Not a message after all: a string's bytes. Go on after it.
            i = end;
            ends.pop();
            continue;
        };
        let ok = match tag & 7 {
            0 => varint(buf, &mut i).is_some(),
            1 => {
                i += 8;
                true
            }
            5 => {
                i += 4;
                true
            }
            2 => match varint(buf, &mut i).and_then(|n| usize::try_from(n).ok()) {
                Some(n) if i.checked_add(n).is_some_and(|e| e <= end) => {
                    ends.push(i + n);
                    if ends.len() > limit + 1 {
                        return true;
                    }
                    true
                }
                _ => false,
            },
            _ => false,
        };
        if !ok || i > end {
            i = end;
            ends.pop();
        }
    }
    false
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

/// A parse `shared` keeps, or a new one, not kept: what the translation reads. A
/// statement it translates is mostly one the cache of translations holds after, or one
/// a client writes its values into, which no parse kept would serve again.
///
/// # Errors
///
/// As `parse`.
pub(crate) fn kept_or_new(sql: &str) -> Shared {
    if let Some(hit) = PARSED.with(|c| c.borrow().get(sql).cloned()) {
        return hit;
    }
    parse(sql).map(Rc::new)
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

    #[test]
    fn a_deep_tree_is_refused_before_it_is_decoded() {
        // A 200-member UNION nests one SelectStmt per member: within the bound.
        let union = (0..200)
            .map(|i| format!("(SELECT {i})"))
            .collect::<Vec<_>>()
            .join(" UNION ");
        assert!(parse(&union).is_ok());
        // Nested subqueries, five messages a level: past it, and refused whole.
        let deep = format!("SELECT {}1{}", "(SELECT ".repeat(1500), ")".repeat(1500));
        assert_eq!(parse(&deep), Err(super::TOO_DEEP));
        let shallow = format!("SELECT {}1{}", "(SELECT ".repeat(100), ")".repeat(100));
        assert!(parse(&shallow).is_ok());
    }
}
