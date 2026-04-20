pub mod config;
pub mod content;
pub mod crosslink;
pub mod docid;
pub mod embed;
pub mod error;
pub mod index;
pub mod lint;
pub mod model;
pub mod retrieval;
pub mod schema;
pub mod search;
pub mod storage;
pub mod types;
pub mod transcript;
pub mod validate;
pub mod vector;

use std::fs;
use std::path::{Path, PathBuf};

use config::Config;
use search::Bm25Search;

/// Filename for the BM25 + content SQLite database.
pub const SEARCH_DB_NAME: &str = ".search.db";

/// RAII writer lock. Releases OS-level flock on Drop. OS also releases on
/// process crash, so no manual cleanup needed even on SIGKILL.
pub(crate) struct WriterLock {
    file: fs::File,
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        use fs2::FileExt;
        let _ = FileExt::unlock(&self.file);
    }
}

/// The Memex knowledge base.
///
/// `_writer_lock` is `Some(_)` when this handle holds the exclusive writer flock
/// on `{root}/.lock`. Mutating methods check this via `require_writer()`.
pub struct Memex {
    root: PathBuf,
    search: Bm25Search,
    _writer_lock: Option<WriterLock>,
    config: Config,
}

impl std::fmt::Debug for Memex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memex")
            .field("root", &self.root)
            .field("is_writer", &self._writer_lock.is_some())
            .finish_non_exhaustive()
    }
}

impl Memex {
    /// Read-only handle. No lock acquired. Concurrent readers + one writer OK
    /// via SQLite WAL. Returns a usable handle even if DB is empty — search
    /// returns empty, lint scans work.
    ///
    /// Propagates Config::load errors (MalformedConfig, InvalidEnvVar) rather
    /// than silently defaulting — the spec requires every command to fail
    /// with a clear error when the user's configured timeout is unusable.
    pub fn open(root: PathBuf) -> error::Result<Self> {
        let config = Config::load(&root)?;
        if !root.join("wiki").is_dir() {
            let wiki_dir = root.join("wiki");
            fs::create_dir_all(&wiki_dir).map_err(|e| error::MemexError::FileOpFailed {
                path: wiki_dir.clone(),
                operation: "create wiki dir",
                source: e,
            })?;
        }
        let search = Bm25Search::open(&root.join(SEARCH_DB_NAME))?;

        // Migration hint: if DB is empty but wiki/ has .md files, suggest rebuild.
        // Spec Section 7 — r1 auto-rebuilt here; we moved that to open_writer to
        // eliminate the two-reader rebuild race. Now hint instead.
        if search.is_empty()? {
            let wiki_dir = root.join("wiki");
            if wiki_dir.is_dir() && any_md_file(&wiki_dir) {
                eprintln!(
                    "note: search index is empty but `{}` contains markdown files; \
                     run `memex lint --fix` or any write command to rebuild.",
                    wiki_dir.display()
                );
            }
        }

        Ok(Self {
            root,
            search,
            _writer_lock: None,
            config,
        })
    }

    /// Writer handle. Acquires exclusive flock with config-provided timeout.
    /// If DB is empty, rebuilds from disk (safe — under lock). Runs stale-tmp
    /// cleanup. Error taxonomy: TimedOut → LockTimeout, other I/O → LockAcquireIo.
    pub fn open_writer(root: PathBuf) -> error::Result<Self> {
        let config = Config::load(&root)?;
        Self::open_writer_with_config(root, config)
    }

