//! Enum types.
//!
//! Enum types are stored in two internal tables in the main database:
//! _overlite_enum_types (one row per type; its rowid seeds the pg_type oid) and
//! _overlite_enums (one row per label, with a sort order). SQLite has no enum
//! types, so the protocol layer records CREATE/ALTER/DROP TYPE ... AS ENUM here
//! and rewrites an enum column to TEXT plus a CHECK constraint: native SQLite, so
//! the file stays usable without overlite. Both tables are hidden from the
//! catalog by the `_overlite_*` GLOB filter (views.rs).

use crate::catalog::lower_chars;
use crate::db_state::DbState;
use overlite_core::sync::RwLockExt;
use overlite_core::text::slice;
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// Lets the process-global format_type() scalar render an enum type's oid as its
/// name (scalars have no DB access). It is refreshed from _overlite_enum_types on
/// every connection setup, so a type created in a prior session (or by another
/// connection) is picked up on connect. Keys are oids.
static ENUM_NAME_BY_OID: LazyLock<RwLock<HashMap<i64, String>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Stores oid:name pairs (as produced by the enum-name query in setup_connection)
/// into the global registry.
pub(crate) fn refresh_enum_names(pairs: &[String]) {
    for p in pairs {
        let Some(i) = p.find(':') else {
            continue;
        };
        let Ok(oid) = p[..i].parse::<i64>() else {
            continue;
        };
        ENUM_NAME_BY_OID.write_locked().insert(oid, p[i + 1..].to_string());
    }
}

/// Whether some enum type of this process goes by name.
pub(crate) fn enum_type_exists(name: &str) -> bool {
    ENUM_NAME_BY_OID
        .read_locked()
        .values()
        .any(|n| n.eq_ignore_ascii_case(name))
}

pub(crate) fn lookup_enum_name(oid: i64) -> Option<String> {
    ENUM_NAME_BY_OID.read_locked().get(&oid).cloned()
}

/// Offsets a type's rowid into an oid range that won't collide with the built-in
/// pg_type oids or the sequence/pg_class ranges.
pub(crate) const ENUM_OID_BASE: i64 = 90000000;

/// How many oids each database gets for its enum types. The registry
/// format_type() reads is process-wide, so two databases numbering their types
/// from the same rowid would otherwise render each other's names.
pub(crate) const ENUM_OID_BAND_WIDTH: i64 = 1000000;

pub(crate) const ENUM_TYPES_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_enum_types (
  typname TEXT PRIMARY KEY COLLATE NOCASE
)"#;

pub(crate) const ENUM_LABELS_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_enums (
  typname   TEXT COLLATE NOCASE,
  label     TEXT,
  sortorder REAL
)"#;

/// Composite types (CREATE TYPE ... AS (...)) are recorded by name so they show
/// in pg_type (typtype 'c') and \dT; their fields aren't modeled (SQLite has no
/// composite storage).
pub(crate) const COMPOSITE_TYPES_TABLE_DDL: &str = r#"CREATE TABLE IF NOT EXISTS _overlite_composite_types (
  typname TEXT PRIMARY KEY COLLATE NOCASE
)"#;

/// Pulls the label list out of the CHECK an enum column carries: `m text CHECK (m
/// IN ('sad', 'ok', 'happy'))` yields "sad,ok,happy", in the enum's own order.
/// The declared SQLite type is only ever "text", so the CHECK is what still says
/// which enum the column was declared as; matching that list against
/// _overlite_enums recovers the type. Returns "" when the column has no such
/// CHECK.
///
/// The list is joined with "," to be compared against group_concat's default,
/// which means a label containing a comma simply fails to match and the column
/// reports as text: the same answer as before, never a wrong type.
pub(crate) fn enum_labels_from_ddl(ddl: &str, col: &str) -> String {
    let (low, low_col) = (lower_chars(ddl), lower_chars(col));
    for want in [format!("check ({low_col} in ("), format!("check (\"{low_col}\" in (")] {
        let Some(i) = low.find(&want) else {
            continue;
        };
        // The offset comes from the lowered text, and applies to ddl.
        let rest = slice(ddl, i + want.len(), ddl.len());
        let Some(end) = rest.find(')') else {
            continue;
        };
        return sql_string_literals(&rest[..end]).join(",");
    }
    String::new()
}

