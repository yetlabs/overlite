//! The translation cache: what `rewrite` made of a statement's text, kept so that a
//! statement sent again is not parsed and printed again.
//!
//! One cache for the process, in shards each behind its own lock, bounded twice: by
//! the number of statements and by the bytes they hold, the least recently used going
//! first. Clients of one application send the same statements, so a connection opened
//! later finds them translated, and memory is bounded once for the server rather than
//! per connection. A statement is kept the second time it is seen: a client that
//! writes its values into the text (the simple protocol, or a driver interpolating
//! parameters) sends most texts once, and those would otherwise push out the ones that
//! repeat, and pay for being kept.
//!
//! What a translation depends on, and how each is held:
//!
//! - the statement's text, the key. What the session expands before `rewrite` (enum
//!   columns, sequence calls, session identity, RLS filters) is in that text already;
//!   an expanded statement's labels are those of the statement as sent, so
//!   `rewrite_expanded` keys it by both texts.
//! - the collations CREATE COLLATION made (`rewrite_collate` asks the engine whether a
//!   name is one): each entry keeps the registry's generation it was made under, and
//!   one made under another is not answered.
//! - the clock, for a table's DDL only: a constant DEFAULT is folded when the table is
//!   made, as Postgres folds `DEFAULT 'today'::date`. Only statements that read or
//!   write rows are kept (`cacheable`), so DDL is translated every time.
//!
//! - the catalog's columns and the search_path, for a statement the AST path types
//!   (ast/env.rs): the session's context gives a stamp, which goes into the entry's
//!   hash and generation, so a translation is answered only under the snapshot of
//!   the tables and the search_path it was made under.
//!
//! What it does not depend on, because it is applied after `rewrite`, on every
//! execution: schema qualification and search_path's table names, the session's
//! TimeZone (the typed translation reads it when it runs, through overlite_tz_in and
//! overlite_tz_wall), and for the text passes' translations the catalog's types (the
//! engine's `resolve` passes).
//! tests/wire/translation_cache.rs runs one text across each change.

use overlite_core::sync::MutexExt;
use std::collections::HashMap;
use std::hash::{BuildHasher, BuildHasherDefault, Hasher};
use std::sync::Mutex;

/// How many statements the cache keeps.
pub(crate) const MAX_ENTRIES: usize = 16 * 1024;
/// How many bytes of statement and translation the cache keeps, each entry's
/// bookkeeping counted.
pub(crate) const MAX_BYTES: usize = 64 * 1024 * 1024;
/// A statement longer than this is translated every time: a bulk INSERT is seldom
/// sent twice, and would push many statements out.
pub(crate) const MAX_STATEMENT_LEN: usize = 16 * 1024;
/// What an entry costs beyond its text: the map's slot, the slab's, two boxes.
const ENTRY_OVERHEAD: usize = 96;
/// Shards. Each lock is held for a lookup and a copy, so with this many 16 cores
/// seldom wait on one another (`time_the_cache`).
const SHARDS: usize = 256;
/// Bits of each shard's doorkeeper, a Bloom filter of the statements seen once: two
/// bits per statement, cleared once half as many statements as bits went in, so that
/// it forgets what was seen once long ago. A false positive keeps a statement seen
/// once, which costs nothing but its room.
const SEEN_BITS: usize = 4096;

/// Whether rewrite's output for sql may be kept: statements that read or write rows.
/// DDL is translated once and may fold the clock into a DEFAULT; the session's own
/// statements never reach `rewrite`.
pub(crate) fn cacheable(sql: &str) -> bool {
    sql.len() <= MAX_STATEMENT_LEN && reads_or_writes_rows(sql)
}

/// Whether sql is a statement that reads or writes rows, by its first word.
pub(crate) fn reads_or_writes_rows(sql: &str) -> bool {
    let word = sql
        .trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '(')
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    ["SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "VALUES", "TABLE"]
        .iter()
        .any(|w| w.eq_ignore_ascii_case(word))
}

/// The map's keys are already hashes (SipHash with a key of the process's own, so a
/// client cannot choose collisions): its hasher passes them through.
#[derive(Default, Debug)]
struct PassThrough(u64);

impl Hasher for PassThrough {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | u64::from(b);
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

const NIL: u32 = u32::MAX;

#[derive(Debug)]
struct Slot {
    hash: u64,
    key: Box<str>,
    value: Box<str>,
    generation: u64,
    prev: u32,
    next: u32,
}

/// What a lookup found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Lookup {
    Hit(String),
    /// Not held. `admit` says whether the statement was seen once before, and so
    /// should be kept once translated.
    Miss {
        admit: bool,
    },
}

