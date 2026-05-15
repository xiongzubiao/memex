//! Lazily-opened, single-root Memex handle.
//!
//! A daemon process serves exactly one `MEMEX_ROOT` for its lifetime
//! (see `memex daemon start`). Every async task in the daemon —
//! request handlers, retrieval actor, watcher, worker pool — needs
//! the same `Arc<Memex>` for that root. Sharing one `Arc` serializes
//! DB access via `Db`'s internal mutex and avoids file-level
//! lock contention that would happen if each task opened its own
//! connection.
//!
//! The handle holds a single optional value. The first `get_or_open`
//! call binds the root and runs `Memex::open` (PRAGMA WAL + schema
//! init); subsequent calls with the same root return the bound
//! `Arc<Memex>`, and calls with a different root return an error.
//! Lazy because some tests create a daemon harness that never
//! receives a request — eager open at startup would pay the
//! schema-init cost across every test for no reason.
//!
//! Open runs INSIDE the mutex so concurrent first-hits can't race
//! on `PRAGMA journal_mode=WAL`, which needs a brief exclusive
//! file lock during init.
//!
//! If memex ever grows multi-tenant deployment, this module — not
//! callers — is where to revisit. Callers can rely on single-root
//! semantics today.

use memex_core::Memex;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct MemexHandle {
    state: Mutex<Option<(PathBuf, Arc<Memex>)>>,
}

impl MemexHandle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(None),
        })
    }

    /// Return the bound `Arc<Memex>`, or `None` if `get_or_open` hasn't
    /// been called yet. Used by the worker pool to access the active
    /// Memex for cache operations without an explicit root path.
    pub fn get(&self) -> Option<Arc<Memex>> {
        let g = self.state.lock().expect("MemexHandle mutex poisoned");
        g.as_ref().map(|(_root, h)| h.clone())
    }

    /// Look up the bound handle, opening on first call. Subsequent
    /// calls with the same `root` return the existing handle. A
    /// different `root` is a single-root invariant violation and
    /// surfaces as `MemexError::Other`.
    pub fn get_or_open(&self, root: &Path) -> memex_core::error::Result<Arc<Memex>> {
        let mut g = self.state.lock().expect("MemexHandle mutex poisoned");
        if let Some((bound_root, h)) = g.as_ref() {
            if bound_root == root {
                return Ok(h.clone());
            }
            return Err(memex_core::error::MemexError::Other(anyhow::anyhow!(
                "MemexHandle is single-root; already bound to {:?}, refusing different root {:?}",
                bound_root,
                root
            )));
        }
        let opened = Arc::new(Memex::open(root.to_path_buf())?);
        *g = Some((root.to_path_buf(), opened.clone()));
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
    fn returns_same_arc_on_second_call() {
        let (_tmp, root) = fresh_root();
        let h = MemexHandle::new();
        let h1 = h.get_or_open(&root).unwrap();
        let h2 = h.get_or_open(&root).unwrap();
        assert!(
            Arc::ptr_eq(&h1, &h2),
            "second call must return the bound Arc"
        );
    }

    #[test]
    fn second_root_is_rejected() {
        let (_tmp1, r1) = fresh_root();
        let (_tmp2, r2) = fresh_root();
        let h = MemexHandle::new();
        h.get_or_open(&r1).unwrap();
        let err = h.get_or_open(&r2).expect_err("second root must error");
        assert!(
            err.to_string().contains("single-root"),
            "expected single-root error, got: {err}"
        );
    }

    #[test]
    fn get_returns_none_before_open_then_handle_after() {
        let (_tmp, root) = fresh_root();
        let h = MemexHandle::new();
        assert!(h.get().is_none(), "unbound handle must return None");
        let bound = h.get_or_open(&root).unwrap();
        let got = h.get().expect("get must return after open");
        assert!(Arc::ptr_eq(&bound, &got));
    }
}
