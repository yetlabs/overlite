//! What a translation may read of the catalog: the type environment's context.
//!
//! The session sets it around each translation of a statement it runs or describes
//! (`with`), from its connection's snapshot of the tables' columns
//! (`overlite_engine::TableTypes`) and its search_path. A translation made under a
//! context is kept in the cache under the context's stamp, so that it is answered only
//! while the same snapshot and search_path stand; one made with none (the session's own
//! probes, DDL helpers) is the untyped translation the engine's type passes finish.

use overlite_engine::{ColumnType, TableTypes};
use std::cell::RefCell;
use std::hash::{BuildHasher, Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;

/// The catalog as one translation sees it.
#[derive(Debug)]
pub(crate) struct Context {
    pub types: Arc<TableTypes>,
    /// The schemas an unqualified table is looked for in, in order, public last.
    search_path: Vec<String>,
    /// What a translation made under this context is kept under.
    pub stamp: u64,
}

impl Context {
    pub(crate) fn new(types: Arc<TableTypes>, search_path: Option<&[String]>) -> Context {
        let search_path: Vec<String> = search_path
            .unwrap_or(&[])
            .iter()
            .map(|s| s.to_lowercase())
            .filter(|s| !matches!(s.as_str(), "public" | "$user" | "pg_catalog"))
            .collect();
        let mut h =
            std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default().build_hasher();
        types.generation.hash(&mut h);
        search_path.hash(&mut h);
        // Never 0, which is the stamp of a translation made with no context.
        let stamp = h.finish() | 1;
        Context {
            types,
            search_path,
            stamp,
        }
    }

    /// The columns of a table or view as the statement names it: qualified, or by the
    /// search_path's schemas, then public.
    pub(crate) fn table(&self, schema: &str, name: &str) -> Option<&[ColumnType]> {
        if !schema.is_empty() {
            return if schema.eq_ignore_ascii_case("public") {
                self.types.table(name)
            } else {
                self.types.table(&[schema, ".", name].concat())
            };
        }
        for s in &self.search_path {
            if let Some(t) = self.types.table(&[s, ".", name].concat()) {
                return Some(t);
            }
        }
        self.types.table(name)
    }
}

thread_local! {
    static CONTEXT: RefCell<Option<Rc<Context>>> = const { RefCell::new(None) };
}

/// The context of the translation running on this thread, if any.
pub(crate) fn current() -> Option<Rc<Context>> {
    CONTEXT.with(|c| c.borrow().clone())
}

/// The stamp the translation cache keeps a translation under: the current context's,
/// or 0 with none.
pub(crate) fn stamp() -> u64 {
    CONTEXT.with(|c| c.borrow().as_ref().map_or(0, |c| c.stamp))
}

/// Runs f with ctx as the context of the translations it makes, the previous one
/// restored after.
pub(crate) fn with<T>(ctx: Option<Context>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Rc<Context>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            CONTEXT.with(|c| *c.borrow_mut() = prev);
        }
    }
    let prev = CONTEXT.with(|c| c.replace(ctx.map(Rc::new)));
    let _restore = Restore(prev);
    f()
}

/// Runs f with no context: what is translated in it is untyped.
pub(crate) fn without<T>(f: impl FnOnce() -> T) -> T {
    with(None, f)
}