/// The single-quoted values in s, in order, with '' unescaped back to a lone
/// quote.
fn sql_string_literals(s: &str) -> Vec<String> {
    let p = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < p.len() {
        if p[i] != b'\'' {
            i += 1;
            continue;
        }
        let mut b: Vec<u8> = Vec::new();
        i += 1;
        while i < p.len() {
            if p[i] == b'\'' {
                if i + 1 < p.len() && p[i + 1] == b'\'' {
                    b.push(b'\'');
                    i += 2;
                    continue;
                }
                break;
            }
            b.push(p[i]);
            i += 1;
        }
        out.push(String::from_utf8_lossy(&b).into_owned());
        i += 1;
    }
    out
}

/// The table whose writes invalidate the oid->name registry.
pub(crate) const ENUM_TYPES_TABLE: &str = "_overlite_enum_types";

/// Reloads the registry format_type() reads, from the connection that just
/// changed it.
pub(crate) fn refresh_enum_names_from(st: &DbState, q: &Connection) {
    let sql = format!(
        "SELECT (rowid + {}) || ':' || typname FROM {}",
        st.enum_base(),
        ENUM_TYPES_TABLE
    );
    let Ok(mut stmt) = q.prepare(&sql) else {
        return;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return;
    };
    let mut pairs = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        if let Ok(v) = row.get::<_, String>(0) {
            pairs.push(v);
        }
    }
    refresh_enum_names(&pairs);
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::sqlite::tests::{TempDir, text_or_empty};
    use crate::sqlite::{DbSession, Sqlite};
    use overlite_core::Value;
    use overlite_core::text::quote_literal;

    // An enum type's oid is its rowid plus a base, and format_type() renders a name
    // from a registry shared by the whole process. Two databases numbering from the
    // same base therefore produced the same oid for different types, and each
    // rendered whichever name was stored last: the other database's.
    #[test]
    fn enums_do_not_collide_across_databases() {
        let dir = TempDir::new();

        // A client session, which is the path that keeps the registry current. The
        // engine stays open with it, until the end of the test.
        let open = |name: &str| {
            let eng = Sqlite::open(&dir.join(&format!("{name}.db"))).unwrap();
            let sess = eng.session().unwrap();
            (eng, sess)
        };

        let (shop_eng, shop) = open("shop");
        let (blog_eng, blog) = open("blog");

        let exec = |s: &DbSession, sql: &str| {
            s.execute(sql, &[]).unwrap_or_else(|e| panic!("exec {sql}: {e}"));
        };
        // The first enum in each database, so both are rowid 1.
        exec(
            &shop,
            "INSERT INTO _overlite_enum_types (typname) VALUES ('order_status')",
        );
        exec(
            &blog,
            "INSERT INTO _overlite_enum_types (typname) VALUES ('post_state')",
        );

        // Their oids differ, which is what keeps the shared registry honest.
        let oid = |s: &DbSession, typname: &str| -> i64 {
            let rs = s
                .execute(
                    &format!("SELECT oid FROM pg_type WHERE typname = {}", quote_literal(typname)),
                    &[],
                )
                .unwrap();
            assert_eq!(rs.rows.len(), 1);
            match rs.rows[0][0] {
                Value::Int(i) => i,
                ref v => panic!("oid is {v:?}"),
            }
        };
        let (shop_oid, blog_oid) = (oid(&shop, "order_status"), oid(&blog, "post_state"));
        assert_ne!(
            shop_oid, blog_oid,
            "two databases gave the same oid to different enum types"
        );

        // And each renders its own name, not the other's.
        let name = |s: &DbSession, o: i64| {
            let rs = s.execute(&format!("SELECT format_type({o}, NULL)"), &[]).unwrap();
            text_or_empty(&rs.rows[0][0])
        };
        assert_eq!(name(&shop, shop_oid), "order_status");
        assert_eq!(name(&blog, blog_oid), "post_state");

        // A connection that arrives later loads the registry on connect and agrees.
        let (_later_eng, later) = open("shop");
        assert_eq!(name(&later, shop_oid), "order_status");

        drop((shop_eng, blog_eng));
    }
}
