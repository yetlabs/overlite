//! Recognizing, without allocating, a statement the passes would leave unchanged.

use crate::ddl::sequences::is_ident_start;
use crate::rewrite::aliases::KEYWORD_LABELS;
use crate::rewrite::labels::is_label_word;
use std::collections::HashSet;
use std::sync::LazyLock;

/// The longest word recognized; longer ones take the full rewrite.
const MAX_WORD: usize = 32;

/// needs_no_rewrite reports whether sql is certain to come out of rewrite_full
/// unchanged, in one allocation-free pass. Rewriting costs some 60 passes, most
/// of them regular expressions, and the statements that run most often are plain
/// lookups that none of them touches.
///
/// It is deliberately narrow: anything it does not recognize answers false and
/// takes the full rewrite, so it can only be wrong by being slow. A false true
/// would skip a translation; the tests check it against every word the rewrite
/// passes mention and every statement of the wire suite.
pub fn needs_no_rewrite(sql: &str) -> bool {
    if !plain_statement(sql) || update_with_alias(sql) || !select_list_names_itself(sql) {
        return false;
    }
    let b = sql.as_bytes();
    let mut word = [0u8; MAX_WORD];
    let mut after_as = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'\'' => match skip_literal(sql, i) {
                Some(end) => i = end,
                None => return false,
            },
            b'0'..=b'9' => i += 1,
            b'$' => {
                i += 1;
                if i >= b.len() || !b[i].is_ascii_digit() {
                    return false;
                }
            }
            // "--" is a comment and "->" a JSON operator.
            b'-' => {
                if i + 1 < b.len() && (b[i + 1] == b'-' || b[i + 1] == b'>') {
                    return false;
                }
                i += 1;
            }
            b'/' => {
                if i + 1 < b.len() && b[i + 1] == b'*' {
                    return false;
                }
                i += 1;
            }
            // "<@" and "<<" style operators stop at their second character.
            b'(' | b')' | b',' | b'.' | b'=' | b'<' | b'>' | b'*' | b'+' => i += 1,
            _ if is_ident_start(c) => {
                let (end, n) = lower_word(b, i, &mut word);
                if n == 0 {
                    return false;
                }
                // lower_word only stores ASCII.
                let Ok(w) = std::str::from_utf8(&word[..n]) else {
                    return false;
                };
                if is_rewritten_word(w) || w.starts_with("pg_") || (w == "cast" && !casts_to_text(b, end)) {
                    return false;
                }
                let next = skip_blanks(b, end);
                if next < b.len() {
                    match b[next] {
                        b'(' if !is_plain_call(w) => return false,
                        b'\'' if !is_plain_before_literal(w) => return false,
                        _ => {}
                    }
                }
                // rewrite_keyword_labels quotes a reserved word used as a label.
                if after_as && LABEL_WORDS.contains(w) {
                    return false;
                }
                after_as = w == "as";
                i = end;
            }
            _ => return false, // quoted identifiers, casts, arrays, other operators
        }
    }
    true
}

/// Whether the CAST whose word ends at end is one to text, the one kind that stays
/// SQLite's. A string in it takes the full rewrite, since a parenthesis may be its text.
fn casts_to_text(b: &[u8], end: usize) -> bool {
    let mut depth = 0;
    for (i, c) in b.iter().enumerate().skip(end) {
        match c {
            b'\'' | b'"' => return false,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i >= 8 && b[i - 8..i].eq_ignore_ascii_case(b" as text");
                }
            }
            _ => {}
        }
    }
    false
}

fn plain_statement(sql: &str) -> bool {
    let b = sql.as_bytes();
    let i = skip_blanks(b, 0);
    if i >= b.len() || !is_ident_start(b[i]) {
        return false;
    }
    let mut word = [0u8; MAX_WORD];
    let (_, n) = lower_word(b, i, &mut word);
    matches!(&word[..n], b"select" | b"insert" | b"update" | b"delete")
}