/// A map from statement to translation that forgets the least recently used first
/// once it holds more than its entries or bytes: a slab of slots linked from the most
/// recently used (head) to the least (tail), found by the statement's hash, which the
/// caller computes outside any lock.
#[derive(Debug)]
pub(crate) struct Lru {
    map: HashMap<u64, u32, BuildHasherDefault<PassThrough>>,
    slots: Vec<Slot>,
    free: Vec<u32>,
    head: u32,
    tail: u32,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    /// The doorkeeper (`SEEN_BITS`), and how many statements went in since it was
    /// cleared.
    seen: Box<[u64]>,
    seen_count: usize,
    hits: u64,
    misses: u64,
}

impl Lru {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Lru {
        Lru {
            map: HashMap::default(),
            slots: Vec::new(),
            free: Vec::new(),
            head: NIL,
            tail: NIL,
            bytes: 0,
            max_entries: max_entries.max(1),
            max_bytes,
            seen: vec![0; SEEN_BITS / 64].into_boxed_slice(),
            seen_count: 0,
            hits: 0,
            misses: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    fn cost(key: &str, value: &str) -> usize {
        key.len() + value.len() + ENTRY_OVERHEAD
    }

    fn unlink(&mut self, i: u32) {
        let (prev, next) = {
            let s = &self.slots[i as usize];
            (s.prev, s.next)
        };
        if prev == NIL {
            self.head = next;
        } else {
            self.slots[prev as usize].next = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.slots[next as usize].prev = prev;
        }
    }

    fn push_front(&mut self, i: u32) {
        let old = self.head;
        {
            let s = &mut self.slots[i as usize];
            s.prev = NIL;
            s.next = old;
        }
        if old == NIL {
            self.tail = i;
        } else {
            self.slots[old as usize].prev = i;
        }
        self.head = i;
    }

    fn remove_slot(&mut self, i: u32) {
        self.unlink(i);
        let s = &mut self.slots[i as usize];
        self.bytes -= Self::cost(&s.key, &s.value);
        self.map.remove(&s.hash);
        // A free slot keeps no text.
        s.key = Box::default();
        s.value = Box::default();
        self.free.push(i);
    }

    /// The translation of key (whose hash is h) made under generation, which becomes
    /// the most recently used. One made under another generation is dropped.
    pub(crate) fn lookup(&mut self, key: &str, h: u64, generation: u64) -> Lookup {
        if let Some(&i) = self.map.get(&h) {
            let s = &self.slots[i as usize];
            if s.generation == generation && *s.key == *key {
                let hit = String::from(&*s.value);
                if self.head != i {
                    self.unlink(i);
                    self.push_front(i);
                }
                self.hits += 1;
                return Lookup::Hit(hit);
            }
            // Stale, or another text under the same 64-bit hash: make room for this one.
            self.remove_slot(i);
        }
        self.misses += 1;
        Lookup::Miss {
            admit: self.seen_before(h),
        }
    }

    /// Whether h went through the doorkeeper before; it has now.
    fn seen_before(&mut self, h: u64) -> bool {
        let bits = [(h as usize) % SEEN_BITS, ((h >> 20) as usize) % SEEN_BITS];
        let set = |seen: &[u64], b: usize| seen[b / 64] & (1 << (b % 64)) != 0;
        if bits.iter().all(|&b| set(&self.seen, b)) {
            return true;
        }
        if self.seen_count >= SEEN_BITS / 2 {
            self.seen.fill(0);
            self.seen_count = 0;
        }
        for b in bits {
            self.seen[b / 64] |= 1 << (b % 64);
        }
        self.seen_count += 1;
        false
    }

    /// Keeps key's translation, then forgets the least recently used until both
    /// bounds hold. An entry larger than the byte bound alone is not kept.
    pub(crate) fn insert(&mut self, key: &str, h: u64, value: &str, generation: u64) {
        let cost = Self::cost(key, value);
        if cost > self.max_bytes {
            return;
        }
        if let Some(&i) = self.map.get(&h) {
            self.remove_slot(i);
        }
        while self.tail != NIL && (self.map.len() >= self.max_entries || self.bytes + cost > self.max_bytes) {
            self.remove_slot(self.tail);
        }
        let slot = Slot {
            hash: h,
            key: key.into(),
            value: value.into(),
            generation,
            prev: NIL,
            next: NIL,
        };
        let i = if let Some(i) = self.free.pop() {
            self.slots[i as usize] = slot;
            i
        } else {
            self.slots.push(slot);
            (self.slots.len() - 1) as u32
        };
        self.push_front(i);
        self.map.insert(h, i);
        self.bytes += cost;
    }
}

/// A shard on cache lines of its own, so that two cores locking neighbouring shards
/// do not contend for one line.
#[derive(Debug)]
#[repr(align(128))]
struct Shard(Mutex<Lru>);

/// The process's cache: [`Lru`]s by the statement's hash, each with its share of the
/// bounds.
#[derive(Debug)]
pub(crate) struct Shared {
    shards: Box<[Shard]>,
    hasher: std::collections::hash_map::RandomState,
}

impl Shared {
    pub(crate) fn new(shards: usize, max_entries: usize, max_bytes: usize) -> Shared {
        let shards = shards.max(1);
        Shared {
            shards: (0..shards)
                .map(|_| Shard(Mutex::new(Lru::new(max_entries.div_ceil(shards), max_bytes / shards))))
                .collect(),
            hasher: std::collections::hash_map::RandomState::new(),
        }
    }

