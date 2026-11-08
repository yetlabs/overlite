//! The tokens of a DDL statement the session runs itself: names read as Postgres
//! reads them, whole when quoted and folded when not, so that a space or a keyword
//! inside a quoted name stays part of the name.

use crate::error::{Error, Result};
use crate::rewrite::strings::end_of_string_literal;
use crate::rewrite::tokens::is_word_char;
use overlite_core::SqlState;

/// One token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Word {
    /// An unquoted word, folded to lower case: a keyword or a name.
    Bare(String),
    /// A double-quoted name, as it reads between the quotes. Never a keyword.
    Quoted(String),
    /// A string literal, as it reads between the quotes.
    Str(String),
    /// Any other single character.
    Punct(char),
}

impl Word {
    /// Whether this is the keyword kw, which is in lower case.
    pub(crate) fn is(&self, kw: &str) -> bool {
        matches!(self, Word::Bare(w) if w == kw)
    }

    /// The name this token spells, if it can be a name at all.
    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Word::Bare(w) | Word::Quoted(w) => Some(w),
            _ => None,
        }
    }
}

/// The tokens of sql, comments left out.
///
/// # Errors
///
/// 42601 for a quoted name that is empty or never closed, as Postgres' scanner.
pub(crate) fn ddl_words(sql: &str) -> Result<Vec<Word>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            c if c.is_ascii_whitespace() => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => i = crate::rewrite::blank::skip_line_comment(sql, i),
            b'/' if b.get(i + 1) == Some(&b'*') => i = crate::rewrite::blank::skip_block_comment(sql, i).0,
            b'\'' => {
                let end = end_of_string_literal(sql, i);
                let inner = sql[i + 1..end].strip_suffix('\'').unwrap_or(&sql[i + 1..end]);
                out.push(Word::Str(inner.replace("''", "'")));
                i = end;
            }
            b'"' => {
                let (name, end) = quoted_name(sql, i)?;
                out.push(Word::Quoted(name));
                i = end;
            }
            c if is_word_char(c) || c >= 0x80 => {
                let start = i;
                while i < b.len() && (is_word_char(b[i]) || b[i] >= 0x80) {
                    i += 1;
                }
                out.push(Word::Bare(sql[start..i].to_ascii_lowercase()));
            }
            c => {
                out.push(Word::Punct(c as char));
                i += 1;
            }
        }
    }
    Ok(out)
}

/// The name of the quoted identifier opening at i, and the index past it.
fn quoted_name(sql: &str, i: usize) -> Result<(String, usize)> {
    let b = sql.as_bytes();
    let mut name = String::new();
    let mut start = i + 1;
    let mut j = start;
    while j < b.len() {
        if b[j] != b'"' {
            j += 1;
            continue;
        }
        name.push_str(&sql[start..j]);
        if b.get(j + 1) != Some(&b'"') {
            if name.is_empty() {
                return Err(Error::with_state(
                    SqlState::SYNTAX_ERROR,
                    "zero-length delimited identifier",
                ));
            }
            return Ok((name, j + 1));
        }
        name.push('"');
        j += 2;
        start = j;
    }
    Err(Error::with_state(
        SqlState::SYNTAX_ERROR,
        "unterminated quoted identifier",
    ))
}

/// A syntax error, in Postgres' words, at the token the statement cannot go on from.
pub(crate) fn syntax_error_at(word: Option<&Word>) -> Error {
    let at = match word {
        None => "end of input".to_string(),
        Some(Word::Bare(w) | Word::Quoted(w)) => format!("\"{w}\""),
        Some(Word::Str(s)) => format!("\"'{s}'\""),
        Some(Word::Punct(c)) => format!("\"{c}\""),
    };
    let msg = format!("syntax error at or near {at}");
    let msg = if word.is_none() {
        msg.replace("or near ", "")
    } else {
        msg
    };
    Error::with_state(SqlState::SYNTAX_ERROR, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(s: &str) -> Word {
        Word::Bare(s.to_string())
    }

    #[test]
    fn names_stay_whole() {
        assert_eq!(
            ddl_words(
                r#"DROP /* x */ "a b" -- y
              Foo 'it''s' ; "say ""hi""""#
            )
            .unwrap(),
            vec![
                bare("drop"),
                Word::Quoted("a b".into()),
                bare("foo"),
                Word::Str("it's".into()),
                Word::Punct(';'),
                Word::Quoted(r#"say "hi""#.into()),
            ]
        );
    }

    #[test]
    fn bad_quotes_are_syntax_errors() {
        for sql in [r#"DROP """#, r#"DROP "abc"#] {
            assert_eq!(ddl_words(sql).unwrap_err().sql_state(), SqlState::SYNTAX_ERROR, "{sql}");
        }
    }
}
