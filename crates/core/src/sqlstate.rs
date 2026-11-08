//! SQLSTATE codes: the five characters a Postgres client keys its error handling on.

use std::fmt;

/// A SQLSTATE error code.
///
/// The constants are named after the condition names of Postgres' Appendix A.
///
/// ```
/// use overlite_core::SqlState;
///
/// assert_eq!(SqlState::UNDEFINED_TABLE.as_str(), "42P01");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SqlState(&'static str);

impl SqlState {
    pub const SUCCESS: SqlState = SqlState("00000");
    pub const PROTOCOL_VIOLATION: SqlState = SqlState("08P01");
    pub const CONNECTION_FAILURE: SqlState = SqlState("08006");
    pub const FEATURE_NOT_SUPPORTED: SqlState = SqlState("0A000");
    pub const EXTERNAL_ROUTINE_INVOCATION_EXCEPTION: SqlState = SqlState("39000");
    pub const STRING_DATA_RIGHT_TRUNCATION: SqlState = SqlState("22001");
    pub const DATA_EXCEPTION: SqlState = SqlState("22000");
    pub const NUMERIC_VALUE_OUT_OF_RANGE: SqlState = SqlState("22003");
    pub const NULL_VALUE_NOT_ALLOWED: SqlState = SqlState("22004");
    pub const INVALID_DATETIME_FORMAT: SqlState = SqlState("22007");
    pub const DATETIME_FIELD_OVERFLOW: SqlState = SqlState("22008");
    pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: SqlState = SqlState("22009");
    pub const DIVISION_BY_ZERO: SqlState = SqlState("22012");
    pub const INVALID_REGULAR_EXPRESSION: SqlState = SqlState("2201B");
    pub const INVALID_ARGUMENT_FOR_WIDTH_BUCKET_FUNCTION: SqlState = SqlState("2201G");
    pub const ARRAY_SUBSCRIPT_ERROR: SqlState = SqlState("2202E");
    pub const SEQUENCE_GENERATOR_LIMIT_EXCEEDED: SqlState = SqlState("2200H");
    pub const CHARACTER_NOT_IN_REPERTOIRE: SqlState = SqlState("22021");
    pub const INVALID_PARAMETER_VALUE: SqlState = SqlState("22023");
    pub const INVALID_ESCAPE_CHARACTER: SqlState = SqlState("22019");
    pub const INVALID_ESCAPE_SEQUENCE: SqlState = SqlState("22025");
    pub const INVALID_TEXT_REPRESENTATION: SqlState = SqlState("22P02");
    pub const INVALID_BINARY_REPRESENTATION: SqlState = SqlState("22P03");
    pub const INTEGRITY_CONSTRAINT_VIOLATION: SqlState = SqlState("23000");
    pub const BAD_COPY_FILE_FORMAT: SqlState = SqlState("22P04");
    pub const NOT_NULL_VIOLATION: SqlState = SqlState("23502");
    pub const FOREIGN_KEY_VIOLATION: SqlState = SqlState("23503");
    pub const UNIQUE_VIOLATION: SqlState = SqlState("23505");
    pub const CHECK_VIOLATION: SqlState = SqlState("23514");
    pub const EXCLUSION_VIOLATION: SqlState = SqlState("23P01");
    pub const INVALID_TRANSACTION_STATE: SqlState = SqlState("25000");
    pub const ACTIVE_SQL_TRANSACTION: SqlState = SqlState("25001");
    pub const READ_ONLY_SQL_TRANSACTION: SqlState = SqlState("25006");
    pub const NO_ACTIVE_SQL_TRANSACTION: SqlState = SqlState("25P01");
    pub const IN_FAILED_SQL_TRANSACTION: SqlState = SqlState("25P02");
    pub const INVALID_SQL_STATEMENT_NAME: SqlState = SqlState("26000");
    pub const INVALID_AUTHORIZATION_SPECIFICATION: SqlState = SqlState("28000");
    pub const INVALID_PASSWORD: SqlState = SqlState("28P01");
    pub const DEPENDENT_OBJECTS_STILL_EXIST: SqlState = SqlState("2BP01");
    pub const INVALID_SAVEPOINT_SPECIFICATION: SqlState = SqlState("3B001");
    pub const INVALID_CURSOR_NAME: SqlState = SqlState("34000");
    pub const INVALID_CATALOG_NAME: SqlState = SqlState("3D000");
    pub const INVALID_SCHEMA_NAME: SqlState = SqlState("3F000");
    pub const INSUFFICIENT_PRIVILEGE: SqlState = SqlState("42501");
    pub const SYNTAX_ERROR: SqlState = SqlState("42601");
    pub const INVALID_NAME: SqlState = SqlState("42602");
    pub const CANNOT_COERCE: SqlState = SqlState("42846");
    pub const NAME_TOO_LONG: SqlState = SqlState("42622");
    pub const UNDEFINED_COLUMN: SqlState = SqlState("42703");
    pub const GENERATED_ALWAYS: SqlState = SqlState("428C9");
    pub const UNDEFINED_OBJECT: SqlState = SqlState("42704");
    pub const DUPLICATE_COLUMN: SqlState = SqlState("42701");
    pub const DUPLICATE_CURSOR: SqlState = SqlState("42P03");
    pub const AMBIGUOUS_COLUMN: SqlState = SqlState("42702");
    pub const WRONG_OBJECT_TYPE: SqlState = SqlState("42809");
    pub const RESERVED_NAME: SqlState = SqlState("42939");
    pub const DUPLICATE_OBJECT: SqlState = SqlState("42710");
    pub const INVALID_OBJECT_DEFINITION: SqlState = SqlState("42P17");
    pub const DATATYPE_MISMATCH: SqlState = SqlState("42804");
    pub const GROUPING_ERROR: SqlState = SqlState("42803");
    pub const INVALID_FOREIGN_KEY: SqlState = SqlState("42830");
    pub const UNDEFINED_FUNCTION: SqlState = SqlState("42883");
    pub const UNDEFINED_TABLE: SqlState = SqlState("42P01");
    pub const UNDEFINED_PARAMETER: SqlState = SqlState("42P02");
    pub const DUPLICATE_DATABASE: SqlState = SqlState("42P04");
    pub const DUPLICATE_SCHEMA: SqlState = SqlState("42P06");
    pub const DUPLICATE_TABLE: SqlState = SqlState("42P07");
    pub const INVALID_COLUMN_REFERENCE: SqlState = SqlState("42P10");
    pub const INVALID_RECURSION: SqlState = SqlState("42P19");
    pub const PROGRAM_LIMIT_EXCEEDED: SqlState = SqlState("54000");
    pub const TOO_MANY_COLUMNS: SqlState = SqlState("54011");
    pub const WINDOWING_ERROR: SqlState = SqlState("42P20");
    pub const DISK_FULL: SqlState = SqlState("53100");
    pub const OUT_OF_MEMORY: SqlState = SqlState("53200");
    pub const TOO_MANY_CONNECTIONS: SqlState = SqlState("53300");
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: SqlState = SqlState("55000");
    pub const OBJECT_IN_USE: SqlState = SqlState("55006");
    pub const LOCK_NOT_AVAILABLE: SqlState = SqlState("55P03");
    pub const QUERY_CANCELED: SqlState = SqlState("57014");
    pub const IO_ERROR: SqlState = SqlState("58030");
    pub const INTERNAL_ERROR: SqlState = SqlState("XX000");
    pub const DATA_CORRUPTED: SqlState = SqlState("XX001");

    /// The five characters that go on the wire.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Debug for SqlState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SqlState({})", self.0)
    }
}

impl AsRef<str> for SqlState {
    fn as_ref(&self) -> &str {
        self.0
    }
}
