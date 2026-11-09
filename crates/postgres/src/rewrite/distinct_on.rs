//! DISTINCT ON, which SQLite lacks.

use crate::rewrite::catalog::is_word_byte;
use crate::rewrite::functions::has_word_at;
use crate::rewrite::strings::end_of_string_literal;

/// Finds the first whole-word occurrence of word at paren depth zero and outside
/// string literals, at or after from.
pub(crate) fn index_top_level_word(sql: &str, from: usize, word: &str) -> Option<usize> {
    let low = sql.to_ascii_lowercase();
    let b = sql.as_bytes();
    let l = low.as_bytes();
    let mut depth = 0;
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                i = end_of_string_literal(sql, i);
                continue;
            }
            b'(' => {
                depth += 1;
                i += 1;
                continue;
            }
            b')' => {
                if depth > 0 {
                    depth -= 1;
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        if depth == 0 && (i == 0 || !is_word_byte(l[i - 1])) && has_word_at(&low, i, word) {
            return Some(i);
        }
        i += 1;
    }
    None
}
