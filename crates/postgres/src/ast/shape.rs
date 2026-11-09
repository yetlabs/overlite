//! A statement's shape: a hash of its text with the values of its literals left out,
//! so that `WHERE id = 7` and `WHERE id = 8` are one shape. String literals count by
//! their place only, numbers too; names, quoted or not, keywords and operators count
//! whole. It reads the text `statement_text` leaves (comments blanked, one kind of
//! string literal).
//!
//! `OUTSIDE` remembers shapes the AST path left to the text passes, so that another
//! statement of the shape goes to them without a parse. Being wrong costs nothing but
//! that parse or, at worst, a statement the AST path could have taken going the text
//! passes' way, which is the way every statement went before it: a mistake can make
//! a translation slower, never different from the text passes' own.

use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// The shape of sql, never 0.
pub(crate) fn of(sql: &str) -> u64 {
    let b = sql.as_bytes();
    // One key for the process, so that every thread agrees on a shape; a collision
    // only sends a statement to the text passes.
    let mut h = FIXED.build_hasher();
    let mut i = 0;
    let mut start = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' {
            h.write(&b[start..i]);
            h.write_u8(0xFF);
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            start = i;
        } else if c == b'"' {
            // A quoted name is part of the shape, whatever it holds.
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            i += 1;
        } else if c.is_ascii_digit() && (i == 0 || !is_word(b[i - 1])) {
            h.write(&b[start..i]);
            h.write_u8(0xFE);
            while i < b.len() && (is_word(b[i]) || b[i] == b'.') {
                i += 1;
            }
            start = i;
        } else if is_word(c) {
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    h.write(&b[start.min(b.len())..]);
    h.finish() | 1
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

static FIXED: std::sync::LazyLock<std::collections::hash_map::RandomState> =
    std::sync::LazyLock::new(std::collections::hash_map::RandomState::new);

/// A fixed set of shapes, each in the slot its hash picks; a newer shape takes the
/// slot of an older one. Lock-free: a slot is one atomic word.
#[derive(Debug)]
pub(crate) struct Shapes([AtomicU64; SLOTS]);

const SLOTS: usize = 4096;

impl Shapes {
    // Built once, in a static; the tests' own copies are small enough.
    #[allow(clippy::large_stack_arrays)]
    pub(crate) const fn new() -> Shapes {
        Shapes([const { AtomicU64::new(0) }; SLOTS])
    }

    pub(crate) fn holds(&self, shape: u64) -> bool {
        self.0[(shape as usize) % SLOTS].load(Ordering::Relaxed) == shape
    }

    pub(crate) fn insert(&self, shape: u64) {
        self.0[(shape as usize) % SLOTS].store(shape, Ordering::Relaxed);
    }
}

/// Shapes the AST path does not cover.
pub(crate) static OUTSIDE: Shapes = Shapes::new();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_not_the_shape() {
        let same = [
            ("SELECT a FROM t WHERE id = 7", "SELECT a FROM t WHERE id = 81234"),
            ("SELECT a FROM t WHERE n = 'x'", "SELECT a FROM t WHERE n = 'it''s'"),
            ("SELECT a FROM t WHERE n = 1.5", "SELECT a FROM t WHERE n = 22.25"),
            ("SELECT 'a', 'b' FROM t", "SELECT '', 'zz' FROM t"),
        ];
        for (a, b) in same {
            assert_eq!(of(a), of(b), "{a} / {b}");
        }
        let other = [
            ("SELECT a FROM t WHERE id = 7", "SELECT b FROM t WHERE id = 7"),
            ("SELECT a FROM t WHERE id = 7", "SELECT a FROM t WHERE id = '7'"),
            ("SELECT a1 FROM t", "SELECT a2 FROM t"),
            ("SELECT \"7\" FROM t", "SELECT \"8\" FROM t"),
            ("SELECT $1 FROM t", "SELECT $2 FROM t"),
            (
                "SELECT a FROM t WHERE id IN (1, 2)",
                "SELECT a FROM t WHERE id IN (1, 2, 3)",
            ),
        ];
        for (a, b) in other {
            assert_ne!(of(a), of(b), "{a} / {b}");
        }
    }

    #[test]
    fn a_shape_is_remembered_until_another_takes_its_slot() {
        let s = Shapes::new();
        let a = of("SELECT a FROM t WHERE id = 1");
        assert!(!s.holds(a));
        s.insert(a);
        assert!(s.holds(a));
        assert!(s.holds(of("SELECT a FROM t WHERE id = 2")));
        let b = a.wrapping_add(SLOTS as u64);
        s.insert(b);
        assert!(!s.holds(a));
    }
}