    pub(crate) fn hash(&self, key: &str) -> u64 {
        self.hasher.hash_one(key)
    }

    fn shard(&self, h: u64) -> &Mutex<Lru> {
        // The high bits: the low ones index the map and the doorkeeper.
        &self.shards[((h >> 40) % self.shards.len() as u64) as usize].0
    }

    pub(crate) fn lookup(&self, key: &str, h: u64, generation: u64) -> Lookup {
        self.shard(h).locked().lookup(key, h, generation)
    }

    pub(crate) fn insert(&self, key: &str, h: u64, value: &str, generation: u64) {
        self.shard(h).locked().insert(key, h, value, generation);
    }

    /// Entries and bytes held, over every shard, and the hits and misses so far.
    #[cfg(test)]
    pub(crate) fn usage(&self) -> (usize, usize, u64, u64) {
        self.shards.iter().fold((0, 0, 0, 0), |(n, b, hits, misses), s| {
            let s = s.0.locked();
            (n + s.len(), b + s.bytes(), hits + s.hits, misses + s.misses)
        })
    }
}

/// The cache `rewrite` uses.
pub(crate) static CACHE: std::sync::LazyLock<Shared> =
    std::sync::LazyLock::new(|| Shared::new(SHARDS, MAX_ENTRIES, MAX_BYTES));

/// Whether the translation cache is on: OVERLITE_REWRITE_CACHE=0 turns it off.
pub(crate) static ENABLED: std::sync::LazyLock<bool> =
    std::sync::LazyLock::new(|| std::env::var("OVERLITE_REWRITE_CACHE").map_or(true, |v| v != "0"));

/// f's translation of sql, from the cache when it holds one made under the current
/// state of what translations depend on (the module's list).
pub(crate) fn cached(sql: &str, f: impl FnOnce() -> String) -> String {
    if !*ENABLED || !cacheable(sql) {
        return f();
    }
    // Read before translating: a collation made meanwhile leaves this entry stale,
    // never a stale entry under the new generation.
    // A translation typed from the catalog is kept under its context's stamp
    // (ast::env): another database, schema or search_path is another entry.
    let stamp = crate::ast::env::stamp();
    let generation = overlite_engine::collations_generation() ^ stamp;
    let h = CACHE.hash(sql) ^ stamp.rotate_left(29);
    match CACHE.lookup(sql, h, generation) {
        Lookup::Hit(hit) => hit,
        Lookup::Miss { admit } => {
            let out = f();
            if admit {
                CACHE.insert(sql, h, &out, generation);
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> u64 {
        std::collections::hash_map::RandomState::new().hash_one(s) | 1
    }

    /// A cache with fixed hashes, for the tests: the key's own bytes.
    struct T(Lru, std::collections::HashMap<String, u64>);

    impl T {
        fn new(entries: usize, bytes: usize) -> T {
            T(Lru::new(entries, bytes), std::collections::HashMap::new())
        }
        fn hash(&mut self, k: &str) -> u64 {
            *self.1.entry(k.to_string()).or_insert_with(|| h(k))
        }
        fn get(&mut self, k: &str, generation: u64) -> Option<String> {
            let hk = self.hash(k);
            match self.0.lookup(k, hk, generation) {
                Lookup::Hit(v) => Some(v),
                Lookup::Miss { .. } => None,
            }
        }
        fn put(&mut self, k: &str, v: &str, generation: u64) {
            let hk = self.hash(k);
            self.0.insert(k, hk, v, generation);
        }
    }

    #[test]
    fn the_least_recently_used_goes_first() {
        let mut c = T::new(3, usize::MAX);
        c.put("a", "A", 0);
        c.put("b", "B", 0);
        c.put("c", "C", 0);
        assert_eq!(c.get("a", 0).as_deref(), Some("A"));
        c.put("d", "D", 0);
        assert_eq!(c.get("b", 0), None, "b was the least recently used");
        assert_eq!(c.get("a", 0).as_deref(), Some("A"));
        assert_eq!(c.get("c", 0).as_deref(), Some("C"));
        assert_eq!(c.get("d", 0).as_deref(), Some("D"));
        assert_eq!(c.0.len(), 3);
    }

    #[test]
    fn bytes_bound_the_cache_too() {
        let cost = |k: &str, v: &str| k.len() + v.len() + ENTRY_OVERHEAD;
        let mut c = T::new(100, 2 * cost("k1", "0123456789"));
        c.put("k1", "0123456789", 0);
        c.put("k2", "0123456789", 0);
        assert_eq!(c.0.len(), 2);
        c.put("k3", "0123456789", 0);
        assert_eq!(c.0.len(), 2);
        assert_eq!(c.get("k1", 0), None);
        assert!(c.0.bytes() <= 2 * cost("k1", "0123456789"));
        // Larger than the whole bound: not kept, and nothing pushed out for it.
        c.put("big", &"x".repeat(1000), 0);
        assert_eq!(c.get("big", 0), None);
        assert_eq!(c.0.len(), 2);
        // Replacing an entry counts it once.
        c.put("k2", "0123456789", 0);
        c.put("k2", "0123456789", 0);
        assert_eq!(c.0.bytes(), 2 * cost("k1", "0123456789"));
    }

    #[test]
    fn an_entry_of_another_generation_is_not_answered() {
        let mut c = T::new(10, usize::MAX);
        c.put("s", "old", 1);
        assert_eq!(c.get("s", 2), None);
        assert_eq!(c.0.len(), 0, "the stale entry is dropped");
        c.put("s", "new", 2);
        assert_eq!(c.get("s", 2).as_deref(), Some("new"));
    }

    #[test]
    fn a_hash_shared_by_two_texts_answers_neither_wrongly() {
        let mut c = Lru::new(10, usize::MAX);
        c.insert("SELECT 1", 7, "one", 0);
        assert_eq!(c.lookup("SELECT 2", 7, 0), Lookup::Miss { admit: false });
        c.insert("SELECT 2", 7, "two", 0);
        assert_eq!(c.lookup("SELECT 2", 7, 0), Lookup::Hit("two".into()));
        assert!(matches!(c.lookup("SELECT 1", 7, 0), Lookup::Miss { .. }));
    }

    #[test]
    fn a_statement_is_admitted_the_second_time_it_is_missed() {
        let mut c = Lru::new(10, usize::MAX);
        assert_eq!(c.lookup("SELECT 1", 11, 0), Lookup::Miss { admit: false });
        assert_eq!(c.lookup("SELECT 1", 11, 0), Lookup::Miss { admit: true });
        // Two statements sharing a bit, sent in turn, are each admitted.
        let (a, b) = (5 | (1 << 20), 5 | (2 << 20));
        assert_eq!(c.lookup("SELECT 5", a, 0), Lookup::Miss { admit: false });
        assert_eq!(c.lookup("SELECT 6", b, 0), Lookup::Miss { admit: false });
        assert_eq!(c.lookup("SELECT 5", a, 0), Lookup::Miss { admit: true });
        assert_eq!(c.lookup("SELECT 6", b, 0), Lookup::Miss { admit: true });
        // Past half its bits in statements, it starts over.
        for i in 0..SEEN_BITS as u64 {
            c.lookup("SELECT x", 1000 + i * 7919, 0);
        }
        assert_eq!(c.lookup("SELECT 1", 11, 0), Lookup::Miss { admit: false });
    }

    #[test]
    fn slots_are_reused_and_links_hold() {
        let mut c = T::new(8, usize::MAX);
        for round in 0..50u64 {
            for k in 0..20 {
                let key = format!("k{}", (k * 7 + round) % 13);
                if c.get(&key, 0).is_none() {
                    c.put(&key, &key, 0);
                }
            }
            let c = &c.0;
            assert!(c.len() <= 8);
            // Walk the list both ways: it holds every entry once.
            let mut n = 0;
            let mut i = c.head;
            let mut prev = NIL;
            while i != NIL {
                assert_eq!(c.slots[i as usize].prev, prev);
                prev = i;
                i = c.slots[i as usize].next;
                n += 1;
            }
            assert_eq!(prev, c.tail);
            assert_eq!(n, c.len());
        }
        assert!(c.0.slots.len() <= 9, "freed slots are reused: {}", c.0.slots.len());
    }

    #[test]
    fn shards_share_the_bounds() {
        let s = Shared::new(4, 40, usize::MAX);
        for k in 0..1000 {
            let key = format!("SELECT {k}");
            s.insert(&key, s.hash(&key), &key, 0);
        }
        assert!(s.usage().0 <= 40, "{:?}", s.usage());
        let key = "SELECT 999";
        assert_eq!(s.lookup(key, s.hash(key), 0), Lookup::Hit(key.into()));
    }

    #[test]
    fn only_statements_over_rows_are_kept() {
        for sql in [
            "SELECT 1",
            "  select a from t",
            "(SELECT 1) UNION (SELECT 2)",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "INSERT INTO t VALUES (1)",
            "update t set a = 1",
            "DELETE FROM t",
            "VALUES (1)",
            "TABLE t",
        ] {
            assert!(cacheable(sql), "{sql}");
        }
        for sql in [
            "CREATE TABLE t (d date DEFAULT 'today'::date)",
            "ALTER TABLE t ADD COLUMN d date DEFAULT 'now'",
            "CREATE INDEX i ON t (a)",
            "SELECTx",
            "",
        ] {
            assert!(!cacheable(sql), "{sql}");
        }
        assert!(!cacheable(&format!("SELECT '{}'", "x".repeat(MAX_STATEMENT_LEN))));
    }

    /// Lookups per thread with the cache warm, and lookups of statements never seen
    /// (a simple-protocol client's literals), for a cache per thread, one lock, and
    /// the shards: `cargo test --release -p overlite-postgres --lib time_the_cache --
    /// --ignored --nocapture`. Every thread does nothing but this, which is the worst
    /// case for a lock: a server thread spends tens of µs per statement elsewhere.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn time_the_cache() {
        use std::hint::black_box;
        use std::time::Instant;
        let stmts: Vec<(String, String)> = crate::ast::diff::corpus()
            .into_iter()
            .filter(|(_, s)| cacheable(s))
            .take(3000)
            .map(|(_, s)| {
                let out = crate::rewrite::rewrite(&s);
                (s, out)
            })
            .collect();
        let rounds = 100;
        let per_thread = |shared: Option<&Shared>, t: usize, warm: bool| -> f64 {
            let local = Shared::new(1, MAX_ENTRIES, MAX_BYTES);
            let cache = shared.unwrap_or(&local);
            let mut n = 0usize;
            let t0;
            if warm {
                for (k, v) in &stmts {
                    cache.insert(k, cache.hash(k), v, 0);
                }
                t0 = Instant::now();
                for r in 0..rounds {
                    for i in 0..stmts.len() {
                        let (k, _) = &stmts[(i + t * 97 + r) % stmts.len()];
                        black_box(cache.lookup(k, cache.hash(k), 0));
                        n += 1;
                    }
                }
            } else {
                t0 = Instant::now();
                for i in 0..200_000 {
                    let k =
                        format!("SELECT id, upper(v), length(v) FROM kv WHERE id = {t}{i} ORDER BY id DESC LIMIT 1");
                    let h = cache.hash(&k);
                    if let Lookup::Miss { admit: true } = cache.lookup(&k, h, 0) {
                        cache.insert(&k, h, &k, 0);
                    }
                    n += 1;
                }
            }
            t0.elapsed().as_secs_f64() * 1e9 / n as f64
        };
        let run = |threads: usize, kind: &str, warm: bool| -> f64 {
            let shared = match kind {
                "one lock" => Some(Shared::new(1, MAX_ENTRIES, MAX_BYTES)),
                "shards" => Some(Shared::new(SHARDS, MAX_ENTRIES, MAX_BYTES)),
                _ => None,
            };
            let shared = shared.as_ref();
            std::thread::scope(|sc| {
                let hs: Vec<_> = (0..threads)
                    .map(|t| sc.spawn(move || per_thread(shared, t, warm)))
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap_or(f64::NAN)).sum::<f64>() / threads as f64
            })
        };
        for round in 1..=3 {
            for threads in [1, 4, 16] {
                for kind in ["per thread", "one lock", "shards"] {
                    let hit = run(threads, kind, true);
                    let miss = run(threads, kind, false);
                    eprintln!("round {round}, {threads:2} threads, {kind:10}: hit {hit:6.0} ns, unseen {miss:6.0} ns");
                }
            }
        }
    }
}