    fn open_writer_with_config(root: PathBuf, config: Config) -> error::Result<Self> {
        if !root.join("wiki").is_dir() {
            let wiki_dir = root.join("wiki");
            fs::create_dir_all(&wiki_dir).map_err(|e| error::MemexError::FileOpFailed {
                path: wiki_dir.clone(),
                operation: "create wiki dir",
                source: e,
            })?;
        }

        let lock_path = root.join(".lock");
        let lock_file =
            storage::try_acquire_lock(&lock_path, config.lock_timeout).map_err(|e| {
                match e.kind() {
                    std::io::ErrorKind::TimedOut => error::MemexError::LockTimeout {
                        timeout_secs: config.lock_timeout.as_secs(),
                        lock_path: lock_path.clone(),
                    },
                    _ => error::MemexError::LockAcquireIo {
                        lock_path: lock_path.clone(),
                        source: e,
                    },
                }
            })?;
        let writer_lock = WriterLock { file: lock_file };

        let search = Bm25Search::open(&root.join(SEARCH_DB_NAME))?;
        if search.is_empty()? {
            search.rebuild(&root)?;
        }

        // Stale tmp cleanup — safe because we hold the writer flock.
        storage::cleanup_stale_tmp_files(&root.join("wiki"));

        Ok(Self {
            root,
            search,
            _writer_lock: Some(writer_lock),
            config,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn wiki_dir(&self) -> PathBuf {
        self.root.join("wiki")
    }

    pub fn search(&self) -> &Bm25Search {
        &self.search
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn is_writer(&self) -> bool {
        self._writer_lock.is_some()
    }

    pub(crate) fn require_writer(&self) -> error::Result<()> {
        if self._writer_lock.is_none() {
            return Err(error::MemexError::Internal(
                "attempted mutating operation on read-only Memex handle".into(),
            ));
        }
        Ok(())
    }

    pub fn read_index(&self) -> error::Result<String> {
        self.search.generate_index()
    }

    pub fn reindex(&self) -> error::Result<()> {
        self.require_writer()?;
        self.search.rebuild(&self.root)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixOutcome {
    Applied,
    Stale,
}

impl Memex {
    /// Writer-handle fix: lock already held by self._writer_lock.
    pub fn apply_fix(&self, issue: &types::LintIssue) -> error::Result<FixOutcome> {
        self.require_writer()?;
        if !lint::is_issue_still_present(&self.search, &self.root, issue)? {
            return Ok(FixOutcome::Stale);
        }
        lint::apply_fix_inner(&self.search, &self.root, issue)?;
        Ok(FixOutcome::Applied)
    }

    /// Reader-handle fix: briefly acquires writer lock, opens a FRESH
    /// SQLite connection (avoids WAL-snapshot staleness from self.search),
    /// re-verifies under lock, applies if still needed, releases.
    ///
    /// When called on a writer handle, delegates to apply_fix (lock already held).
    pub fn apply_fix_locked(&self, issue: &types::LintIssue) -> error::Result<FixOutcome> {
        if self.is_writer() {
            return self.apply_fix(issue);
        }

        let lock_path = self.root.join(".lock");
        let lock_file =
            storage::try_acquire_lock(&lock_path, self.config.lock_timeout).map_err(|e| match e
                .kind()
            {
                std::io::ErrorKind::TimedOut => error::MemexError::LockTimeout {
                    timeout_secs: self.config.lock_timeout.as_secs(),
                    lock_path: lock_path.clone(),
                },
                _ => error::MemexError::LockAcquireIo {
                    lock_path: lock_path.clone(),
                    source: e,
                },
            })?;
        let _guard = WriterLock { file: lock_file };

        // Fresh connection — sees the latest committed state, not the reader's
        // pinned WAL snapshot from process start.
        let fresh = search::Bm25Search::open(&self.root.join(SEARCH_DB_NAME))?;

        if !lint::is_issue_still_present(&fresh, &self.root, issue)? {
            return Ok(FixOutcome::Stale);
        }
        lint::apply_fix_inner(&fresh, &self.root, issue)?;
        Ok(FixOutcome::Applied)
    }
}

/// Test-only constructor: lets tests inject a custom timeout without touching
/// MEMEX_LOCK_TIMEOUT_SECONDS (which is process-global and races with parallel tests).
#[cfg(any(test, feature = "test-utils"))]
impl Memex {
    pub fn open_writer_with_timeout(
        root: PathBuf,
        timeout: std::time::Duration,
    ) -> error::Result<Self> {
        Self::open_writer_with_config(
            root,
            Config {
                lock_timeout: timeout,
            },
        )
    }
}

/// True if `dir` contains any *.md file (non-recursive; wiki/ is flat).
fn any_md_file(dir: &Path) -> bool {
    fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|x| x == "md"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types;
    use tempfile::TempDir;

    #[test]
    fn open_creates_wiki_directory() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let memex = Memex::open(root.clone()).unwrap();
        assert!(root.join("wiki").is_dir());
        assert!(root.join(SEARCH_DB_NAME).exists());
        assert_eq!(memex.root(), root);
    }

    #[test]
    fn open_existing_memex() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        Memex::open(root.clone()).unwrap();
        let memex = Memex::open(root.clone()).unwrap();
        assert_eq!(memex.root(), root);
    }

    #[test]
    fn memex_open_creates_search_db() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let _memex = Memex::open(root.clone()).unwrap();
        assert!(root.join(SEARCH_DB_NAME).exists());
    }

    #[test]
    fn reindex_requires_writer() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let memex = Memex::open(root.clone()).unwrap();
        assert!(matches!(
            memex.reindex(),
            Err(error::MemexError::Internal(_))
        ));
    }

    #[test]
    fn reindex_rebuilds_from_wiki_pages() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let writer = Memex::open_writer(root.clone()).unwrap();
        std::fs::write(
            root.join("wiki/test-page.md"),
            "---\ntitle: Test Page\ntags:\n  - entity\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nTest content.\n",
        ).unwrap();
        writer.reindex().unwrap();
        let idx = writer.read_index().unwrap();
        assert!(idx.contains("Test Page"), "got: {idx}");
    }

    #[test]
    fn open_does_not_create_lock_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let _m = Memex::open(root.clone()).unwrap();
        assert!(!root.join(".lock").exists());
    }

    #[test]
    fn open_writer_creates_lock_file() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let _m = Memex::open_writer(root.clone()).unwrap();
        assert!(root.join(".lock").exists());
    }

    #[test]
    fn open_writer_drops_release_lock() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        {
            let _m = Memex::open_writer(root.clone()).unwrap();
        }
        let start = std::time::Instant::now();
        let _m = Memex::open_writer(root).unwrap();
        assert!(start.elapsed() < std::time::Duration::from_millis(100));
    }

    #[test]
    fn open_writer_with_timeout_returns_lock_timeout() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let _holder = Memex::open_writer(root.clone()).unwrap();
        let result = Memex::open_writer_with_timeout(root, std::time::Duration::from_millis(100));
        assert!(matches!(result, Err(error::MemexError::LockTimeout { .. })));
    }

    #[test]
    fn apply_fix_locked_returns_stale_when_no_issue() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let reader = Memex::open(root.clone()).unwrap();
        // Fabricate an issue for a page that doesn't exist.
        let bogus = types::LintIssue {
            kind: types::LintIssueKind::StaleIndex,
            page: "ghost".into(),
            target: "wiki/ghost.md".into(),
        };
        let outcome = reader.apply_fix_locked(&bogus).unwrap();
        assert!(matches!(outcome, FixOutcome::Stale));
    }
}
