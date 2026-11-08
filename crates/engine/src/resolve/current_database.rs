//! current_database() names the database the connection is on, which a process
//! serving several of them cannot answer from a globally registered SQLite
//! function. It is resolved per statement instead, like current_schema().

use crate::db_state::DbState;
use crate::resolve::current_schema::replace_call_outside_strings;
use crate::resolve::schema_qualify::lowercase;
use overlite_core::text::quote_literal;

/// Replaces current_database() with this database's name.
pub(crate) fn resolve_current_database(st: &DbState, query: &str) -> String {
    if !lowercase(query).contains("current_database") {
        return query.to_string();
    }
    let name = quote_literal(st.db_name());
    replace_call_outside_strings(query, "current_database", |_| name.clone())
}
