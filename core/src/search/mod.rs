use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{MemexError, Result};

mod bm25;
mod collections;
mod commit;
mod fusion;
mod lookup;

#[cfg(test)]
mod test_helpers;

pub use bm25::{sanitize_query, search_titles};
pub use collections::{normalize_collections, validate_collection_names};
pub use commit::{
    CommitOutcome, CommitResult, DocSpec, IngestWikiPage, commit_doc, upsert_document,
};
pub use fusion::rrf_fuse;
pub use lookup::{DocLookup, DocumentMeta, SourceListRow, resolve_ref};

/// Minimum relevance score threshold. Currently 0.0 — no filtering
/// (matches QMD's `hybridQuery` default).
pub const MIN_SCORE: f32 = 0.0;

/// RFC 3339 timestamp at seconds precision, UTC.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A single search result from the BM25 full-text index.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: PathBuf,
    pub title: String,
    /// Relevance score normalized to 0.0-1.0 range.
    pub score: f32,
    /// Chunk body for the matched span. Empty until populated by the
    /// retrieval pass that runs the stat-check + body-attach.
    pub body: String,
    /// Document type (wiki or raw).
    pub doc_type: String,
    /// Full SHA-256 content hash. Identity for the document; short forms
    /// are computed via `crate::docid::short` at display seams.
    pub hash: String,
    /// Chunk index (0-based) within the doc. `Some` when this result was
    /// retrieved at chunk granularity (chunk-level BM25 or vector probe);
    /// `None` for doc-granularity results (legacy paths, title-only search).
    /// Used by RRF to dedupe at chunk level — multiple chunks of the same
    /// doc are kept as separate results — and by `populate_bodies` to
    /// confine the body slice to that specific chunk.
    pub chunk_seq: Option<i32>,
}

// ---------------------------------------------------------------------------
// New public functions for the content-addressable schema
// ---------------------------------------------------------------------------

/// Strong signal detection: s1 >= 0.85 AND (s1 - s2) >= 0.15
///
/// If there's only one result (s2 = 0.0), a high s1 is still strong.
///
/// When `intent` is `Some(_)`, returns `false` unconditionally — an
/// explicit user intent always forces the full expansion+rerank
/// pipeline so the focused-snippet line picker can use intent terms
/// to pick the most relevant slice of each chunk.
pub fn is_strong_signal(s1: f64, s2: f64, intent: Option<&str>) -> bool {
    if intent.is_some() {
        return false;
    }
    s1 >= 0.85 && (s1 - s2) >= 0.15
}

// ---------------------------------------------------------------------------
// Db
// ---------------------------------------------------------------------------

/// BM25 full-text search backed by SQLite FTS5.
///
/// Uses the new content-addressable schema (content/documents/documents_fts)
/// from `schema::init_schema`. The inner `Connection` is wrapped in a `Mutex`
/// so that `Db` is `Send + Sync`.
pub struct Db {
    pub(crate) conn: Mutex<rusqlite::Connection>,
}

impl Db {
    /// Open (or create) the search database at `db_path`.
    ///
    /// Creates the new content-addressable schema (content, documents, documents_fts)
    /// via `schema::init_schema`. Enables WAL mode for concurrent reads.
    pub fn open(db_path: &Path) -> Result<Self> {
        // Must precede Connection::open so the new connection picks up
        // the sqlite-vec extension via sqlite3_auto_extension.
        crate::schema::register_sqlite_vec_once();
        let conn = rusqlite::Connection::open(db_path).map_err(sqlite_err)?;

        // busy_timeout must be armed before ANY statement that can hit a
        // file-level lock — including the `PRAGMA journal_mode=WAL` below,
        // which needs brief exclusive access. Without this, concurrent first
        // opens on the same DB hit "database is locked" without retry.
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .map_err(sqlite_err)?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(sqlite_err)?;

        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(sqlite_err)?;

        crate::schema::init_schema(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Returns `true` if the documents table contains zero rows.
    pub fn is_empty(&self) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
            .map_err(sqlite_err)?;
        Ok(count == 0)
    }

    /// Execute a closure with access to the underlying SQLite connection.
    ///
    /// This is used by vector operations (e.g. `vector::store_chunk`) that need
    /// direct connection access.
    pub fn with_connection<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T>,
    {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        f(&conn)
    }

    /// Run `f` inside a committed transaction. Rollback is automatic if `f`
    /// returns `Err` or panics (rusqlite's `Transaction` drops without
    /// commit).
    pub fn with_transaction<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<T>,
    {
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        let out = f(&tx)?;
        tx.commit().map_err(sqlite_err)?;
        Ok(out)
    }
}

/// Test-only helpers. Kept in their own impl block so production reviewers
/// can see at a glance what's not part of the runtime API.
#[cfg(any(test, feature = "test-utils"))]
impl Db {
    pub fn conn_for_test(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().expect("lock poisoned in test")
    }

