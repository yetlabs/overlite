//! What pg_get_triggerdef() reads.

use overlite_core::sync::RwLockExt;
use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// Backs the process-global pg_get_triggerdef() scalar (which has no DB access)
/// with each trigger's CREATE statement. Refreshed from the main database's
/// sqlite_master on every connection setup. Keys are the pg_trigger oids (rowid +
/// 70000000 for the public schema).
static TRIGGER_DEF_BY_OID: LazyLock<RwLock<HashMap<i64, String>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Stores oid\x1fsql pairs (produced by the query in setup_connection) into the
/// registry.
pub(crate) fn refresh_trigger_defs(pairs: &[String]) {
    for p in pairs {
        let Some(i) = p.find('\x1f') else {
            continue;
        };
        let Ok(oid) = p[..i].parse::<i64>() else {
            continue;
        };
        TRIGGER_DEF_BY_OID.write_locked().insert(oid, p[i + 1..].to_string());
    }
}

pub(crate) fn lookup_trigger_def(oid: i64) -> String {
    TRIGGER_DEF_BY_OID.read_locked().get(&oid).cloned().unwrap_or_default()
}
