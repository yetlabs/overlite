//! What every overlite crate shares: the data model between the wire protocol and the
//! storage engine ([`Value`], [`Column`], [`ResultSet`]), the names Postgres gives to
//! types and errors ([`Oid`], [`SqlState`]), the handling of [`text`] and the policy on
//! poisoned locks ([`sync`]).
//!
//! ```
//! use overlite_core::{SqlState, Value};
//!
//! assert_eq!(Value::from(1.5).to_string(), "1.5");
//! assert_eq!(Value::Null.to_string(), "<nil>");
//! assert_eq!(SqlState::SYNTAX_ERROR.as_str(), "42601");
//! ```

mod oid;
mod sqlstate;
pub mod sync;
pub mod text;
mod value;

pub use oid::Oid;
pub use sqlstate::SqlState;
pub use value::{Column, Command, ResultSet, Timestamp, Value, format_float};

/// Reads a boolean the way Postgres' boolin does.
pub fn parse_bool(s: &str) -> Option<bool> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    match s.as_str() {
        "1" | "on" => return Some(true),
        "0" | "of" | "off" => return Some(false),
        _ => {}
    }
    for (word, value) in [("true", true), ("yes", true), ("false", false), ("no", false)] {
        if word.starts_with(s.as_str()) {
            return Some(value);
        }
    }
    None
}
