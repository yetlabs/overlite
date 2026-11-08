//! The patterns of LIKE and ILIKE, written for SQLite's GLOB and LIKE: see
//! rewrite/like.rs in the postgres crate. Postgres reads `%` and `_` as wildcards, and
//! the escape character (a backslash unless ESCAPE says otherwise, none for '') before
//! any character as that character.

use crate::error::{Error, Result};
use overlite_core::{SqlState, Value};

/// One piece of a pattern.
enum Piece {
    Any,
    One,
    Char(char),
}

fn pieces(pattern: &str, escape: &Value) -> Result<Vec<Piece>> {
    let esc = match escape {
        Value::Null => None,
        v => {
            let e = v.to_string();
            let mut chars = e.chars();
            match (chars.next(), chars.next()) {
                (None, _) => None,
                (Some(c), None) => Some(c),
                _ => {
                    return Err(Error::with_state(
                        SqlState::INVALID_ESCAPE_CHARACTER,
                        "invalid escape string".to_string(),
                    ));
                }
            }
        }
    };
    let mut out = Vec::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == esc {
            match chars.next() {
                Some(n) => out.push(Piece::Char(n)),
                None => {
                    return Err(Error::with_state(
                        SqlState::INVALID_ESCAPE_SEQUENCE,
                        "LIKE pattern must not end with escape character".to_string(),
                    ));
                }
            }
            continue;
        }
        out.push(match c {
            '%' => Piece::Any,
            '_' => Piece::One,
            c => Piece::Char(c),
        });
    }
    Ok(out)
}

/// The GLOB pattern that matches what a LIKE pattern does: `*` and `?` for the
/// wildcards, and each of GLOB's own `*?[` as a class of itself.
///
/// # Errors
///
/// 22025 for a pattern ending in its escape character, 22019 for an escape that is
/// more than one character.
pub(crate) fn like_glob(pattern: &Value, escape: &Value) -> Result<Value> {
    if pattern.is_null() {
        return Ok(Value::Null);
    }
    let mut out = String::new();
    for p in pieces(&pattern.to_string(), escape)? {
        match p {
            Piece::Any => out.push('*'),
            Piece::One => out.push('?'),
            Piece::Char(c @ ('*' | '?' | '[')) => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            Piece::Char(c) => out.push(c),
        }
    }
    Ok(Value::Text(out))
}

/// The SQLite LIKE pattern, with a backslash for its escape, that matches what an ILIKE
/// pattern does.
///
/// # Errors
///
/// As like_glob.
pub(crate) fn ilike_pattern(pattern: &Value, escape: &Value) -> Result<Value> {
    if pattern.is_null() {
        return Ok(Value::Null);
    }
    let mut out = String::new();
    for p in pieces(&pattern.to_string(), escape)? {
        match p {
            Piece::Any => out.push('%'),
            Piece::One => out.push('_'),
            Piece::Char(c @ ('%' | '_' | '\\')) => {
                out.push('\\');
                out.push(c);
            }
            Piece::Char(c) => out.push(c),
        }
    }
    Ok(Value::Text(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Value {
        Value::Text(s.to_string())
    }

    #[test]
    fn patterns() {
        let bs = t("\\");
        assert_eq!(like_glob(&t("a\\_b%"), &bs).unwrap(), t("a_b*"));
        assert_eq!(like_glob(&t("a*?[_"), &bs).unwrap(), t("a[*][?][[]?"));
        assert_eq!(like_glob(&t("a\\b"), &t("")).unwrap(), t("a\\b"));
        assert_eq!(like_glob(&t("a#%b"), &t("#")).unwrap(), t("a%b"));
        assert_eq!(ilike_pattern(&t("a#%b\\"), &t("#")).unwrap(), t("a\\%b\\\\"));
        assert_eq!(like_glob(&Value::Null, &bs).unwrap(), Value::Null);
        assert_eq!(
            like_glob(&t("a\\"), &bs).unwrap_err().sql_state(),
            SqlState::INVALID_ESCAPE_SEQUENCE
        );
        assert!(like_glob(&t("a"), &t("ab")).is_err());
    }
}
