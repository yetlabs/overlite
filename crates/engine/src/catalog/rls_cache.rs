//! The per-database answer to "does this table have row-level security on".

use crate::error::Result;
use crate::sqlite::{DbSession, do_query};
use overlite_core::Value;
use overlite_core::sync::RwLockExt;
use std::collections::HashSet;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// Bounds how long a change the server did not make itself can go unnoticed.
const RLS_CACHE_TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
struct RlsInner {
    generation: u64,
    tables: Option<HashSet<String>>,
    loaded_at: Option<Instant>,
}

/// Remembers which tables of a database have row-level security on, so a
/// statement on a table without it does not pay for a query to find that out.
#[derive(Debug, Default)]
pub(crate) struct RlsCache {
    inner: RwLock<RlsInner>,
}

impl RlsCache {
    /// Whether table has RLS on (`enabled`), and whether the cache could tell.
    fn lookup(&self, table: &str, now: Instant) -> Option<bool> {
        let g = self.inner.read_locked();
        let tables = g.tables.as_ref()?;
        if now.duration_since(g.loaded_at?) >= RLS_CACHE_TTL {
            return None;
        }
        // The folded name is computed on every lookup.
        Some(tables.contains(ascii_lower(table).as_ref()))
    }

    fn generation(&self) -> u64 {
        self.inner.read_locked().generation
    }

    /// Keeps tables, read after generation returned gen. A load an invalidation
    /// overtook may hold the state from before the change, so it is dropped.
    fn store(&self, generation: u64, tables: HashSet<String>, now: Instant) {
        let mut g = self.inner.write_locked();
        if generation != g.generation {
            return;
        }
        g.tables = Some(tables);
        g.loaded_at = Some(now);
    }

    fn invalidate(&self) {
        let mut g = self.inner.write_locked();
        g.generation += 1;
        g.tables = None;
    }
}

/// Folds the way SQLite's lower() does: ASCII only.
pub(crate) fn ascii_lower(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes().any(|c| c.is_ascii_uppercase()) {
        std::borrow::Cow::Owned(s.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

impl DbSession {
    /// Whether table has row-level security on, as committed.
    ///
    /// # Errors
    ///
    /// When the catalog query fails.
    pub fn rls_enabled(&self, table: &str) -> Result<bool> {
        let cache = &self.state.rls;
        if let Some(enabled) = cache.lookup(table, Instant::now()) {
            return Ok(enabled);
        }

        let generation = cache.generation();
        let rs = do_query(
            &self.conn,
            "SELECT tablename FROM _overlite_rls WHERE enabled <> 0",
            &[],
            "",
        )?;
        let mut tables = HashSet::new();
        for row in &rs.rows {
            if let Some(Value::Text(name)) = row.first() {
                tables.insert(ascii_lower(name).into_owned());
            }
        }
        let enabled = tables.contains(ascii_lower(table).as_ref());
        cache.store(generation, tables, Instant::now());
        Ok(enabled)
    }

    /// Makes the next rls_enabled read the database again.
    pub fn invalidate_rls(&self) {
        self.state.rls.invalidate();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| ascii_lower(n).into_owned()).collect()
    }

    #[test]
    fn rls_cache_empty_is_not_known() {
        let c = RlsCache::default();
        assert_eq!(c.lookup("docs", Instant::now()), None, "nothing loaded yet");
    }

    #[test]
    fn rls_cache_answers_from_store() {
        let c = RlsCache::default();
        let now = Instant::now();
        c.store(c.generation(), set(&["docs"]), now);

        assert_eq!(c.lookup("DOCS", now), Some(true));
        assert_eq!(c.lookup("other", now), Some(false));
    }

    #[test]
    fn rls_cache_expires() {
        let c = RlsCache::default();
        let now = Instant::now();
        c.store(c.generation(), set(&[]), now);

        assert!(
            c.lookup(
                "docs",
                (now + RLS_CACHE_TTL).checked_sub(Duration::from_millis(1)).unwrap()
            )
            .is_some()
        );
        assert_eq!(
            c.lookup("docs", now + RLS_CACHE_TTL),
            None,
            "an external change must be picked up"
        );
    }

    #[test]
    fn rls_cache_invalidate() {
        let c = RlsCache::default();
        let now = Instant::now();
        c.store(c.generation(), set(&[]), now);
        c.invalidate();
        assert_eq!(c.lookup("docs", now), None);
    }

    // A load that started before a commit may hold the state from before it. If
    // it landed after the invalidation, the table would be served unfiltered.
    #[test]
    fn rls_cache_drops_load_overtaken_by_invalidation() {
        let c = RlsCache::default();
        let now = Instant::now();
        let generation = c.generation();
        c.invalidate();
        c.store(generation, set(&[]), now);

        assert_eq!(c.lookup("docs", now), None);
    }

    #[test]
    fn ascii_lower_matches_sqlite() {
        for (input, want) in [
            ("", ""),
            ("docs", "docs"),
            ("DoCs", "docs"),
            ("AÇÃO", "aÇÃo"), // lower() leaves everything past ASCII alone
            ("ÇAB", "Çab"),
        ] {
            assert_eq!(ascii_lower(input), want, "{input}");
        }
    }
}
