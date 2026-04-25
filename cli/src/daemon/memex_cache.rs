//! Shared per-root Memex handle cache.
//!
//! Both the retrieval actor (query path) and the handler (ingest path)
//! need a `memex_core::Memex` handle keyed on `memex_root`. Opening a
//! handle runs SQLite schema init, which is expensive and must not race
//! across threads against the same root. Sharing one `Arc<Memex>` per
//! root also serializes DB access via `Bm25Search`'s internal mutex,
//! avoiding file-level lock contention from independent connections.
//!
//! The cache is unbounded: one daemon typically serves one root for its
//! entire lifetime (`memex daemon start` is keyed on `MEMEX_ROOT`), so
//! the map holds at most one entry in practice. If memex ever grows a
//! multi-tenant deployment, revisit with eviction policy informed by real
//! usage data.

use memex_core::Memex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct MemexCache {
    handles: Mutex<HashMap<PathBuf, Arc<Memex>>>,
}

impl MemexCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            handles: Mutex::new(HashMap::new()),
        })
    }

    /// Look up a cached handle, opening a new one if absent. Open runs
    /// INSIDE the cache mutex so concurrent first-hits on the same root
    /// can't race on `PRAGMA journal_mode=WAL`, which needs a brief
    /// exclusive file lock. In practice the daemon holds a single root for
    /// its lifetime, so blocking other-root lookups during a first open
    /// (~100ms for schema init) is a non-issue.
    pub fn get_or_open(&self, root: &Path) -> memex_core::error::Result<Arc<Memex>> {
        let mut g = self.handles.lock().expect("memex cache mutex poisoned");
        if let Some(h) = g.get(root).cloned() {
            return Ok(h);
        }
        let opened = Arc::new(Memex::open(root.to_path_buf())?);
        g.insert(root.to_path_buf(), opened.clone());
        Ok(opened)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh_root() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("memex");
        std::fs::create_dir_all(&root).unwrap();
        (tmp, root)
    }

    #[test]
    fn caches_handle_per_root_and_returns_same_arc_on_second_call() {
        let (_tmp, root) = fresh_root();
        let cache = MemexCache::new();
        let h1 = cache.get_or_open(&root).unwrap();
        let h2 = cache.get_or_open(&root).unwrap();
        assert!(Arc::ptr_eq(&h1, &h2), "second call must return the cached Arc");
    }

    #[test]
    fn distinct_roots_get_distinct_handles() {
        let (_tmp1, r1) = fresh_root();
        let (_tmp2, r2) = fresh_root();
        let cache = MemexCache::new();
        let h1 = cache.get_or_open(&r1).unwrap();
        let h2 = cache.get_or_open(&r2).unwrap();
        assert!(!Arc::ptr_eq(&h1, &h2));
    }
}
