//! The crate's shared reader of a statement as tokens: words, literals and quoted
//! names, punctuation, each with its parenthesis depth, comments left out.

use crate::rewrite::blank::{skip_block_comment, skip_line_comment};
use crate::rewrite::strings::end_of_string_literal;
use std::ops::Range;

#[derive(Debug, PartialEq)]
pub(crate) enum CodeTokKind {
    Word,
    /// A literal or a quoted name.
    Value,
    Punct,
}

#[derive(Debug)]
pub(crate) struct CodeTok {
    pub(crate) span: Range<usize>,
    pub(crate) kind: CodeTokKind,
    /// Parentheses around it; one of its own counts as outside.
    pub(crate) depth: i32,
}

impl CodeTok {
    pub(crate) fn is_word(&self, sql: &str, w: &str) -> bool {
        self.kind == CodeTokKind::Word && sql[self.span.clone()].eq_ignore_ascii_case(w)
    }

    pub(crate) fn is_punct(&self, sql: &str, p: &str) -> bool {
        self.kind == CodeTokKind::Punct && &sql[self.span.clone()] == p
    }
}

/// The tokens of sql, comments left out.
pub(crate) fn code_tokens(sql: &str) -> Vec<CodeTok> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0;
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let kind = match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = skip_line_comment(sql, i);
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(sql, i).0;
                continue;
            }
            b'\'' => {
                i = end_of_string_literal(sql, i);
                CodeTokKind::Value
            }
            // A doubled quote reads as two names side by side, which skips the same.
            b'"' => {
                i = sql[i + 1..].find('"').map_or(b.len(), |j| i + j + 2);
                CodeTokKind::Value
            }
            c if is_word_char(c) => {
                while i < b.len() && is_word_char(b[i]) {
                    i += 1;
                }
                CodeTokKind::Word
            }
            _ => {
                i += 1;
                CodeTokKind::Punct
            }
        };
        if b[start] == b')' {
            depth -= 1;
        }
        out.push(CodeTok {
            span: start..i,
            kind,
            depth,
        });
        if b[start] == b'(' {
            depth += 1;
        }
    }
    out
}

/// A byte of a word, with the bytes of a character outside ASCII among them.
pub(crate) fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}
