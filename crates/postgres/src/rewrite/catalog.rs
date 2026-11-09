//! Catalog references: pg_catalog, information_schema, object comments and definitions.

use overlite_core::lazy_regex;

// Matches references to the emulated information_schema so they
// resolve to the quoted, dotted view names the engine materializes. Either name may
// be quoted, as ORMs write them (`"information_schema"."sequences"`); a quoted name
// keeps its case, as in Postgres.
lazy_regex!(
    RE_INFO_SCHEMA,
    concat!(
        r#"(?:(?i:\binformation_schema)|"information_schema")\."#,
        r#"(?:(?i:(?:tables|columns|table_constraints|key_column_usage|referential_constraints|"#,
        r#"constraint_column_usage|check_constraints|views|schemata|sequences|routines|column_privileges)\b)"#,
        r#"|"(?:tables|columns|table_constraints|key_column_usage|referential_constraints|"#,
        r#"constraint_column_usage|check_constraints|views|schemata|sequences|routines|column_privileges)")"#
    )
);

pub(crate) fn is_word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_lowercase() || b.is_ascii_uppercase() || b.is_ascii_digit()
}

// Strips the "pg_catalog." schema qualifier: our emulated catalog
// tables and functions live under bare names, and clients reference them both
// qualified and unqualified.
lazy_regex!(RE_PG_CATALOG, r#"(?:(?i:\bpg_catalog)|"pg_catalog")\."#);

// Matches a "public." (or "public".) schema qualifier on a single name.
// Stripping it across a whole statement is the engine's job, after search_path
// resolution; this is for the places that handle one table name or clause.
lazy_regex!(pub(crate) RE_PUBLIC, r#"(?i)"?\bpublic\b"?\."#);