    /// Insert a document row with the body-only hash convention used by
    /// `commit_doc`. No production callers — used by in-module tests
    /// to seed search/collections fixtures.
    pub fn index_page(
        &self,
        path: &Path,
        title: &str,
        body: &str,
        last_modified: i64,
    ) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let path_str = path.to_string_lossy().to_string();
        let doc_type = if path_str.starts_with("raw/") {
            "raw"
        } else {
            "wiki"
        };
        let (id, _hash) = upsert_document(
            &conn,
            &DocSpec {
                doc_type,
                path: &path_str,
                title,
                source: None,
                body,
                mtime: crate::storage::nanos_to_systime(last_modified),
                size: body.len() as i64,
            },
        )?;
        Ok(id)
    }
}

/// Parse a wiki page's content into (title, body, summary, collections)
/// for indexing.
///
/// Uses `crate::validate::parse_frontmatter` to extract frontmatter
/// fields. Returns `None` if the page has no valid frontmatter. Summary
/// is taken from the frontmatter `summary` field if present, otherwise
/// extracted from the first non-empty body line (truncated to 120 chars).
pub fn parse_page_for_indexing(content: &str) -> Option<(String, String, String, Vec<String>)> {
    let (fm, body) = crate::validate::parse_frontmatter(content).ok()?;
    if fm.title.trim().is_empty() {
        return None;
    }
    let summary = fm
        .summary
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::index::extract_summary(&body, 120));
    let collections = normalize_collections(&fm.collections);
    Some((fm.title, body, summary, collections))
}

/// Convert a rusqlite error into a MemexError::Io.
pub(crate) fn sqlite_err(e: rusqlite::Error) -> MemexError {
    MemexError::Io(std::io::Error::other(e.to_string()))
}

/// Convert a mutex poison error into a MemexError::Io.
pub(crate) fn mutex_err<T>(e: &std::sync::PoisonError<T>) -> MemexError {
    MemexError::Io(std::io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::test_helpers::make_page;

    // Tests for module-local items: is_strong_signal and
    // parse_page_for_indexing. Tests for everything else live next to
    // the code they exercise (bm25.rs, commit.rs, collections.rs,
    // fusion.rs, lookup.rs, ingest_jobs.rs).

    #[test]
    fn signal_detection_strong() {
        assert!(is_strong_signal(0.91, 0.67, None));
    }

    #[test]
    fn signal_detection_weak() {
        assert!(!is_strong_signal(0.70, 0.60, None));
    }

    #[test]
    fn signal_detection_single_result() {
        assert!(is_strong_signal(0.91, 0.0, None));
    }

    #[test]
    fn signal_detection_intent_forces_weak() {
        // An explicit intent forces weak so the expansion+rerank
        // pipeline runs even on textbook strong scores.
        assert!(!is_strong_signal(0.91, 0.0, Some("user intent here")));
        assert!(!is_strong_signal(0.99, 0.10, Some("more context")));
    }

    #[test]
    fn parse_page_for_indexing_valid() {
        let page = make_page("Test Title", "Some body text.");
        let (title, body, _summary, collections) = parse_page_for_indexing(&page).unwrap();
        assert_eq!(title, "Test Title");
        assert!(body.contains("Some body text."));
        assert_eq!(collections, vec!["default".to_string()]);
    }

    #[test]
    fn parse_page_for_indexing_no_frontmatter() {
        assert!(parse_page_for_indexing("Just plain text.").is_none());
    }

    #[test]
    fn parse_page_for_indexing_empty_title() {
        let page = "---\ntitle: \"\"
created_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\nbody";
        assert!(parse_page_for_indexing(page).is_none());
    }
}

#[cfg(test)]
mod parallel_access_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn bm25_search_open_sets_busy_timeout() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join(".search.db");
        let search = Db::open(&db_path).unwrap();

        let timeout_ms: i64 = search
            .with_connection(|conn| {
                Ok(conn
                    .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                    .unwrap())
            })
            .unwrap();

        assert_eq!(timeout_ms, 5000);
    }
}
