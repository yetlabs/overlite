//! A cell and a statement's result.

use crate::text::text_from_bytes;
use std::fmt;

/// A timestamp with its offset, as read from a date or time column.
pub type Timestamp = chrono::DateTime<chrono::FixedOffset>;

/// A single cell.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    #[default]
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
    Bool(bool),
    Time(Timestamp),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Text as is, a blob as lossy text, anything else printed.
    #[must_use]
    pub fn to_text_lossy(&self) -> String {
        match self {
            Value::Blob(b) => text_from_bytes(b).into_owned(),
            other => other.to_string(),
        }
    }
}

/// The cell printed as text: `<nil>` for NULL, shortest digits for a float, and a
/// blob as its byte values.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("<nil>"),
            Value::Int(i) => i.fmt(f),
            Value::Float(v) => f.write_str(&format_float(*v)),
            Value::Text(s) => f.write_str(s),
            Value::Blob(b) => {
                let parts: Vec<String> = b.iter().map(ToString::to_string).collect();
                write!(f, "[{}]", parts.join(" "))
            }
            Value::Bool(b) => b.fmt(f),
            Value::Time(t) => write!(f, "{}", t.format("%Y-%m-%d %H:%M:%S%.f %z")),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Blob(v)
    }
}

/// A float the way results print it:
/// shortest round-trip digits, exponent form when exp < -4 || exp >= 6.
pub fn format_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        // As float8out writes it.
        return if f > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }
    let sci = format!("{f:e}"); // shortest digits, e.g. 1.5e3
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{}e{}{:02}", mant, sign, exp.abs());
    }
    format!("{f}")
}

/// One output column.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Column {
    pub name: String,
    /// The declared type, upper-cased, or a type name taken from the first row
    /// ("int8", "float8", "text", "bytea") for computed columns; may be
    /// empty.
    pub decl_type: String,
}

/// The outcome of executing one statement.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultSet {
    pub is_query: bool,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub rows_affected: i64,
    pub last_insert_id: i64,
    pub command: Command,
}

impl ResultSet {
    /// The CommandComplete tag.
    #[must_use]
    pub fn command_tag(&self) -> String {
        match &self.command {
            Command::Select => format!("SELECT {}", self.rows_affected),
            Command::Insert => format!("INSERT 0 {}", self.rows_affected),
            Command::Update => format!("UPDATE {}", self.rows_affected),
            Command::Delete => format!("DELETE {}", self.rows_affected),
            Command::Unnamed => "OK".to_string(),
            Command::Other(name) => name.clone(),
        }
    }
}

/// What a statement was. The four that report a row count have their own variant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Command {
    /// A statement with no words to name it.
    #[default]
    Unnamed,
    Select,
    Insert,
    Update,
    Delete,
    /// Any other, by its name.
    Other(String),
}

impl fmt::Display for Command {
    /// The tag of a statement with no row count to report.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Command::Unnamed => "OK",
            Command::Select => "SELECT",
            Command::Insert => "INSERT",
            Command::Update => "UPDATE",
            Command::Delete => "DELETE",
            Command::Other(name) => name,
        })
    }
}

impl From<&str> for Command {
    fn from(name: &str) -> Command {
        match name {
            "" => Command::Unnamed,
            "SELECT" => Command::Select,
            "INSERT" => Command::Insert,
            "UPDATE" => Command::Update,
            "DELETE" => Command::Delete,
            other => Command::Other(other.to_string()),
        }
    }
}

impl From<String> for Command {
    fn from(name: String) -> Command {
        Command::from(name.as_str())
    }
}
