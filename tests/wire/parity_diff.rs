use std::collections::HashMap;

use crate::common::*;
use sha2::{Digest, Sha256};

/// What the old server, in its last build, answered to two statement corpora:
/// tests/fixtures/parity-baseline.golden.
/// `@ name` starts a corpus and a fresh server, `S` is a statement (a leading `!`
/// marks setup, and is not sent), `T` its outcome over the simple protocol, `B` over
/// the extended one with binary results (SELECTs only). A value is the outcome as
/// `quote_bytes` writes it or, past 96 characters, `#` and its SHA-256.
const GOLDEN: &str = include_str!("../fixtures/parity-baseline.golden");

/// Where the server answers otherwise, on purpose: NOTES.md, "Behavior" and "Limits".
/// The reasons are checked, not trusted: an entry that stops differing
/// fails the test, so the list cannot outlive the difference.
const KNOWN_DIFFERENCES: &[(&str, char, &str)] = &[
    (
        GRANT_BYTE,
        'T',
        "GRANT's regex stops differently at a byte outside UTF-8",
    ),
    (
        FORWARD_REF_EMPTY,
        'T',
        "a forward reference is refused where the old server took it",
    ),
    (
        FORWARD_REF_EMPTY,
        'B',
        "a forward reference is refused where the old server took it",
    ),
    (
        FORWARD_REF_AB,
        'T',
        "a forward reference is refused where the old server took it",
    ),
    (
        FORWARD_REF_AB,
        'B',
        "a forward reference is refused where the old server took it",
    ),
    (
        BAD_BYTE_PATTERN,
        'T',
        "a regex error quotes a byte outside UTF-8 as \\U0010ffff-style text",
    ),
    (
        BAD_BYTE_PATTERN,
        'B',
        "a regex error quotes a byte outside UTF-8 as \\U0010ffff-style text",
    ),
    (
        BAD_BYTE_ONLY,
        'T',
        "a regex error quotes a byte outside UTF-8 as \\U0010ffff-style text",
    ),
    (
        BAD_BYTE_ONLY,
        'B',
        "a regex error quotes a byte outside UTF-8 as \\U0010ffff-style text",
    ),
    (r#""SELECT a FROM ts WHERE id=9""#, 'B', NO_BINARY_TIME),
    (r#""SELECT b FROM ts WHERE id=9""#, 'B', NO_BINARY_TIME),
    (r#""SELECT c FROM ts WHERE id=9""#, 'B', NO_BINARY_TIME),
    (r#""SELECT d FROM ts WHERE id=9""#, 'B', NO_BINARY_TIME),
    (r#""SELECT a FROM ts WHERE id=10""#, 'B', NO_BINARY_TIME),
    (r#""SELECT b FROM ts WHERE id=10""#, 'B', NO_BINARY_TIME),
    (r#""SELECT c FROM ts WHERE id=10""#, 'B', NO_BINARY_TIME),
    (r#""SELECT d FROM ts WHERE id=10""#, 'B', NO_BINARY_TIME),
];

const NO_BINARY_TIME: &str = "a time past any offset has no binary form, and is 22P03 rather than its text";
const GRANT_BYTE: &str = r#""!GRANT \"a\xffb\" TO \"a\xffb\"""#;
const FORWARD_REF_EMPTY: &str = r#""SELECT '' ~ '\\M\\1{2}(?P<n>b)'""#;
const FORWARD_REF_AB: &str = r#""SELECT 'a b' ~ '\\M\\1{2}(?P<n>b)'""#;
const BAD_BYTE_PATTERN: &str = r#""SELECT 'a\xffb' ~ 'a\xffb'""#;
const BAD_BYTE_ONLY: &str = r#""SELECT 'a' ~ '\xff'""#;

/// Arrays and hstore in binary results: the old server sent the stored JSON under
/// format code 1, and this one sends what array_send and hstore_send do, 34 rows. A
/// stored value that is neither, 14 rows, went out as it was and is now 22P03: format 1
/// promises those bytes. Hence a count of the rows that differ rather than an entry per
/// statement, which fails if it changes either way.
const BINARY_SEND: &[&str] = &[r#""SELECT a, h FROM arr""#, r#""SELECT h FROM arr""#];
const BINARY_SEND_DIFFERENCES: usize = 48;

/// The old outcome of the statement too long to list: a pattern nested past 1000
/// groups, reported now in the regex crate's words.
const DEEP_NESTING_OLD: &str = "#d0ced82078f04f9824a55fa72abed6852abbbb53b8be6f34b35f6cfcdc8ffdca";

/// What Postgres answers here and the old server did not, by class, because the corpus
/// runs each odd name through every statement: a count of each, which fails if it
/// changes either way, rather than an entry per statement.
///
/// A Query message with a NUL inside: Postgres reads one string, and what is past it
/// makes the message malformed, 08P01.
const NUL_IN_QUERY: usize = 15;
/// CREATE and DROP DATABASE with a quoted name: Postgres takes any name, where the old
/// server refused what would not do as a file name, and cut a name at its first space.
/// The name is percent-encoded into the file's now.
const QUOTED_DATABASE_NAME: usize = 37;

/// Casts to a date or time type and to hstore: the old server left them to SQLite's
/// CAST, which reads a date as the number before its first dash, and these outcomes
/// hold that number. What they answer now is what Postgres answers: the value, 22007
/// for a text that is none, and 22009 for an offset of 16 hours or more.
const CAST_BY_AFFINITY: usize = 78;

/// `CAST(text AS TEXT[])` of a JSON array's text: the old server left it to SQLite's
/// CAST, which kept the text; the AST path reads it as the array it spells and sends a
/// text[] (stage 4 of docs/adr/0001-sql-ast.md). Postgres refuses either literal
/// (malformed array literal), which neither does.
const CAST_TO_ARRAY: usize = 30;

/// COMMENT ON a table that is not there: the old server answered success, where
/// Postgres answers 42P01, and 42601 for the one that does not scan.
const COMMENT_ON_NOTHING: usize = 21;

/// A regular expression that does not compile: 2201B now, in the text of the error
/// alone, where the old server said 42000 with SQLite's words around it.
const BAD_REGEX: usize = 2768;
/// What took its SQLSTATE from the route it failed on, or from words in its text, and
/// has Postgres' now: 42704 for a type, 3F000 for a schema, 42P01 for a table or a
/// sequence that is not there, 22P02 for a value and for malformed JSON, 0A000 for a
/// schema name a file cannot hold. And a write the old server stored as it came, that
/// the column's type does not read: an array or an hstore that is neither (22P02,
/// 42601), a timestamp past any offset (22009). The reads of those arrays that failed
/// on them answer now. Since stage 5c of the AST migration, what Postgres' grammar
/// refuses (a name `"a\"b"`) is refused with Postgres' message, and a json operator over
/// an array (`a -> 0` of a text[]) with Postgres' 42883, where the old server read the
/// array's JSON.
const STATE_BY_KIND: usize = 273;

/// A `time` column: the old server described it by its text, 25, where Postgres says
/// 1083. These hold what no time can be, which Postgres would have refused; and as such
/// a write is refused now, there as here, the rows of ts that held a timestamp past any
/// offset are not there to read.
const TIME_COLUMN: usize = 24;

/// A literal that reads as a timestamp with an offset, in no timestamp's place: the old
/// server gave its UTC wherever it stood, where Postgres keeps the text. The date
/// functions among these have an argument of no type, which is 42725 in Postgres.
const OFFSET_LITERAL: usize = 26;

/// A comparison, a match or a test in a select list: a boolean, t or f under OID 16 as
/// in Postgres, where the old server sent SQLite's integer, int8 1 or 0. Less the 14
/// `a -> 0 IS NULL` of a text[], refused now as Postgres refuses them (STATE_BY_KIND).
const BOOLEAN_RESULT: usize = 2041;

/// Whether an outcome is the old one but for booleans: in binary, each 0 or 1 int8
/// cell that is now one byte; in text, columns now described as OID 16.
/// The reads of rows the old server stored and Postgres refuses to: arrays and hstores
/// that are neither, in arr, and timestamps past any offset, ts ids 0 to 11. Every write
/// refuses them now, so what reads those rows reads none, and DELETE FROM arr deletes
/// fewer. Six reads of arr that read them with `a -> 0` are refused now, as Postgres
/// refuses a json operator over an array (STATE_BY_KIND).
const REFUSED_WRITE_ROWS: usize = 170;

fn reads_refused_rows(quoted: &str) -> bool {
    if quoted.contains("FROM arr") {
        return true;
    }
    let Some(at) = quoted.find("FROM ts WHERE id=") else {
        return false;
    };
    let digits: String = quoted[at + 17..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u32>().is_ok_and(|id| id <= 11)
}

fn boolean_outcome(want: &str, got: &[u8], shown: &str, binary: bool) -> bool {
    if binary {
        let widened: Vec<String> = shown
            .split(['[', ']', ' '])
            .map(|t| match t {
                "00" => "0000000000000000".to_string(),
                "01" => "0000000000000001".to_string(),
                t => t.to_string(),
            })
            .collect();
        let old: Vec<&str> = want.split(['[', ']', ' ']).collect();
        return shown != want && widened == old;
    }
    if want.starts_with('#') {
        // Too long to show: the old outcome is the new one with int8 1 and 0.
        let old = String::from_utf8_lossy(got)
            .replace("\":16 ", "\":20 ")
            .replace("\"t\",", "\"1\",")
            .replace("\"f\",", "\"0\",");
        return same(want, old.as_bytes());
    }
    shown.contains(r#":16 \""#) && want.contains(r#":20 \""#)
}

/// A column of an expression the statement does not name: labelled as Postgres labels
/// it (count, ?column?, the operand of a cast), where the old server sent SQLite's
/// label, the text of the expression after its rewrite. Twelve reads of ts rows that
/// held a timestamp past any offset are not here: those writes are refused now.
const POSTGRES_LABEL: usize = 123;

/// An outcome without its column labels: each `"name":oid ` before the rows.
fn unlabelled(outcome: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(outcome.len());
    let mut i = 0;
    let mut line_start = true;
    while i < outcome.len() {
        if line_start && outcome[i] == b'"' {
            // A quoted name, then `:digits `, is a label.
            let mut j = i + 1;
            while j < outcome.len() && outcome[j] != b'"' {
                j += if outcome[j] == b'\\' { 2 } else { 1 };
            }
            let mut k = j + 1;
            if outcome.get(k) == Some(&b':') {
                k += 1;
                while outcome.get(k).is_some_and(u8::is_ascii_digit) {
                    k += 1;
                }
                if outcome.get(k) == Some(&b' ') {
                    out.extend_from_slice(&outcome[j + 1..=k]);
                    i = k + 1;
                    continue;
                }
            }
            line_start = false;
        }
        if outcome[i] == b'\n' {
            line_start = true;
        }
        out.push(outcome[i]);
        i += 1;
    }
    out
}

/// Whether an outcome is the old one but for its labels. An old outcome too long to
/// keep is compared through its hash, rebuilt with the labels SQLite gave: the text of
/// each item, which the passes left as it was in these.
fn same_but_labels(want: &str, got: &[u8], sql: &[u8]) -> bool {
    if !want.starts_with('#') {
        return unlabelled(&unquote_bytes(want)) == unlabelled(got);
    }
    let old = sqlite_labelled(got, sql);
    same(want, &old) || same(want, &int_booleans(&old))
}

/// The select-list items of a plain SELECT, as written.
fn select_items(sql: &[u8]) -> Vec<&[u8]> {
    let Some(list) = sql.strip_prefix(b"SELECT ") else {
        return Vec::new();
    };
    let (mut depth, mut quote, mut from, mut out) = (0, false, 0, Vec::new());
    for (i, &c) in list.iter().enumerate() {
        match c {
            b'\'' => quote = !quote,
            _ if quote => {}
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&list[from..i]);
                from = i + 1;
            }
            b' ' if depth == 0 && list[i..].starts_with(b" FROM ") => {
                out.push(&list[from..i]);
                return out.into_iter().map(<[u8]>::trim_ascii).collect();
            }
            _ => {}
        }
    }
    out.push(&list[from..]);
    out.into_iter().map(<[u8]>::trim_ascii).collect()
}

/// got with each label replaced by the text of its item.
fn sqlite_labelled(got: &[u8], sql: &[u8]) -> Vec<u8> {
    let items = select_items(sql);
    let mut out = Vec::with_capacity(got.len());
    let mut i = 0;
    for item in items {
        let Some(rest) = got[i..].strip_prefix(b"\"") else {
            return got.to_vec();
        };
        let Some(end) = rest.windows(2).position(|w| w == b"\":") else {
            return got.to_vec();
        };
        // The one pass that changed these items' text: `~` became REGEXP.
        let item = String::from_utf8_lossy(item).replace(" ~ ", " REGEXP ");
        out.extend_from_slice(quote_bytes(item.as_bytes()).as_bytes());
        i += 1 + end + 1;
        let oid_end = got[i..]
            .iter()
            .position(|&c| c == b' ')
            .map_or(got.len(), |p| i + p + 1);
        out.extend_from_slice(&got[i..oid_end]);
        i = oid_end;
    }
    out.extend_from_slice(&got[i..]);
    out
}

/// An outcome with its booleans as the int8 1 and 0 the old server sent.
fn int_booleans(outcome: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(outcome)
        .replace("\":16 ", "\":20 ")
        .replace("\"t\",", "\"1\",")
        .replace("\"f\",", "\"0\",")
        .into_bytes()
}

fn same(want: &str, got: &[u8]) -> bool {
    match want.strip_prefix('#') {
        Some(hash) => hash == hex::encode(Sha256::digest(got)),
        None => unquote_bytes(want) == got,
    }
}

// parity_diff replays the corpora and compares each statement. The server runs with TZ=UTC, as the capture did.
#[test]
fn parity_diff() {
    let known: HashMap<(&str, char), &str> = KNOWN_DIFFERENCES.iter().map(|(s, k, why)| ((*s, *k), *why)).collect();
    let mut conn: Option<Conn> = None;
    let mut sql = Vec::new();
    let mut quoted = "";
    let (mut total, mut diffs, mut expected) = (0, Vec::new(), 0);
    let mut binary_send = 0;
    let (mut nul_in_query, mut quoted_database, mut cast_by_affinity, mut comment_on_nothing) = (0, 0, 0, 0);
    let mut cast_to_array = 0;
    let (mut bad_regex, mut state_by_kind, mut time_column, mut offset_literal) = (0, 0, 0, 0);
    let mut refused_rows = 0;
    let (mut boolean_result, mut postgres_label) = (0, 0);
    for line in GOLDEN.lines() {
        let (kind, rest) = line.split_once(' ').unwrap();
        match kind {
            "#" => {}
            "@" => {
                conn = Some(connect(&start_server_env(Env::new().with("TZ", "UTC"))));
            }
            "S" => {
                quoted = rest;
                let raw = unquote_bytes(rest);
                sql = raw.strip_prefix(b"!").map_or(raw.clone(), <[u8]>::to_vec);
            }
            "T" | "B" => {
                let conn = conn.as_ref().expect("a corpus line comes first");
                let kind = kind.chars().next().unwrap();
                total += 1;
                let got = outcome(conn, &sql, kind == 'B');
                let differs = !same(rest, &got);
                if differs && kind == 'T' && same_but_labels(rest, &got, &sql) {
                    postgres_label += 1;
                    expected += 1;
                    continue;
                }
                if differs && kind == 'B' && BINARY_SEND.contains(&quoted) {
                    binary_send += 1;
                    expected += 1;
                    continue;
                }
                if differs && quote_bytes(&got).contains("08P01 invalid message format") {
                    nul_in_query += 1;
                    expected += 1;
                    continue;
                }
                if differs
                    && (quoted.starts_with(r#""!CREATE DATABASE \""#) || quoted.starts_with(r#""!DROP DATABASE \""#))
                {
                    quoted_database += 1;
                    expected += 1;
                    continue;
                }
                if differs
                    && ["AS TIMESTAMP", "::timestamp", "AS hstore"]
                        .iter()
                        .any(|w| quoted.contains(w))
                {
                    cast_by_affinity += 1;
                    expected += 1;
                    continue;
                }
                if differs && quoted.contains("AS TEXT[])") {
                    cast_to_array += 1;
                    expected += 1;
                    continue;
                }
                if differs && quoted.starts_with(r#""!COMMENT ON TABLE"#) {
                    comment_on_nothing += 1;
                    expected += 1;
                    continue;
                }
                let shown = quote_bytes(&got);
                if differs
                    && (quoted.starts_with(r#""SELECT '2024"#)
                        || quoted.starts_with(r#""SELECT date_trunc('hour','2024"#))
                {
                    offset_literal += 1;
                    expected += 1;
                    continue;
                }
                if differs && quoted.starts_with(r#""SELECT d FROM ts"#) {
                    time_column += 1;
                    expected += 1;
                    continue;
                }
                if differs && boolean_outcome(rest, &got, &shown, kind == 'B') {
                    boolean_result += 1;
                    expected += 1;
                    continue;
                }
                if differs && shown.contains("ERR ERROR 2201B error parsing regexp") {
                    bad_regex += 1;
                    expected += 1;
                    continue;
                }
                if differs && !shown.starts_with("\"ERR ERROR ") && reads_refused_rows(quoted) {
                    refused_rows += 1;
                    expected += 1;
                    continue;
                }
                if differs && shown.starts_with("\"ERR ERROR ") && !known.contains_key(&(quoted, kind)) {
                    state_by_kind += 1;
                    expected += 1;
                    continue;
                }
                let known = known.get(&(quoted, kind)).copied();
                let deep = rest == DEEP_NESTING_OLD && quote_bytes(&got).contains("regex parse error");
                let known = known.or(deep.then_some("a regex nested past 1000 groups keeps the regex crate's wording"));
                match (differs, known) {
                    (false, None) => {}
                    (true, Some(_)) => expected += 1,
                    (false, Some(why)) => diffs.push(format!("no longer differs ({why}): {quoted} {kind}")),
                    (true, None) => {
                        let show: String = quote_bytes(&got).chars().take(300).collect();
                        let head: String = quoted.chars().take(200).collect();
                        diffs.push(format!(
                            "DIFF {head} binary={}\n  old: {rest}\n  new: {show}",
                            kind == 'B'
                        ));
                    }
                }
            }
            other => panic!("bad golden line {other:?}"),
        }
    }
    println!("{total} statements, {} differ, {expected} known", diffs.len());
    assert_eq!(
        binary_send, BINARY_SEND_DIFFERENCES,
        "array and hstore rows sent in binary"
    );
    assert_eq!(nul_in_query, NUL_IN_QUERY, "statements with a NUL inside");
    assert_eq!(
        quoted_database, QUOTED_DATABASE_NAME,
        "database statements with a quoted name"
    );
    assert_eq!(cast_by_affinity, CAST_BY_AFFINITY, "casts SQLite took by affinity");
    assert_eq!(cast_to_array, CAST_TO_ARRAY, "casts of a JSON text to an array");
    assert_eq!(comment_on_nothing, COMMENT_ON_NOTHING, "comments on what is not there");
    assert_eq!(bad_regex, BAD_REGEX, "regular expressions that do not compile");
    assert_eq!(state_by_kind, STATE_BY_KIND, "errors that took another SQLSTATE");
    assert_eq!(time_column, TIME_COLUMN, "time columns described by their text");
    assert_eq!(
        refused_rows, REFUSED_WRITE_ROWS,
        "reads of rows a refused write never stored"
    );
    assert_eq!(offset_literal, OFFSET_LITERAL, "literals with an offset that stay text");
    assert_eq!(boolean_result, BOOLEAN_RESULT, "boolean results sent as booleans");
    assert_eq!(
        postgres_label, POSTGRES_LABEL,
        "columns labelled as Postgres labels them"
    );
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
}