/// Whether a SELECT's list holds only columns and stars, perhaps labelled, which
/// SQLite names as Postgres does: anything else gets a label from apply_labels.
pub(crate) fn select_list_names_itself(sql: &str) -> bool {
    let b = sql.as_bytes();
    let mut word = [0u8; MAX_WORD];
    let i = skip_blanks(b, 0);
    let (mut i, n) = lower_word(b, i, &mut word);
    if &word[..n] != b"select" {
        return true;
    }
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b',' | b'.' | b'*' => i += 1,
            c if is_ident_start(c) => {
                let (end, n) = lower_word(b, i, &mut word);
                if n == 0 {
                    return false;
                }
                if &word[..n] == b"from" {
                    return true;
                }
                // A word Postgres labels other than as written.
                if is_label_word(&word[..n]) {
                    return false;
                }
                i = end;
            }
            _ => return false,
        }
    }
    true
}

/// Whether an UPDATE names its table with an alias, which rewrite_update_from gives an
/// AS: anything after the table name but SET.
fn update_with_alias(sql: &str) -> bool {
    let b = sql.as_bytes();
    let mut word = [0u8; MAX_WORD];
    let i = skip_blanks(b, 0);
    let (end, n) = lower_word(b, i, &mut word);
    if &word[..n] != b"update" {
        return false;
    }
    // The table's name, qualified or not.
    let mut i = skip_blanks(b, end);
    loop {
        let (end, n) = lower_word(b, i, &mut word);
        if n == 0 {
            return true;
        }
        i = end;
        if b.get(i) != Some(&b'.') {
            break;
        }
        i += 1;
    }
    let i = skip_blanks(b, i);
    let (_, n) = lower_word(b, i, &mut word);
    &word[..n] != b"set"
}

fn skip_blanks(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// Copies the identifier at i into word, lowered, and returns where it ends and
/// its length; a length of 0 means it did not fit or is not plain ASCII.
fn lower_word(b: &[u8], mut i: usize, word: &mut [u8; MAX_WORD]) -> (usize, usize) {
    let mut n = 0;
    while i < b.len() {
        let c = match b[i] {
            c @ b'A'..=b'Z' => c + (b'a' - b'A'),
            c @ (b'a'..=b'z' | b'0'..=b'9' | b'_') => c,
            0x80.. | b'$' => return (i, 0),
            _ => return (i, n),
        };
        if n == MAX_WORD {
            return (i, 0);
        }
        word[n] = c;
        n += 1;
        i += 1;
    }
    (i, n)
}

/// Returns the position after the string literal opening at i. Three kinds take
/// the full rewrite: a backslash, whatever the literal's flavor; a colon, since a
/// bare literal that reads as a timestamp with an offset is rewritten; and the
/// spellings of a boolean, which become 1 or 0 against a catalog column.
pub(crate) fn skip_literal(sql: &str, i: usize) -> Option<usize> {
    let b = sql.as_bytes();
    let start = i + 1;
    let mut i = start;
    while i < b.len() {
        match b[i] {
            b'\\' | b':' => return None,
            b'\'' => {
                if i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                let lit = &sql[start..i];
                if BOOL_LITERALS.iter().any(|w| lit.eq_ignore_ascii_case(w)) {
                    return None;
                }
                return Some(i + 1);
            }
            _ => i += 1,
        }
    }
    None
}

const BOOL_LITERALS: [&str; 12] = ["t", "f", "true", "false", "y", "n", "yes", "no", "on", "off", "1", "0"];

/// The words rewrite_keyword_labels quotes after AS.
static LABEL_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| KEYWORD_LABELS.split('|').collect());

/// Words that may precede "(" unrewritten: SQL keywords, and functions no pass
/// touches.
fn is_plain_call(w: &str) -> bool {
    matches!(
        w,
        "and"
            | "or"
            | "not"
            | "in"
            | "where"
            | "on"
            | "from"
            | "join"
            | "having"
            | "select"
            | "values"
            | "as"
            | "by"
            | "set"
            | "exists"
            | "cast"
            | "then"
            | "else"
            | "when"
            | "case"
            | "like"
            | "between"
            | "count"
            | "min"
            | "max"
            | "coalesce"
            | "lower"
            | "upper"
            | "length"
            | "abs"
    )
}

