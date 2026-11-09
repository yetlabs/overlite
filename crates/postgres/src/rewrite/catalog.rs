//! Catalog references: pg_catalog, information_schema, object comments and definitions.

use overlite_core::lazy_regex;

pub(crate) fn is_word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_lowercase() || b.is_ascii_uppercase() || b.is_ascii_digit()
}

// Matches a "public." (or "public".) schema qualifier on a single name.
// Stripping it across a whole statement is the engine's job, after search_path
// resolution; this is for the places that handle one table name or clause.
lazy_regex!(pub(crate) RE_PUBLIC, r#"(?i)"?\bpublic\b"?\."#);