/// Words that may precede a string literal. Any other could be a prefix or a type
/// name ("DATE '2024-01-01'", "E'\n'").
fn is_plain_before_literal(w: &str) -> bool {
    matches!(
        w,
        "and"
            | "or"
            | "not"
            | "in"
            | "where"
            | "on"
            | "select"
            | "values"
            | "by"
            | "set"
            | "then"
            | "else"
            | "when"
            | "case"
            | "like"
            | "between"
            | "limit"
    )
}

/// Words whose mere presence can change the output of a pass.
fn is_rewritten_word(w: &str) -> bool {
    matches!(
        w,
        "all"
            | "any"
            | "array"
            | "at"
            | "collate"
            | "ctid"
            | "current_catalog"
            | "current_database"
            | "current_date"
            | "current_role"
            | "current_schema"
            | "current_time"
            | "current_timestamp"
            | "current_user"
            | "date"
            | "default"
            | "distinct"
            | "extract"
            | "false"
            | "for"
            | "ilike"
            | "information_schema"
            | "interval"
            | "lateral"
            | "localtime"
            | "localtimestamp"
            | "now"
            | "nulls"
            | "offset"
            | "operator"
            | "overlaps"
            | "returning"
            | "session_user"
            | "similar"
            | "some"
            | "time"
            | "timestamp"
            | "timestamptz"
            | "true"
            | "using"
            | "zone"
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::rewrite_full;
    use rand::{Rng, SeedableRng, rngs::StdRng};
    use std::collections::BTreeSet;
    use std::path::Path;

    fn check(sql: &str) {
        if needs_no_rewrite(sql) {
            assert_eq!(rewrite_full(sql), sql, "skipped a rewrite of {sql:?}");
        }
    }

    /// Every word in this module's source: a pass cannot react to one not here.
    fn source_words() -> Vec<String> {
        let mut out = BTreeSet::new();
        collect_source_words(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src")), &mut out);
        out.into_iter().collect()
    }

    fn collect_source_words(dir: &Path, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            if path.is_dir() {
                collect_source_words(&path, out);
                continue;
            }
            let src = std::fs::read_to_string(path).unwrap();
            for w in src.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
                if !w.is_empty() && w.len() < MAX_WORD && !w.as_bytes()[0].is_ascii_digit() {
                    out.insert(w.to_ascii_lowercase());
                }
            }
        }
    }

    #[test]
    fn never_skips_a_rewrite_of_any_known_word() {
        let templates = [
            "SELECT @ FROM t",
            "SELECT @ a FROM t",
            "SELECT a @ b FROM t",
            "SELECT a AS @ FROM t",
            "SELECT @(a) FROM t",
            "SELECT @ (a, 'x') FROM t",
            "SELECT a FROM @",
            "SELECT a FROM t @",
            "SELECT a FROM t @ 1",
            "SELECT a FROM t @ b",
            "SELECT a FROM t WHERE @ = 1",
            "SELECT a FROM @ WHERE b = 'f'",
            "SELECT a FROM t WHERE @ = 'f'",
            "SELECT a FROM t WHERE @ = 't'",
            "SELECT a FROM t WHERE @ = 'yes'",
            "SELECT a FROM t WHERE @ = '0'",
            "SELECT a FROM t WHERE @ <> 'on'",
            "SELECT a FROM t WHERE b = @ 'x'",
            "SELECT a FROM t WHERE b @ c",
            "SELECT a FROM t WHERE b @ 'x'",
            "SELECT a FROM t ORDER BY a @",
            "SELECT a FROM t ORDER BY a @ 1",
            "SELECT a FROM t LIMIT 1 @ 2",
            "SELECT a FROM t JOIN @ ON a = b",
            "SELECT a FROM t, @(b)",
            "SELECT a FROM t WHERE @.b = 1",
            "SELECT @.a FROM t",
            "UPDATE t SET a = @",
            "UPDATE @ SET a = 1",
            "UPDATE t SET @ = 1 @ a",
            "DELETE FROM t WHERE @",
            "DELETE FROM @",
            "INSERT INTO @ (a) VALUES (1)",
            "INSERT INTO t (@) VALUES (@)",
            "INSERT INTO t (a) VALUES (1) @ a",
            "SELECT a FROM t WHERE b @ (1, 2)",
        ];
        for w in source_words() {
            for t in templates {
                check(&t.replace('@', &w));
                check(&t.replace('@', &w.to_ascii_uppercase()));
            }
        }
    }

    #[test]
    fn never_skips_a_rewrite_of_a_statement_in_the_suite() {
        let corpus = include_str!("../../../../tests/fixtures/rewrite-corpus.hex");
        let (mut checked, mut skipped) = (0, 0);
        for line in corpus.lines() {
            let raw = hex::decode(line).unwrap();
            let sql = String::from_utf8_lossy(&raw);
            checked += 1;
            if needs_no_rewrite(&sql) {
                skipped += 1;
            }
            check(&sql);
            check(sql.trim());
        }
        assert!(checked > 500, "{checked}");
        assert!(
            skipped > 100,
            "the check must reach the fast path to prove anything: {skipped}"
        );
    }

    #[test]
    fn never_skips_a_rewrite_of_random_statements() {
        let words = source_words();
        let tokens = [
            "(",
            ")",
            ",",
            ".",
            "=",
            "<",
            ">",
            "*",
            "+",
            "-",
            "/",
            "1",
            "$1",
            "'x'",
            "'f'",
            "'t'",
            "'true'",
            "'false'",
            "'yes'",
            "'no'",
            "'on'",
            "'off'",
            "'1'",
            "'0'",
            "'Y'",
            "'N'",
            "'2024-01-01 00:00:00+03'",
            "''",
            "<=",
            ">=",
            "<>",
            "<@",
            "<<",
            ">>",
            "=>",
        ];
        let starts = ["SELECT", "INSERT INTO t", "UPDATE t SET", "DELETE FROM t WHERE"];
        let mut r = StdRng::seed_from_u64(1);
        for _ in 0..200_000 {
            let mut s = starts[r.gen_range(0..starts.len())].to_string();
            for _ in 0..r.gen_range(1..=10) {
                s.push(' ');
                if r.gen_bool(0.5) {
                    s.push_str(&words[r.gen_range(0..words.len())]);
                } else {
                    s.push_str(tokens[r.gen_range(0..tokens.len())]);
                }
            }
            check(&s);
        }
    }

    #[test]
    fn takes_plain_statements() {
        for sql in [
            "SELECT cnpj, razao_social FROM cnpj WHERE cnpj >= CAST(12345678 AS text) ORDER BY cnpj LIMIT 1",
            "SELECT cnpj, razao_social FROM cnpj WHERE cnpj = $1",
            "select a, b from t where a = 'it''s' and b like 'x%' limit 10",
            "SELECT razao_social FROM t WHERE razao_social LIKE '%PADARIA%'",
            "SELECT a.x FROM a JOIN b ON a.id = b.id WHERE a.y IN (1, 2) ORDER BY a.x DESC",
            "SELECT a FROM t WHERE b = 'ação'",
            "INSERT INTO t VALUES ($1, 'x')",
            "UPDATE t SET a = a + 1 WHERE id = $1",
            "DELETE FROM t WHERE id = $1",
        ] {
            assert!(needs_no_rewrite(sql), "{sql}");
            assert_eq!(rewrite_full(sql), sql);
        }
    }

    #[test]
    fn leaves_the_rest_to_the_rewrite() {
        for sql in [
            "SELECT a::text FROM t",
            "SELECT now()",
            "SELECT count(*) FROM t WHERE razao_social LIKE '%PADARIA%'",
            "SELECT a IS NULL FROM t",
            "SELECT NOT a FROM t",
            "SELECT \"a\" FROM t",
            "SELECT a FROM t OFFSET 5",
            "SELECT a FROM t FOR UPDATE",
            "SELECT a FROM t WHERE b ILIKE 'x'",
            "SELECT a -> 'b' FROM t",
            "SELECT a FROM t -- c",
            "SELECT a /* c */ FROM t",
            "SELECT E'x'",
            "SELECT a FROM t;",
            "SELECT a FROM pg_class",
            "CREATE TABLE t (a int)",
            "SELECT 'unterminated",
            "SELECT interval '1 day'",
            "SELECT ctid FROM t",
            "SELECT t.CTID FROM t",
            "SELECT a[1] FROM t",
            "SELECT 'a\\'b'",
            "SELECT ação FROM t",
        ] {
            assert!(!needs_no_rewrite(sql), "{sql}");
        }
    }
}
