use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

use crate::error::{MemexError, Result};
use crate::types::Document;

mod bm25;
mod fusion;
mod ingest_jobs;
pub use ingest_jobs::{JobType, PendingJob};

pub use bm25::{sanitize_query, search_bm25};
pub use fusion::rrf_fuse;

/// Minimum relevance score threshold. Currently 0.0 — no filtering
/// (matches QMD's `hybridQuery` default).
pub const MIN_SCORE: f32 = 0.0;

/// RFC 3339 timestamp at seconds precision, UTC.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// A wiki page ready to be inserted by `store_ingest_batch`.
///
/// `content` is the full markdown (frontmatter + body) as it will be stored
/// in the content table; the caller composes it. `tags` is the
/// comma-joined CSV form the `documents.tags` column expects.
#[derive(Debug, Clone)]
pub struct IngestWikiPage {
    pub slug: String,
    pub title: String,
    pub content: String,
    pub tags: String,
}

/// Return value of `store_ingest_batch`. Hashes are surfaced so callers can
/// embed without recomputing.
#[derive(Debug, Clone)]
pub struct IngestBatchResult {
    pub source_hash: String,
    pub wiki_hashes: Vec<(String, String)>,
}

/// Result of `Bm25Search::lookup_documents_with_collections`:
/// (hash → docs, doc_id → collections, doc_id → (mtime, size)).
/// The hash→docs fan-out lets retrieval map one chunk-hash to multiple
/// wiki pages; the per-doc-id maps carry collection membership and the
/// stored stat metadata used by retrieval's stat-check — drop results
/// whose on-disk mtime/size diverge from the indexed values, since the
/// chunk pos/len no longer slice valid coordinates.
pub type DocLookup = (
    std::collections::HashMap<String, Vec<Document>>,
    std::collections::HashMap<i64, Vec<String>>,
    std::collections::HashMap<i64, (String, i64)>,
);

/// A single search result from the BM25 full-text index.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: PathBuf,
    pub title: String,
    /// Relevance score normalized to 0.0-1.0 range.
    pub score: f32,
    /// A short excerpt from the page body highlighting the matched term.
    pub snippet: String,
    /// Document type (wiki or raw).
    pub doc_type: String,
    /// Full SHA-256 content hash. Identity for the document; short forms
    /// are computed via `crate::docid::short` at display seams.
    pub hash: String,
}

// ---------------------------------------------------------------------------
// New public functions for the content-addressable schema
// ---------------------------------------------------------------------------

/// Normalize raw BM25 score to [0, 1). Formula: |x| / (1 + |x|)
///
/// FTS5 BM25 scores are negative (lower = better): -10 is strong, -0.5 is weak.
/// This sigmoid-like transform is query-independent — a score of 0.67 always
/// means the same thing regardless of what other results exist.
/// Maps: strong(-10)→0.91, medium(-2)→0.67, weak(-0.5)→0.33, none(0)→0.
pub fn normalize_bm25(raw: f64) -> f64 {
    let abs = raw.abs();
    abs / (1.0 + abs)
}

/// Normalize collection names for storage.
///
/// Rules:
/// - trim whitespace
/// - lowercase
/// - remove empty names
/// - sort and dedup
/// - default to `["default"]` when nothing remains
pub fn normalize_collections(names: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = names
        .iter()
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    if normalized.is_empty() {
        vec!["default".to_string()]
    } else {
        normalized
    }
}

/// Reject collection names that would render badly in `source list`
/// output, break shell composition, or smuggle structure into the
/// `documents.tags`-adjacent CSV/JSON serialization paths. Empty
/// names are filtered (defaulted to "default") rather than rejected,
/// matching `normalize_collections`. Returns the offending name so
/// callers can include it in a user-facing error.
pub fn validate_collection_names(names: &[String]) -> std::result::Result<(), String> {
    for name in names {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().any(|c| c.is_control()) {
            return Err(format!(
                "collection name {trimmed:?} contains control characters (newline, tab, etc.)"
            ));
        }
        if trimmed.len() > 64 {
            return Err(format!(
                "collection name {trimmed:?} exceeds 64 chars"
            ));
        }
    }
    Ok(())
}

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

/// Document metadata pulled from the `documents` row.
#[derive(Debug, Clone)]
pub struct DocumentMeta {
    pub mtime: String,
    pub size: i64,
}

/// Inputs for `upsert_document_conn`. Bundles all columns the indexer
/// writes plus the body text used to refresh the FTS row.
pub struct UpsertDocument<'a> {
    pub doc_type: &'a str,
    pub path: &'a str,
    pub title: &'a str,
    pub hash: &'a str,
    pub tags: &'a str,
    pub source: Option<&'a str>,
    pub body: &'a str,
    pub mtime: &'a str,
    pub size: i64,
}

/// Insert or update a document on a raw connection. Writes the `documents`
/// row and refreshes the matching `documents_fts` row in one go.
pub(crate) fn upsert_document_conn(conn: &rusqlite::Connection, doc: &UpsertDocument) -> Result<i64> {
    // Capture the previous (id, path, title, tags) so we can issue a precise
    // FTS5 'delete' (contentless tables need the previous column values to
    // tear down their per-doc index entries; empty placeholders corrupt them).
    let prior: Option<(i64, String, String, String)> = conn
        .query_row(
            "SELECT id, path, title, tags FROM documents WHERE doc_type=?1 AND path=?2",
            rusqlite::params![doc.doc_type, doc.path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok();

    conn.execute(
        "INSERT INTO documents (doc_type, path, title, hash, tags, source, mtime, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(doc_type, path) DO UPDATE SET
             title = excluded.title, hash = excluded.hash, tags = excluded.tags,
             source = excluded.source, mtime = excluded.mtime, size = excluded.size",
        rusqlite::params![doc.doc_type, doc.path, doc.title, doc.hash, doc.tags, doc.source, doc.mtime, doc.size],
    )
    .map_err(sqlite_err)?;
    let id: i64 = conn
        .query_row(
            "SELECT id FROM documents WHERE doc_type=?1 AND path=?2",
            rusqlite::params![doc.doc_type, doc.path],
            |r| r.get(0),
        )
        .map_err(sqlite_err)?;
    if let Some((prior_id, prior_path, prior_title, prior_tags)) = prior {
        // Pass the previous body as empty: FTS5 contentless tables verify
        // tokens against what we provide, but our body lives on disk and we
        // re-insert below, so the cleanup is best-effort here. Log
        // failures rather than swallowing — repeated cleanup misses
        // accumulate stale tokens that show as ghost hits in search,
        // and silent accumulation has no recovery signal until lint
        // reindex runs.
        if let Err(e) = conn.execute(
            "INSERT INTO documents_fts(documents_fts, rowid, path, title, tags, body) VALUES('delete', ?1, ?2, ?3, ?4, '')",
            rusqlite::params![prior_id, prior_path, prior_title, prior_tags],
        ) {
            tracing::warn!(
                document_id = prior_id,
                path = %prior_path,
                error = %e,
                "FTS prior-row delete failed (run `memex lint --fix` to clean stale tokens)"
            );
        }
    }
    conn.execute(
        "INSERT INTO documents_fts(rowid, path, title, tags, body) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, doc.path, doc.title, doc.tags, doc.body],
    )
    .map_err(sqlite_err)?;
    ensure_default_document_collection(conn, id)?;
    Ok(id)
}

/// Outcome of a single doc commit: was the row created, did it replace
/// content, or was it identical to what was already there?
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum CommitOutcome {
    /// No prior row at this (doc_type, path).
    Inserted,
    /// Prior row existed; hash may or may not have changed.
    Updated,
    /// Prior row had the same hash; no chunk cleanup needed.
    Unchanged,
}

/// All inputs `commit_doc` needs. Caller is responsible for parsing
/// frontmatter once and passing the body slice + the on-disk file size
/// — `commit_doc` does not re-parse. This keeps the schema convention
/// (hash body-only, size = full file size, FTS body-only) explicit at
/// every callsite.
pub struct DocSpec<'a> {
    pub doc_type: &'a str,
    /// DB-relative path. Caller decides the convention:
    /// `wiki/<slug>.md` for wiki, `raw/<hash[..2]>/<hash[2..]>` for raw.
    pub path: &'a str,
    pub title: &'a str,
    pub tags: &'a str,
    pub source: Option<&'a str>,
    pub mtime: &'a str,
    /// Body bytes (frontmatter already stripped). Hashed and fed to FTS verbatim.
    pub body: &'a str,
    /// Full file size on disk (frontmatter + body) — stored as `documents.size`.
    pub size: i64,
}

/// Outcome of `commit_doc`: the upsert result + the body hash for
/// callers that need to embed or report it.
pub struct CommitResult {
    pub outcome: CommitOutcome,
    pub body_hash: String,
}

/// Commit a doc row from a parsed body. Single source of truth for the
/// hash + chunk-cleanup invariants:
/// - hash: content_hash of `spec.body`
/// - size: `spec.size` (caller-provided full file size)
/// - body: `spec.body` fed to FTS as-is
/// - chunks: if the prior row's hash differed, the prior chunks are
///   deleted under the same transaction so vector search never returns
///   chunks for replaced content
///
/// Caller is responsible for the path convention (raw is
/// content-addressable, wiki is slug-named) and for committing the
/// transaction. Caller also handles the embedding step if applicable.
pub(crate) fn commit_doc(
    tx: &rusqlite::Connection,
    spec: &DocSpec<'_>,
) -> Result<CommitResult> {
    let body_hash = crate::storage::content_hash(spec.body.as_bytes());

    let prev_hash: Option<String> = tx
        .query_row(
            "SELECT hash FROM documents WHERE doc_type=?1 AND path=?2",
            rusqlite::params![spec.doc_type, spec.path],
            |r| r.get(0),
        )
        .ok();

    upsert_document_conn(
        tx,
        &UpsertDocument {
            doc_type: spec.doc_type,
            path: spec.path,
            title: spec.title,
            hash: &body_hash,
            tags: spec.tags,
            source: spec.source,
            body: spec.body,
            mtime: spec.mtime,
            size: spec.size,
        },
    )?;

    let outcome = match prev_hash.as_deref() {
        None => CommitOutcome::Inserted,
        Some(p) if p == body_hash => CommitOutcome::Unchanged,
        Some(p) => {
            // Only drop chunks for the old hash if no other live row
            // still references it. Two wiki pages with byte-identical
            // bodies share a hash; an UPSERT that changes one's body
            // would otherwise wipe the survivor's chunks, leaving its
            // documents row intact but invisible to vector search
            // (lint can't detect this — the row's `embed_model` stays
            // current, no `outdated_chunk_hashes` flag).
            if !hash_still_referenced(tx, p)? {
                crate::vector::delete_chunks(tx, p)?;
            }
            CommitOutcome::Updated
        }
    };
    Ok(CommitResult { outcome, body_hash })
}

/// Return true iff at least one `documents` row still has the given
/// hash. Use after an UPSERT/DELETE to decide whether the hash's
/// chunk rows can be safely dropped (no other doc references them).
fn hash_still_referenced(conn: &rusqlite::Connection, hash: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE hash = ?1",
            rusqlite::params![hash],
            |r| r.get(0),
        )
        .map_err(sqlite_err)?;
    Ok(count > 0)
}

/// Construct a `Document` from a SQLite row with the standard 6-column SELECT.
/// Expects columns: id, doc_type, path, title, hash, tags.
fn row_to_document(row: &rusqlite::Row) -> rusqlite::Result<Document> {
    Ok(Document {
        id: row.get(0)?,
        doc_type: row.get(1)?,
        path: row.get(2)?,
        title: row.get(3)?,
        hash: row.get(4)?,
        tags: row.get(5)?,
    })
}

fn document_id_by_path(conn: &rusqlite::Connection, doc_type: &str, path: &str) -> Result<i64> {
    conn.query_row(
        "SELECT id FROM documents WHERE doc_type = ?1 AND path = ?2",
        rusqlite::params![doc_type, path],
        |row| row.get(0),
    )
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => {
            MemexError::NotFound(format!("document not found: {doc_type}:{path}"))
        }
        other => sqlite_err(other),
    })
}

fn ensure_default_document_collection(conn: &rusqlite::Connection, document_id: i64) -> Result<()> {
    let has_membership: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM document_collections WHERE document_id = ?1)",
            rusqlite::params![document_id],
            |row| row.get(0),
        )
        .map_err(sqlite_err)?;
    if has_membership != 0 {
        return Ok(());
    }

    conn.execute(
        "INSERT OR IGNORE INTO collections (name) VALUES ('default')",
        [],
    )
    .map_err(sqlite_err)?;
    conn.execute(
        "INSERT INTO document_collections (document_id, collection_id) \
         SELECT ?1, id FROM collections WHERE name = 'default'",
        rusqlite::params![document_id],
    )
    .map_err(sqlite_err)?;
    Ok(())
}

fn set_document_collections_in_conn(
    conn: &rusqlite::Connection,
    doc_type: &str,
    path: &str,
    incoming: &[String],
) -> Result<()> {
    let document_id = document_id_by_path(conn, doc_type, path)?;
    let names = normalize_collections(incoming);

    conn.execute(
        "DELETE FROM document_collections WHERE document_id = ?1",
        rusqlite::params![document_id],
    )
    .map_err(sqlite_err)?;

    for name in names {
        conn.execute(
            "INSERT OR IGNORE INTO collections (name) VALUES (?1)",
            rusqlite::params![&name],
        )
        .map_err(sqlite_err)?;
        let collection_id: i64 = conn
            .query_row(
                "SELECT id FROM collections WHERE name = ?1",
                rusqlite::params![&name],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        conn.execute(
            "INSERT OR IGNORE INTO document_collections (document_id, collection_id) VALUES (?1, ?2)",
            rusqlite::params![document_id, collection_id],
        )
        .map_err(sqlite_err)?;
    }

    Ok(())
}

/// Three-tier identifier resolution.
///
/// 1. Hash prefix: `WHERE hash LIKE ?1 || '%'`
/// 2. Stem: match path basename without extension
/// 3. Title: `WHERE title = ?1 COLLATE NOCASE`
pub fn resolve_ref(conn: &rusqlite::Connection, reference: &str) -> Result<Vec<Document>> {
    // Tier 1: Hash prefix match
    let mut stmt = conn.prepare(
        "SELECT id, doc_type, path, title, hash, tags \
         FROM documents WHERE hash LIKE ?1 || '%'",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![reference], row_to_document)?
        .filter_map(|r| r.ok())
        .collect();
    if !docs.is_empty() {
        return Ok(docs);
    }

    // Tier 2: Stem match — path basename without extension
    // Build pattern like '%/reference.md' or 'reference.md'
    let stem_pattern = format!("%/{reference}.md");
    let exact_stem = format!("{reference}.md");
    let mut stmt = conn.prepare(
        "SELECT id, doc_type, path, title, hash, tags \
         FROM documents WHERE path LIKE ?1 OR path = ?2",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![stem_pattern, exact_stem], |row| {
            row_to_document(row)
        })?
        .filter_map(|r| r.ok())
        .collect();
    if !docs.is_empty() {
        return Ok(docs);
    }

    // Tier 3: Title match (case-insensitive)
    let mut stmt = conn.prepare(
        "SELECT id, doc_type, path, title, hash, tags \
         FROM documents WHERE title = ?1 COLLATE NOCASE",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![reference], row_to_document)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(docs)
}

// ---------------------------------------------------------------------------
// Bm25Search
// ---------------------------------------------------------------------------

/// One row of the `memex source list` output. Exposed as a public type so the
/// CLI can format it (or emit JSON via serde).
#[derive(Debug, serde::Serialize)]
pub struct SourceListRow {
    pub hash: String,
    pub path: String,
    pub title: String,
    pub size_bytes: usize,
    pub mtime: String,
    pub collections: Vec<String>,
}

/// BM25 full-text search backed by SQLite FTS5.
///
/// Uses the new content-addressable schema (content/documents/documents_fts)
/// from `schema::init_schema`. The inner `Connection` is wrapped in a `Mutex`
/// so that `Bm25Search` is `Send + Sync`.
pub struct Bm25Search {
    conn: Mutex<rusqlite::Connection>,
}

impl Bm25Search {
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

    /// Generate human-readable index from the DB.
    /// Format: `# Index\n\n- [Title](path)\n...`
    pub fn generate_index(&self) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT path, title FROM documents ORDER BY title")
            .map_err(sqlite_err)?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() {
            return Ok("# Index\n\nNo wiki pages yet.\n".to_string());
        }

        let mut output = String::from("# Index\n\n");
        for (path, title) in &rows {
            output.push_str(&format!("- [{title}]({path})\n"));
        }
        Ok(output)
    }

    /// Generate compact index for agent consumption.
    /// Format: `short_hash | title\n...`
    pub fn generate_compact_index(&self) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT hash, title FROM documents ORDER BY title")
            .map_err(sqlite_err)?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();

        let mut output = String::new();
        for (hash, title) in &rows {
            output.push_str(&format!("{} | {title}\n", crate::docid::short(hash)));
        }
        Ok(output)
    }

    /// Execute a single-column SELECT returning at most one String.
    fn query_single_string(&self, sql: &str, param: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn.prepare(sql).map_err(sqlite_err)?;
        let mut rows = stmt
            .query_map(rusqlite::params![param], |row| row.get::<_, String>(0))
            .map_err(sqlite_err)?;
        match rows.next() {
            Some(Ok(val)) => Ok(Some(val)),
            Some(Err(e)) => Err(sqlite_err(e)),
            None => Ok(None),
        }
    }

    /// Look up a page by filename stem.
    pub fn lookup_stem(&self, stem: &str) -> Result<Option<PathBuf>> {
        let wiki_path = format!("wiki/{stem}.md");
        self.query_single_string("SELECT path FROM documents WHERE path = ?1", &wiki_path)
            .map(|opt| opt.map(PathBuf::from))
    }

    /// Look up a page by title (case-insensitive).
    pub fn lookup_title(&self, title: &str) -> Result<Option<PathBuf>> {
        self.query_single_string(
            "SELECT path FROM documents WHERE title COLLATE NOCASE = ?1",
            title,
        )
        .map(|opt| opt.map(PathBuf::from))
    }

    /// Resolve a reference that could be a hash prefix, filename stem, or title.
    pub fn resolve_ref(&self, reference: &str) -> Result<Option<PathBuf>> {
        // Tier 1: hash prefix
        let path: Option<String> = {
            let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
            let mut stmt = conn
                .prepare("SELECT path FROM documents WHERE hash LIKE ?1 || '%' LIMIT 1")
                .map_err(sqlite_err)?;
            stmt.query_map(rusqlite::params![reference], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_err)?
            .next()
            .transpose()
            .map_err(sqlite_err)?
        };
        if let Some(p) = path {
            return Ok(Some(PathBuf::from(p)));
        }
        if let Some(path) = self.lookup_stem(reference)? {
            return Ok(Some(path));
        }
        self.lookup_title(reference)
    }

    /// Get total page count.
    pub fn page_count(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
            .map_err(sqlite_err)?;
        Ok(count as usize)
    }

    /// Look up the path for the document with the given full hash.
    pub fn path_by_hash(&self, hash: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let row = conn
            .query_row(
                "SELECT path FROM documents WHERE hash = ?1 LIMIT 1",
                rusqlite::params![hash],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sqlite_err)?;
        Ok(row)
    }

    /// Get document mtime + size for a (doc_type, path) pair.
    pub fn get_document_meta(
        &self,
        doc_type: &str,
        path: &str,
    ) -> Result<Option<DocumentMeta>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let row = conn
            .query_row(
                "SELECT mtime, size FROM documents WHERE doc_type=?1 AND path=?2",
                rusqlite::params![doc_type, path],
                |row| {
                    Ok(DocumentMeta {
                        mtime: row.get::<_, String>(0)?,
                        size: row.get::<_, i64>(1)?,
                    })
                },
            )
            .optional()
            .map_err(sqlite_err)?;
        Ok(row)
    }

    /// Batched mtime+size lookup for many (doc_type, path) pairs in one
    /// connection lock. Replaces the N round-trips per query that the
    /// BM25 stale-filter used to make. Pairs are grouped by doc_type so
    /// each group fits in a single `IN (...)` clause; chunked at 500 to
    /// stay under SQLite's `SQLITE_MAX_VARIABLE_NUMBER` (default 999,
    /// 32766 in newer builds), matching the convention used by
    /// `lookup_documents_with_collections`.
    pub fn get_documents_meta_by_paths(
        &self,
        pairs: &[(String, String)],
    ) -> Result<std::collections::HashMap<(String, String), DocumentMeta>> {
        let mut out: std::collections::HashMap<(String, String), DocumentMeta> =
            std::collections::HashMap::with_capacity(pairs.len());
        if pairs.is_empty() {
            return Ok(out);
        }
        let mut by_type: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for (dt, p) in pairs {
            by_type.entry(dt.as_str()).or_default().push(p.as_str());
        }
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        const CHUNK: usize = 500;
        for (doc_type, paths) in by_type {
            for chunk in paths.chunks(CHUNK) {
                let placeholders = std::iter::repeat_n("?", chunk.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT path, mtime, size FROM documents \
                     WHERE doc_type=?1 AND path IN ({placeholders})"
                );
                let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
                params.push(&doc_type);
                for p in chunk {
                    params.push(p as &dyn rusqlite::ToSql);
                }
                let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
                let rows = stmt
                    .query_map(params.as_slice(), |row| {
                        let path: String = row.get(0)?;
                        let mtime: String = row.get(1)?;
                        let size: i64 = row.get(2)?;
                        Ok((path, mtime, size))
                    })
                    .map_err(sqlite_err)?;
                for row in rows.flatten() {
                    let (path, mtime, size) = row;
                    out.insert(
                        (doc_type.to_string(), path),
                        DocumentMeta { mtime, size },
                    );
                }
            }
        }
        Ok(out)
    }

    /// Return wiki page slugs whose frontmatter `sources:` field contains
    /// `source_path`. Reads bodies from disk under `root` (filesystem-canonical).
    pub fn wiki_pages_referencing_source(
        &self,
        root: &Path,
        source_path: &str,
    ) -> Result<Vec<String>> {
        let rows: Vec<String> = {
            let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
            let mut stmt = conn
                .prepare("SELECT path FROM documents WHERE doc_type = 'wiki'")
                .map_err(sqlite_err)?;
            stmt.query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite_err)?
                .filter_map(|r| r.ok())
                .collect()
        };
        let mut out = Vec::new();
        for path in rows {
            let abs = root.join(&path);
            let Ok(body) = std::fs::read_to_string(&abs) else {
                continue;
            };
            if frontmatter_lists_source(&body, source_path) {
                let slug = std::path::Path::new(&path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&path)
                    .to_string();
                out.push(slug);
            }
        }
        Ok(out)
    }

    /// Get the collection names attached to a document by doc_type and path.
    pub fn document_collections_by_path(&self, doc_type: &str, path: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let document_id = document_id_by_path(&conn, doc_type, path)?;
        let mut stmt = conn
            .prepare(
                "SELECT c.name \
                 FROM document_collections dc \
                 JOIN collections c ON c.id = dc.collection_id \
                 WHERE dc.document_id = ?1 \
                 ORDER BY c.name",
            )
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map(rusqlite::params![document_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_err)?;
        let mut names = Vec::new();
        for row in rows {
            names.push(row.map_err(sqlite_err)?);
        }
        if names.is_empty() {
            Ok(vec!["default".to_string()])
        } else {
            Ok(names)
        }
    }

    /// Replace all collection memberships for a document.
    pub fn set_document_collections_by_path(
        &self,
        doc_type: &str,
        path: &str,
        names: &[String],
    ) -> Result<()> {
        let names = normalize_collections(names);
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        let document_id = document_id_by_path(&tx, doc_type, path)?;

        tx.execute(
            "DELETE FROM document_collections WHERE document_id = ?1",
            rusqlite::params![document_id],
        )
        .map_err(sqlite_err)?;

        for name in names {
            tx.execute(
                "INSERT OR IGNORE INTO collections (name) VALUES (?1)",
                rusqlite::params![&name],
            )
            .map_err(sqlite_err)?;
            let collection_id: i64 = tx
                .query_row(
                    "SELECT id FROM collections WHERE name = ?1",
                    rusqlite::params![&name],
                    |row| row.get(0),
                )
                .map_err(sqlite_err)?;
            tx.execute(
                "INSERT OR IGNORE INTO document_collections (document_id, collection_id) VALUES (?1, ?2)",
                rusqlite::params![document_id, collection_id],
            )
            .map_err(sqlite_err)?;
        }

        tx.commit().map_err(sqlite_err)?;
        Ok(())
    }

    /// Merge incoming collection names with the current memberships.
    pub fn union_document_collections_by_path(
        &self,
        doc_type: &str,
        path: &str,
        incoming: &[String],
    ) -> Result<()> {
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        let document_id = document_id_by_path(&tx, doc_type, path)?;

        let mut current = {
            let mut stmt = tx
                .prepare(
                    "SELECT c.name \
                     FROM document_collections dc \
                     JOIN collections c ON c.id = dc.collection_id \
                     WHERE dc.document_id = ?1 \
                     ORDER BY c.name",
                )
                .map_err(sqlite_err)?;
            let rows = stmt
                .query_map(rusqlite::params![document_id], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(sqlite_err)?;
            let mut names = Vec::new();
            for row in rows {
                names.push(row.map_err(sqlite_err)?);
            }
            if names.is_empty() {
                vec!["default".to_string()]
            } else {
                names
            }
        };

        current.extend(incoming.iter().cloned());
        let names = normalize_collections(&current);

        tx.execute(
            "DELETE FROM document_collections WHERE document_id = ?1",
            rusqlite::params![document_id],
        )
        .map_err(sqlite_err)?;

        for name in names {
            tx.execute(
                "INSERT OR IGNORE INTO collections (name) VALUES (?1)",
                rusqlite::params![&name],
            )
            .map_err(sqlite_err)?;
            let collection_id: i64 = tx
                .query_row(
                    "SELECT id FROM collections WHERE name = ?1",
                    rusqlite::params![&name],
                    |row| row.get(0),
                )
                .map_err(sqlite_err)?;
            tx.execute(
                "INSERT OR IGNORE INTO document_collections (document_id, collection_id) VALUES (?1, ?2)",
                rusqlite::params![document_id, collection_id],
            )
            .map_err(sqlite_err)?;
        }

        tx.commit().map_err(sqlite_err)?;
        Ok(())
    }

    /// Get the last-modified timestamp (`mtime`) for a page by its path.
    pub fn get_last_modified(&self, path: &Path) -> Result<Option<String>> {
        let path_str = path.to_string_lossy();
        self.query_single_string("SELECT mtime FROM documents WHERE path = ?1", &path_str)
            .map(|opt| opt.filter(|s| !s.is_empty()))
    }

    /// Get all page stems and titles (for cross-linking).
    pub fn all_stems_and_titles(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT path, title FROM documents ORDER BY title")
            .map_err(sqlite_err)?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows
            .into_iter()
            .map(|(path, title)| {
                // Extract stem from path like "wiki/my-page.md" -> "my-page"
                let stem = Path::new(&path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                (stem, title)
            })
            .collect())
    }

    /// Insert or update a document row in the documents table and refresh
    /// the matching `documents_fts` row. Wraps `upsert_document_conn` with
    /// mutex-guarded connection access.
    pub fn upsert_document(&self, doc: &UpsertDocument) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        upsert_document_conn(&conn, doc)
    }

    /// Store a raw document and its derived wiki pages in a single
    /// transaction. Returns (raw_content_hash, wiki_slugs+hashes). The
    /// caller is responsible for writing the actual wiki + raw bytes to
    /// disk; this method only updates the index.
    ///
    /// The raw `documents` row is keyed by the hash-addressed relative path
    /// `raw/<H[..2]>/<H[2..]>` so it matches the on-disk artifact layout.
    /// The original `source_path` (URL, file path, etc.) is preserved in the
    /// `source` column.
    pub fn store_ingest_batch(
        &self,
        raw_file: &str,
        source_path: &str,
        source_title: &str,
        wiki_pages: &[IngestWikiPage],
        collections: &[String],
        now: &str,
    ) -> Result<IngestBatchResult> {
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let tx = conn.transaction().map_err(sqlite_err)?;

        let result = (|| -> Result<IngestBatchResult> {
            // Raw doc: path is content-addressable. Split out the body
            // once here so we can derive both the hash-as-path and the
            // commit_doc body without re-parsing.
            let (_, raw_body) = crate::storage::split_frontmatter(raw_file).ok_or_else(|| {
                crate::error::MemexError::Other(anyhow::anyhow!(
                    "store_ingest_batch raw_file has no frontmatter"
                ))
            })?;
            let source_hash = crate::storage::content_hash(raw_body.as_bytes());
            let raw_rel = format!("raw/{}/{}", &source_hash[..2], &source_hash[2..]);
            commit_doc(
                &tx,
                &DocSpec {
                    doc_type: "raw",
                    path: &raw_rel,
                    title: source_title,
                    tags: "",
                    source: Some(source_path),
                    mtime: now,
                    body: raw_body,
                    size: raw_file.len() as i64,
                },
            )?;
            set_document_collections_in_conn(&tx, "raw", &raw_rel, collections)?;

            // Wiki pages: path is slug-named. Caller has already validated
            // each page.content has wiki frontmatter (otherwise the worker
            // would have failed before producing it).
            let mut wiki_hashes = Vec::with_capacity(wiki_pages.len());
            for page in wiki_pages {
                let rel_path = format!("wiki/{}.md", page.slug);
                let (_, body) = crate::storage::split_frontmatter(&page.content).ok_or_else(|| {
                    crate::error::MemexError::Other(anyhow::anyhow!(
                        "store_ingest_batch wiki page {} has no frontmatter",
                        page.slug
                    ))
                })?;
                let res = commit_doc(
                    &tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: &rel_path,
                        title: &page.title,
                        tags: &page.tags,
                        source: None,
                        mtime: now,
                        body,
                        size: page.content.len() as i64,
                    },
                )?;
                set_document_collections_in_conn(&tx, "wiki", &rel_path, collections)?;
                wiki_hashes.push((page.slug.clone(), res.body_hash));
            }

            Ok(IngestBatchResult {
                source_hash,
                wiki_hashes,
            })
        })();

        match &result {
            Ok(_) => {
                tx.commit().map_err(sqlite_err)?;
            }
            Err(_) => {
                let _ = tx.rollback();
            }
        }
        result
    }

    /// Get the content hash for a document by its path.
    pub fn get_document_hash(&self, path: &str) -> Result<Option<String>> {
        self.query_single_string("SELECT hash FROM documents WHERE path = ?1", path)
    }

    /// Three-tier identifier resolution returning full `Document` records.
    ///
    /// Tries docid prefix, then stem, then title (case-insensitive).
    pub fn resolve_ref_documents(&self, reference: &str) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        resolve_ref(&conn, reference)
    }

    /// Get wiki page count (documents with doc_type = 'wiki' only).
    pub fn wiki_page_count(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE doc_type = 'wiki'",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        Ok(count as usize)
    }

    // Ingest-job lifecycle + retention live in `search/ingest_jobs.rs`.

    pub fn source_count(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE doc_type = 'raw'",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        Ok(count as usize)
    }

    /// List `raw` documents. When `collection_filter` is non-empty, only
    /// documents belonging to one of the named collections are returned.
    /// Sorted by `mtime DESC`.
    pub fn list_sources(&self, collection_filter: &[String]) -> Result<Vec<SourceListRow>> {
        // d.path is the canonical "raw/<hh>/<rest>" path; d.source is the
        // user-facing source identifier (URL/filename/label). The CLI
        // displays the latter, but the collection lookup needs the
        // former — `document_collections_by_path` queries `documents`
        // keyed by (doc_type, path).
        let sql = "SELECT d.hash, COALESCE(d.source, d.path), d.title, d.size, d.mtime, d.path \
                   FROM documents d \
                   WHERE d.doc_type = 'raw' \
                   ORDER BY d.mtime DESC";
        let rows: Vec<(SourceListRow, String)> = {
            let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
            let mut stmt = conn.prepare(sql).map_err(sqlite_err)?;
            let iter = stmt
                .query_map([], |row| {
                    Ok((
                        SourceListRow {
                            hash: row.get(0)?,
                            path: row.get(1)?,
                            title: row.get(2)?,
                            size_bytes: row.get::<_, i64>(3)? as usize,
                            mtime: row.get(4)?,
                            collections: Vec::new(),
                        },
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(sqlite_err)?;
            let mut out: Vec<(SourceListRow, String)> = Vec::new();
            for r in iter {
                out.push(r.map_err(sqlite_err)?);
            }
            out
        };
        // Drop the lock before `document_collections_by_path`, which re-locks.
        let mut filtered = Vec::with_capacity(rows.len());
        for (mut row, canonical_path) in rows {
            row.collections = self
                .document_collections_by_path("raw", &canonical_path)
                .unwrap_or_default();
            if collection_filter.is_empty()
                || row
                    .collections
                    .iter()
                    .any(|c| collection_filter.iter().any(|f| f == c))
            {
                filtered.push(row);
            }
        }
        Ok(filtered)
    }

    /// Return all wiki documents (doc_type = 'wiki').
    pub fn all_wiki_documents(&self) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, doc_type, path, title, hash, tags \
                 FROM documents WHERE doc_type = 'wiki'",
            )
            .map_err(sqlite_err)?;
        let docs: Vec<Document> = stmt
            .query_map([], row_to_document)
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(docs)
    }

    /// Return all raw documents (doc_type = 'raw').
    pub fn all_raw_documents(&self) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, doc_type, path, title, hash, tags \
                 FROM documents WHERE doc_type = 'raw'",
            )
            .map_err(sqlite_err)?;
        let docs: Vec<Document> = stmt
            .query_map([], row_to_document)
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(docs)
    }

    /// Delete a document by path. The matching `documents_fts` row is
    /// removed via FTS5 'delete' command; chunks are cleaned via
    /// `vector::delete_chunks` only if no other document still
    /// references the same hash (two wiki pages with byte-identical
    /// bodies share chunks).
    pub fn delete_document_with_cleanup(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        // Fetch (id, hash) before deletion so we can clean FTS + vec rows.
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, hash FROM documents WHERE path = ?1",
                rusqlite::params![path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        if let Some((id, hash)) = &row {
            // Best-effort: remove FTS row keyed by id. Logged on
            // failure so stale tokens don't accumulate silently —
            // recovery is `memex lint --fix` which reindexes.
            if let Err(e) = conn.execute(
                "INSERT INTO documents_fts(documents_fts, rowid, path, title, tags, body) VALUES('delete', ?1, '', '', '', '')",
                rusqlite::params![id],
            ) {
                tracing::warn!(
                    document_id = id,
                    path = %path,
                    error = %e,
                    "FTS row delete failed (run `memex lint --fix` to clean stale tokens)"
                );
            }
            // Drop chunks only when no other doc references this hash,
            // otherwise we'd wipe a sibling row's vectors and leave it
            // invisible to vector search with no lint signal to recover.
            // Count BEFORE the documents row is deleted; the row at
            // `path` must be excluded from the survivor count via id.
            let other_refs: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM documents WHERE hash = ?1 AND id != ?2",
                    rusqlite::params![hash, id],
                    |r| r.get(0),
                )
                .map_err(sqlite_err)?;
            if other_refs == 0 {
                crate::vector::delete_chunks(&conn, hash)?;
            }
        }
        conn.execute(
            "DELETE FROM documents WHERE path = ?1",
            rusqlite::params![path],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    /// Return distinct `(model, count)` pairs for documents whose
    /// `embed_model` differs from `current_model`. Used by lint to detect
    /// stale embeddings after a model upgrade.
    pub fn outdated_chunk_models(&self, current_model: &str) -> Result<Vec<(String, usize)>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT embed_model, COUNT(*) FROM documents \
                 WHERE embed_model IS NOT NULL AND embed_model != ?1 GROUP BY embed_model",
            )
            .map_err(sqlite_err)?;
        let rows: Vec<(String, usize)> = stmt
            .query_map(rusqlite::params![current_model], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Return distinct hashes whose document-level `embed_model` is set but
    /// differs from `current_model`. Used by lint --fix to identify
    /// documents needing re-embedding.
    pub fn outdated_chunk_hashes(&self, current_model: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT hash FROM documents \
                 WHERE embed_model IS NOT NULL AND embed_model != ?1",
            )
            .map_err(sqlite_err)?;
        let rows: Vec<String> = stmt
            .query_map(rusqlite::params![current_model], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// BM25 search filtered by doc_type.
    ///
    /// Wraps the free function `search_bm25()` with mutex-guarded connection access.
    pub fn search_by_doc_type(
        &self,
        query: &str,
        doc_type: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        search_bm25(&conn, query, doc_type, limit)
    }

    /// BM25 search filtered by doc_type and collection membership.
    pub fn search_by_doc_type_in_collections(
        &self,
        query: &str,
        doc_type: &str,
        limit: usize,
        collections: &[String],
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        bm25::search_bm25_in_collections(&conn, query, doc_type, limit, collections)
    }

    /// BM25 search restricted to the title column. Transforms the query to
    /// use FTS5 `title:` column prefix, then delegates to `search_bm25`.
    pub fn search_title_only(
        &self,
        query: &str,
        doc_type: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let sanitized = sanitize_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        // Prefix each AND term with "title:" for column-specific FTS5 matching.
        let title_query = sanitized
            .split(" AND ")
            .map(|term| format!("title: {term}"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        bm25::search_bm25_raw(&conn, &title_query, doc_type, limit)
    }

    /// Look up document metadata by content hash.
    ///
    /// Returns all documents that reference the given content hash.
    /// Used by vector search to convert chunk-level results (keyed by hash)
    /// into full `SearchResult` records with doc_type, path, etc.
    pub fn lookup_documents_by_hash(&self, hash: &str) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, doc_type, path, title, hash, tags \
                 FROM documents WHERE hash = ?1",
            )
            .map_err(sqlite_err)?;
        let docs: Vec<Document> = stmt
            .query_map(rusqlite::params![hash], row_to_document)
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(docs)
    }

    /// Atomic batched lookup that returns each document's row plus its
    /// collection memberships in a single connection-lock scope. Replaces
    /// the two-step `lookup_documents_by_hashes` + `collections_by_document_ids`
    /// flow which had a delete-between-queries race: step 1 could return a
    /// doc whose row was concurrently deleted before step 2, after which
    /// the membership-fallback logic would resurrect it as "default". One
    /// query, one snapshot, no race.
    ///
    /// Returns a `DocLookup` — `(hash → docs, doc_id → collections)`.
    /// Documents with no `document_collections` rows map to `["default"]`,
    /// mirroring `document_collections_by_path` semantics.
    /// `id_chunk_size` caps the SQL `IN (...)` width per query so a popular
    /// hash that fans out to thousands of doc rows doesn't trip SQLite's
    /// `SQLITE_MAX_VARIABLE_NUMBER` limit (default 999, 32766 in newer
    /// builds).
    pub fn lookup_documents_with_collections(
        &self,
        hashes: &[String],
    ) -> Result<DocLookup> {
        if hashes.is_empty() {
            return Ok((Default::default(), Default::default(), Default::default()));
        }
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;

        // Step 1: docs by hash + meta by id, chunked to stay under SQLite's
        // variable cap. mtime/size live alongside the existing columns so
        // retrieval gets the stat-check inputs in the same query.
        const CHUNK: usize = 500;
        let mut docs_acc: std::collections::HashMap<String, Vec<Document>> =
            std::collections::HashMap::new();
        let mut meta_acc: std::collections::HashMap<i64, (String, i64)> =
            std::collections::HashMap::new();
        for hash_chunk in hashes.chunks(CHUNK) {
            let placeholders = std::iter::repeat_n("?", hash_chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id, doc_type, path, title, hash, tags, mtime, size \
                 FROM documents WHERE hash IN ({placeholders})"
            );
            let params: Vec<&dyn rusqlite::ToSql> = hash_chunk
                .iter()
                .map(|h| h as &dyn rusqlite::ToSql)
                .collect();
            let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
            let rows = stmt
                .query_map(params.as_slice(), |row| {
                    let doc = row_to_document(row)?;
                    let mtime: String = row.get(6)?;
                    let size: i64 = row.get(7)?;
                    Ok((doc, mtime, size))
                })
                .map_err(sqlite_err)?;
            for row in rows.flatten() {
                let (d, mtime, size) = row;
                meta_acc.insert(d.id, (mtime, size));
                docs_acc.entry(d.hash.clone()).or_default().push(d);
            }
        }
        let docs: std::collections::HashMap<String, Vec<Document>> = docs_acc;

        // Step 2: memberships, also chunked. Same connection lock means
        // both queries see the same WAL snapshot — no delete race.
        let mut mem_acc: std::collections::HashMap<i64, Vec<String>> =
            std::collections::HashMap::new();
        let all_ids: Vec<i64> = docs.values().flat_map(|v| v.iter().map(|d| d.id)).collect();
        for id_chunk in all_ids.chunks(CHUNK) {
            if id_chunk.is_empty() {
                continue;
            }
            let placeholders = std::iter::repeat_n("?", id_chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT dc.document_id, c.name \
                 FROM document_collections dc \
                 JOIN collections c ON c.id = dc.collection_id \
                 WHERE dc.document_id IN ({placeholders}) \
                 ORDER BY dc.document_id, c.name"
            );
            let params: Vec<&dyn rusqlite::ToSql> =
                id_chunk.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
            let mut stmt = conn.prepare(&sql).map_err(sqlite_err)?;
            let rows = stmt
                .query_map(params.as_slice(), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sqlite_err)?;
            for row in rows {
                let (id, name) = row.map_err(sqlite_err)?;
                mem_acc.entry(id).or_default().push(name);
            }
        }
        // Force-fill "default" only for docs we still saw in step 1's
        // snapshot. A deleted doc won't appear in `all_ids` because step
        // 1 didn't return it under the same connection-lock.
        for &id in &all_ids {
            mem_acc.entry(id).or_insert_with(|| vec!["default".to_string()]);
        }
        let memberships: std::collections::HashMap<i64, Vec<String>> = mem_acc;
        Ok((docs, memberships, meta_acc))
    }

    /// Test-only: remove a documents row + FTS row by path. Does NOT clean
    /// chunks — production deletes use `delete_document_with_cleanup`.
    #[cfg(test)]
    pub fn remove_page(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        // Remove FTS row before the documents row to keep the index in sync.
        if let Ok(id) = conn.query_row(
            "SELECT id FROM documents WHERE path = ?1",
            rusqlite::params![path],
            |row| row.get::<_, i64>(0),
        ) {
            let _ = conn.execute(
                "INSERT INTO documents_fts(documents_fts, rowid, path, title, tags, body) VALUES('delete', ?1, '', '', '', '')",
                rusqlite::params![id],
            );
        }
        conn.execute(
            "DELETE FROM documents WHERE path = ?1",
            rusqlite::params![path],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    pub fn rebuild(&self, root: &Path) -> Result<()> {
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        conn.execute("DELETE FROM documents", [])
            .map_err(sqlite_err)?;
        // Truncate the FTS index too so stale rows don't survive a rebuild.
        let _ = conn.execute(
            "INSERT INTO documents_fts(documents_fts) VALUES('delete-all')",
            [],
        );

        let wiki_dir = root.join("wiki");
        let tx = conn.transaction().map_err(sqlite_err)?;
        if !wiki_dir.is_dir() {
            // Wiki dir is gone but DELETE FROM documents already ran;
            // every chunk is orphaned. Sweep them through the same
            // orphan-cleanup path used at the end of a populated
            // rebuild so a wiki-less rebuild doesn't leak vectors
            // forever.
            let orphan_hashes: Vec<String> = {
                let mut stmt = tx
                    .prepare("SELECT DISTINCT hash FROM chunks")
                    .map_err(sqlite_err)?;
                stmt.query_map([], |row| row.get::<_, String>(0))
                    .map_err(sqlite_err)?
                    .filter_map(|r| r.ok())
                    .collect()
            };
            for hash in &orphan_hashes {
                crate::vector::delete_chunks(&tx, hash)?;
            }
            tx.commit().map_err(sqlite_err)?;
            return Ok(());
        }

        for entry in WalkDir::new(&wiki_dir)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        {
            let abs_path = entry.path();
            let content = match std::fs::read_to_string(abs_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Some((title, body, tags, _summary, collections)) =
                parse_page_for_indexing(&content)
            {
                let rel_path = abs_path.strip_prefix(root).unwrap_or(abs_path);
                let path_str = rel_path.to_string_lossy().to_string();
                let mtime = crate::storage::file_mtime_iso(abs_path);
                // Route through commit_doc so rebuild's hash + body + size
                // convention is identical to index_wiki_file's. Without this,
                // rebuild stored content_hash(full_content) while writers
                // stored content_hash(body), and lint flagged every page stale.
                commit_doc(
                    &tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: &path_str,
                        title: &title,
                        tags: &tags,
                        source: None,
                        mtime: &mtime,
                        body: &body,
                        size: content.len() as i64,
                    },
                )?;
                set_document_collections_in_conn(&tx, "wiki", &path_str, &collections)?;
            }
        }
        // Drop chunks whose document is gone after rebuild (file deleted from
        // disk between runs). Same hash on both sides means the chunks belong
        // to a doc that's still present, so they survive — no needless
        // re-embedding for unchanged content.
        let orphan_hashes: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT DISTINCT hash FROM chunks \
                     WHERE hash NOT IN (SELECT hash FROM documents)",
                )
                .map_err(sqlite_err)?;
            stmt.query_map([], |row| row.get::<_, String>(0))
                .map_err(sqlite_err)?
                .filter_map(|r| r.ok())
                .collect()
        };
        for hash in &orphan_hashes {
            crate::vector::delete_chunks(&tx, hash)?;
        }
        tx.commit().map_err(sqlite_err)?;

        Ok(())
    }
}

/// Test-only helpers. Kept in their own impl block so production reviewers
/// can see at a glance what's not part of the runtime API.
#[cfg(any(test, feature = "test-utils"))]
impl Bm25Search {
    pub fn conn_for_test(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().expect("lock poisoned in test")
    }

    /// Insert a document row with the body-only hash convention used by
    /// `commit_doc`. No production callers — used by 17 in-module tests
    /// to seed search/collections fixtures.
    pub fn index_page(
        &self,
        path: &Path,
        title: &str,
        body: &str,
        tags: &str,
        last_modified: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let path_str = path.to_string_lossy().to_string();
        let doc_type = if path_str.starts_with("raw/") {
            "raw"
        } else {
            "wiki"
        };
        let hash = crate::storage::content_hash(body.as_bytes());
        upsert_document_conn(
            &conn,
            &UpsertDocument {
                doc_type,
                path: &path_str,
                title,
                hash: &hash,
                tags,
                source: None,
                body,
                mtime: last_modified,
                size: body.len() as i64,
            },
        )
    }
}

/// Parse a wiki page's content into (title, body, tags, summary, collections) for indexing.
///
/// Uses `crate::validate::parse_frontmatter` to extract frontmatter fields.
/// Returns `None` if the page has no valid frontmatter.
/// Summary is taken from the frontmatter `summary` field if present, otherwise
/// extracted from the first non-empty body line (truncated to 120 chars).
pub fn parse_page_for_indexing(
    content: &str,
) -> Option<(String, String, String, String, Vec<String>)> {
    let (fm, body) = crate::validate::parse_frontmatter(content).ok()?;
    if fm.title.trim().is_empty() {
        return None;
    }
    let tags = fm.tags.join(", ");
    let summary = fm
        .summary
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::index::extract_summary(&body, 120));
    let collections = normalize_collections(&fm.collections);
    Some((fm.title, body, tags, summary, collections))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------


/// Best-effort check: does the YAML frontmatter at the head of `body` list
/// `source_path` under `sources:`? Tolerant of pages missing `created_at`
/// or `updated_at` (the strict `PageFrontmatter` schema rejects those, but
/// we don't care about those fields for this query). Returns false on any
/// parse failure or missing frontmatter.
fn frontmatter_lists_source(body: &str, source_path: &str) -> bool {
    let trimmed = body.trim_start();
    let after = match trimmed.strip_prefix("---\n").or_else(|| trimmed.strip_prefix("---")) {
        Some(s) => s,
        None => return false,
    };
    let close = match after.find("\n---") {
        Some(i) => i,
        None => return false,
    };
    let yaml_block = &after[..close];
    // Try generic YAML parse to find a `sources:` array.
    let parsed: serde_yaml::Value = match serde_yaml::from_str(yaml_block) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let sources = match parsed.get("sources").and_then(|v| v.as_sequence()) {
        Some(s) => s,
        None => return false,
    };
    sources
        .iter()
        .any(|v| v.as_str().map(|s| s == source_path).unwrap_or(false))
}

/// Convert a rusqlite error into a MemexError::Io.
fn sqlite_err(e: rusqlite::Error) -> MemexError {
    MemexError::Io(std::io::Error::other(e.to_string()))
}

/// Convert a mutex poison error into a MemexError::Io.
fn mutex_err<T>(e: &std::sync::PoisonError<T>) -> MemexError {
    MemexError::Io(std::io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: create a Bm25Search backed by a temp file.
    fn open_temp_search() -> (TempDir, Bm25Search) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join(crate::INDEX_DB_NAME);
        let search = Bm25Search::open(&db_path).unwrap();
        (dir, search)
    }

    /// Helper: a valid wiki page with the given title and body.
    fn make_page(title: &str, body: &str, tags: &[&str]) -> String {
        // Empty list needs `tags: []` (with the space) — `tags:[]` is invalid
        // YAML and serde_yaml refuses to deserialize it as Vec<String>.
        let tag_yaml = if tags.is_empty() {
            " []".to_string()
        } else {
            let items: Vec<String> = tags.iter().map(|t| format!("\n  - {t}")).collect();
            items.join("")
        };
        format!(
            "---\ntitle: {title}\ntags:{tag_yaml}\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\n{body}\n"
        )
    }

    /// Helper: set up an in-memory DB with the new schema.
    fn setup_db() -> rusqlite::Connection {
        crate::schema::register_sqlite_vec_once();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    /// Helper: insert a document into the new schema via upsert_document_conn.
    fn insert_doc(
        conn: &rusqlite::Connection,
        doc_type: &str,
        path: &str,
        title: &str,
        body: &str,
        tags: &str,
    ) {
        let doc = format!("---\ntitle: {title}\ntags: {tags}\n---\n\n{body}\n");
        let hash = crate::storage::content_hash(doc.as_bytes());
        upsert_document_conn(
            conn,
            &UpsertDocument {
                doc_type,
                path,
                title,
                hash: &hash,
                tags,
                source: None,
                body,
                mtime: "2026-04-06T00:00:00Z",
                size: doc.len() as i64,
            },
        )
        .unwrap();
    }

    // -----------------------------------------------------------------------
    // Score normalization / signal detection
    // -----------------------------------------------------------------------

    #[test]
    fn score_normalization() {
        let s = normalize_bm25(-10.0);
        assert!((s - 10.0 / 11.0).abs() < 1e-9, "got {s}");

        let s = normalize_bm25(-2.0);
        assert!((s - 2.0 / 3.0).abs() < 1e-9, "got {s}");

        let s = normalize_bm25(-0.5);
        assert!((s - 1.0 / 3.0).abs() < 1e-9, "got {s}");

        assert_eq!(normalize_bm25(0.0), 0.0);

        let s = normalize_bm25(10.0);
        assert!((s - 10.0 / 11.0).abs() < 1e-9);
    }

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
        // Even on textbook strong scores, an explicit intent forces weak so
        // the expansion+rerank pipeline runs.
        assert!(!is_strong_signal(0.91, 0.0, Some("user intent here")));
        assert!(!is_strong_signal(0.99, 0.10, Some("more context")));
    }

    // -----------------------------------------------------------------------
    // BM25 search filters
    // -----------------------------------------------------------------------

    #[test]
    fn search_filters_by_doc_type() {
        let conn = setup_db();

        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "The borrow checker enforces ownership rules at compile time.",
            "concept, rust",
        );

        insert_doc(
            &conn,
            "raw",
            "raw/ab/cdrust-book.md",
            "Rust Programming Language",
            "The Rust programming language borrow checker is described in this book.",
            "reference",
        );

        let wiki_results = search_bm25(&conn, "borrow checker rust", "wiki", 10).unwrap();
        assert!(!wiki_results.is_empty());
        for r in &wiki_results {
            assert_eq!(r.doc_type, "wiki");
        }

        let raw_results = search_bm25(&conn, "borrow checker rust", "raw", 10).unwrap();
        assert!(!raw_results.is_empty());
        for r in &raw_results {
            assert_eq!(r.doc_type, "raw");
        }
    }

    #[test]
    fn search_by_doc_type_filters_by_collections() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/team-a.md"),
                "Alpha Team A",
                "Alpha content for the team-a collection.",
                "alpha",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .set_document_collections_by_path("wiki", "wiki/team-a.md", &["team-a".to_string()])
            .unwrap();

        search
            .index_page(
                Path::new("wiki/team-b.md"),
                "Alpha Team B",
                "Alpha content for the team-b collection.",
                "alpha",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .set_document_collections_by_path("wiki", "wiki/team-b.md", &["team-b".to_string()])
            .unwrap();

        let results = search
            .search_by_doc_type_in_collections(
                "alpha",
                "wiki",
                10,
                &["team-a".to_string()],
            )
            .unwrap();

        let paths: std::collections::HashSet<PathBuf> =
            results.into_iter().map(|r| r.path).collect();
        assert!(paths.contains(&PathBuf::from("wiki/team-a.md")));
        assert!(!paths.contains(&PathBuf::from("wiki/team-b.md")));
    }

    // -----------------------------------------------------------------------
    // Collection helpers
    // -----------------------------------------------------------------------

    #[test]
    fn document_collections_default_fallback_for_uninitialized_document() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/alpha.md"),
                "Alpha",
                "Body.",
                "",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        assert_eq!(
            search
                .document_collections_by_path("wiki", "wiki/alpha.md")
                .unwrap(),
            vec!["default".to_string()]
        );
    }

    #[test]
    fn document_collections_union_normalizes_and_keeps_default() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/alpha.md"),
                "Alpha",
                "Body.",
                "",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        search
            .union_document_collections_by_path(
                "wiki",
                "wiki/alpha.md",
                &["Team".to_string(), "DEFAULT".to_string(), "".to_string()],
            )
            .unwrap();
        assert_eq!(
            search
                .document_collections_by_path("wiki", "wiki/alpha.md")
                .unwrap(),
            vec!["default".to_string(), "team".to_string()]
        );

        assert_eq!(
            normalize_collections(&[
                "  Team ".to_string(),
                "default".to_string(),
                "".to_string(),
                "TEAM".to_string()
            ]),
            vec!["default".to_string(), "team".to_string()]
        );

        search
            .set_document_collections_by_path(
                "wiki",
                "wiki/alpha.md",
                &[
                    "Research".to_string(),
                    "team".to_string(),
                    "Team".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(
            search
                .document_collections_by_path("wiki", "wiki/alpha.md")
                .unwrap(),
            vec!["research".to_string(), "team".to_string()]
        );
    }

    #[test]
    fn document_collections_cascade_delete_membership_rows() {
        let conn = setup_db();

        // upsert_document_conn (used by insert_doc) already attaches the
        // 'default' membership, so we just verify cascade delete.
        insert_doc(&conn, "wiki", "wiki/alpha.md", "Alpha", "Body.", "");
        let pre: i64 = conn
            .query_row("SELECT COUNT(*) FROM document_collections", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(pre >= 1);

        conn.execute(
            "DELETE FROM documents WHERE doc_type = ?1 AND path = ?2",
            rusqlite::params!["wiki", "wiki/alpha.md"],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM document_collections", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    // -----------------------------------------------------------------------
    // sanitize_query
    // -----------------------------------------------------------------------

    #[test]
    fn sanitize_query_bare_words() {
        let result = sanitize_query("hello world");
        assert_eq!(result, "\"hello\"* AND \"world\"*");
    }

    #[test]
    fn sanitize_query_quoted_phrase() {
        let result = sanitize_query("\"exact match\"");
        assert_eq!(result, "\"exact match\"");
    }

    #[test]
    fn sanitize_query_negation() {
        let result = sanitize_query("rust -python");
        assert_eq!(result, "\"rust\"* NOT \"python\"");
    }

    #[test]
    fn sanitize_query_hyphenated() {
        let result = sanitize_query("multi-agent");
        assert_eq!(result, "\"multi agent\"");
    }

    #[test]
    fn sanitize_query_mixed() {
        let result = sanitize_query("\"exact match\" multi-agent -python");
        assert_eq!(result, "\"exact match\" AND \"multi agent\" NOT \"python\"");
    }

    #[test]
    fn sanitize_query_empty() {
        assert_eq!(sanitize_query(""), "");
        assert_eq!(sanitize_query("   "), "");
    }

    #[test]
    fn sanitize_query_special_chars() {
        let result = sanitize_query("hello (world)");
        assert_eq!(result, "\"hello\"* AND \"world\"*");
    }

    // -----------------------------------------------------------------------
    // resolve_ref tests (now hash-prefix instead of docid)
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_ref_by_hash_prefix() {
        let conn = setup_db();
        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "Body text.",
            "concept",
        );

        // Pull the actual hash so we can query by a 7-char prefix.
        let hash: String = conn
            .query_row(
                "SELECT hash FROM documents WHERE path = 'wiki/rust-borrow.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let docs = resolve_ref(&conn, &hash[..7]).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "wiki/rust-borrow.md");
    }

    #[test]
    fn resolve_ref_by_stem() {
        let conn = setup_db();
        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "Body text.",
            "concept",
        );

        let docs = resolve_ref(&conn, "rust-borrow").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "wiki/rust-borrow.md");
    }

    #[test]
    fn resolve_ref_by_title() {
        let conn = setup_db();
        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "Body text.",
            "concept",
        );

        let docs = resolve_ref(&conn, "Rust Borrow Checker").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "wiki/rust-borrow.md");
    }

    #[test]
    fn resolve_ref_title_case_insensitive() {
        let conn = setup_db();
        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "Body text.",
            "concept",
        );

        let docs = resolve_ref(&conn, "rust borrow checker").unwrap();
        assert_eq!(docs.len(), 1);
    }

    #[test]
    fn resolve_ref_not_found() {
        let conn = setup_db();
        let docs = resolve_ref(&conn, "nonexistent").unwrap();
        assert!(docs.is_empty());
    }

    // -----------------------------------------------------------------------
    // rrf_fuse
    // -----------------------------------------------------------------------

    #[test]
    fn rrf_fuse_single_list() {
        let list = vec![
            SearchResult {
                path: PathBuf::from("wiki/a.md"),
                title: "A".to_string(),
                score: 0.9,
                snippet: String::new(),
                doc_type: "wiki".to_string(),
                hash: String::new(),
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.5,
                snippet: String::new(),
                doc_type: "wiki".to_string(),
                hash: String::new(),
            },
        ];

        let fused = rrf_fuse(&[list], &[1.0], 60);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].score, 1.0);
        assert_eq!(fused[1].score, 0.5);
    }

    #[test]
    fn rrf_fuse_wiki_weighted() {
        let wiki_list = vec![SearchResult {
            path: PathBuf::from("wiki/a.md"),
            title: "A".to_string(),
            score: 0.9,
            snippet: String::new(),
            doc_type: "wiki".to_string(),
            hash: String::new(),
        }];
        let raw_list = vec![SearchResult {
            path: PathBuf::from("raw/b.md"),
            title: "B".to_string(),
            score: 0.8,
            snippet: String::new(),
            doc_type: "raw".to_string(),
            hash: String::new(),
        }];

        let fused = rrf_fuse(&[wiki_list, raw_list], &[2.0, 1.0], 60);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].path, PathBuf::from("wiki/a.md"));
    }

    // -----------------------------------------------------------------------
    // Bm25Search core flow
    // -----------------------------------------------------------------------

    #[test]
    fn bm25_index_and_search() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "The borrow checker enforces ownership rules at compile time.",
                "concept, rust",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/python-gc.md"),
                "Python Garbage Collection",
                "Python uses reference counting with a cyclic garbage collector.",
                "concept, python",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let results = search
            .search_by_doc_type("borrow checker rust", "wiki", 10)
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].path, PathBuf::from("wiki/rust-borrow.md"));
        for r in &results {
            assert!((0.0..=1.0).contains(&r.score), "score out of range: {}", r.score);
        }
    }

    #[test]
    fn bm25_remove_page() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/ephemeral.md"),
                "Ephemeral Page",
                "This page will be removed shortly.",
                "test",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let before = search.search_by_doc_type("ephemeral", "wiki", 10).unwrap();
        assert_eq!(before.len(), 1);

        search.remove_page("wiki/ephemeral.md").unwrap();

        let after = search.search_by_doc_type("ephemeral", "wiki", 10).unwrap();
        assert!(after.is_empty());
    }

    #[test]
    fn bm25_empty_search() {
        let (_dir, search) = open_temp_search();
        let results = search.search_by_doc_type("anything", "wiki", 10).unwrap();
        assert!(results.is_empty());
    }

    /// Regression: when `commit_doc` updates an existing row with a
    /// different body hash (e.g. `lint --fix` on a stale-index page,
    /// or any out-of-band edit reconciled by the watcher), the chunks
    /// linked to the OLD hash must be deleted under the same
    /// transaction. Without this, the chunks become orphans —
    /// invisible via the documents-JOIN (because documents.hash now
    /// differs) but still consuming disk and `chunks_vec` slots.
    #[test]
    fn commit_doc_drops_chunks_for_old_hash_on_body_change() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let wiki_dir = root.join("wiki");
        std::fs::create_dir_all(&wiki_dir).unwrap();

        let body_old = "Old body content.\n";
        let body_new = "New body content with extra paragraph.\n";
        let hash_old = crate::storage::content_hash(body_old.as_bytes());
        let hash_new = crate::storage::content_hash(body_new.as_bytes());
        assert_ne!(hash_old, hash_new);

        let db_path = root.join(crate::INDEX_DB_NAME);
        let search = Bm25Search::open(&db_path).unwrap();

        // Seed: pretend the page was previously committed and embedded
        // under hash_old.
        search
            .with_transaction(|tx| {
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page.md",
                        title: "Page",
                        tags: "",
                        source: None,
                        mtime: "2026-04-28T00:00:00Z",
                        body: body_old,
                        size: body_old.len() as i64,
                    },
                )
            })
            .unwrap();
        let conn = search.conn_for_test();
        crate::vector::store_chunk(
            &conn,
            &hash_old,
            0,
            0,
            5,
            &vec![0.1f32; crate::embed::EMBEDDING_DIM],
        )
        .unwrap();
        drop(conn);

        // Now commit the same path with the NEW body — simulating a
        // post-edit `lint --fix` or a watcher-driven re-index.
        let result = search
            .with_transaction(|tx| {
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page.md",
                        title: "Page",
                        tags: "",
                        source: None,
                        mtime: "2026-04-28T01:00:00Z",
                        body: body_new,
                        size: body_new.len() as i64,
                    },
                )
            })
            .unwrap();
        assert_eq!(result.body_hash, hash_new);
        assert!(matches!(result.outcome, CommitOutcome::Updated));

        // Old chunks must be gone; document row must point at the new hash.
        let conn = search.conn_for_test();
        let old_chunks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE hash = ?1",
                rusqlite::params![hash_old],
                |r| r.get(0),
            )
            .unwrap();
        let stored_hash: String = conn
            .query_row(
                "SELECT hash FROM documents WHERE path = 'wiki/page.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            old_chunks, 0,
            "chunks for the previous hash must be cleaned up"
        );
        assert_eq!(stored_hash, hash_new);
    }

    /// commit_doc's hash-change branch must NOT delete chunks if
    /// another live row still references the old hash. Two wiki pages
    /// with byte-identical bodies share `documents.hash`; updating
    /// one's body so its hash changes would, without the ref-count
    /// guard, wipe the survivor's chunks and leave it invisible to
    /// vector search with no lint signal.
    #[test]
    fn commit_doc_preserves_shared_chunks_on_hash_change() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let db_path = root.join(crate::INDEX_DB_NAME);
        let search = Bm25Search::open(&db_path).unwrap();

        // Two pages share `body_shared` → same hash, same chunks.
        let body_shared = "Shared body across both pages.";
        let body_new = "Page A's new body — distinct content now.";
        let hash_shared = crate::storage::content_hash(body_shared.as_bytes());
        let hash_new = crate::storage::content_hash(body_new.as_bytes());
        assert_ne!(hash_shared, hash_new);

        // Seed: page-a and page-b both at hash_shared, with chunks.
        search
            .with_transaction(|tx| {
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page-a.md",
                        title: "Page A",
                        tags: "",
                        source: None,
                        mtime: "2026-04-30T00:00:00Z",
                        body: body_shared,
                        size: body_shared.len() as i64,
                    },
                )?;
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page-b.md",
                        title: "Page B",
                        tags: "",
                        source: None,
                        mtime: "2026-04-30T00:00:00Z",
                        body: body_shared,
                        size: body_shared.len() as i64,
                    },
                )?;
                crate::vector::store_chunk(
                    tx,
                    &hash_shared,
                    0,
                    0,
                    body_shared.len(),
                    &vec![0.1f32; crate::embed::EMBEDDING_DIM],
                )?;
                Ok(())
            })
            .unwrap();

        // Update page-a's body. Its hash changes; page-b still
        // references hash_shared.
        search
            .with_transaction(|tx| {
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page-a.md",
                        title: "Page A",
                        tags: "",
                        source: None,
                        mtime: "2026-04-30T01:00:00Z",
                        body: body_new,
                        size: body_new.len() as i64,
                    },
                )?;
                Ok(())
            })
            .unwrap();

        // Chunks for hash_shared must remain — page-b still references them.
        let shared_chunks: i64 = search
            .with_connection(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT COUNT(*) FROM chunks WHERE hash = ?1",
                        rusqlite::params![&hash_shared],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap())
            })
            .unwrap();
        assert_eq!(
            shared_chunks, 1,
            "shared chunks must survive when a sibling row still references the hash"
        );
    }

    /// Regression: a `rebuild()` after a wiki file is removed from disk
    /// must drop chunks for that file (its document row is gone after
    /// rebuild, so the chunks are unreachable but were eating disk).
    /// Chunks for surviving files MUST be preserved — same hash on
    /// both sides, no needless re-embedding.
    #[test]
    fn bm25_rebuild_drops_orphan_chunks_keeps_surviving() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let wiki_dir = root.join("wiki");
        std::fs::create_dir_all(&wiki_dir).unwrap();

        let alpha_body = "Alpha body content.\n";
        let beta_body = "Beta body content.\n";
        let alpha_hash = crate::storage::content_hash(alpha_body.as_bytes());
        let beta_hash = crate::storage::content_hash(beta_body.as_bytes());

        std::fs::write(
            wiki_dir.join("alpha.md"),
            make_page("Alpha", alpha_body.trim_end(), &["concept"]),
        )
        .unwrap();
        std::fs::write(
            wiki_dir.join("beta.md"),
            make_page("Beta", beta_body.trim_end(), &["concept"]),
        )
        .unwrap();

        let db_path = root.join(crate::INDEX_DB_NAME);
        let search = Bm25Search::open(&db_path).unwrap();
        search.rebuild(root).unwrap();

        // Pretend both pages had been embedded.
        let conn = search.conn_for_test();
        crate::vector::store_chunk(&conn, &alpha_hash, 0, 0, 5, &vec![0.1f32; crate::embed::EMBEDDING_DIM]).unwrap();
        crate::vector::store_chunk(&conn, &beta_hash, 0, 0, 5, &vec![0.1f32; crate::embed::EMBEDDING_DIM]).unwrap();
        drop(conn);

        // Delete beta.md from disk.
        std::fs::remove_file(wiki_dir.join("beta.md")).unwrap();
        search.rebuild(root).unwrap();

        let alpha_chunks: i64 = search
            .conn_for_test()
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE hash = ?1",
                rusqlite::params![alpha_hash],
                |r| r.get(0),
            )
            .unwrap();
        let beta_chunks: i64 = search
            .conn_for_test()
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE hash = ?1",
                rusqlite::params![beta_hash],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alpha_chunks, 1, "surviving file's chunks must be preserved");
        assert_eq!(beta_chunks, 0, "deleted file's chunks must be cleaned up");
    }

    /// `rebuild()` runs `DELETE FROM documents` first; if `wiki/` is
    /// missing, it returns early. The orphan-chunk sweep must still
    /// run, otherwise the chunks/chunks_vec rows live forever with no
    /// document referring to them.
    #[test]
    fn bm25_rebuild_without_wiki_dir_clears_chunks() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let db_path = root.join(crate::INDEX_DB_NAME);
        let search = Bm25Search::open(&db_path).unwrap();

        // Seed two chunks; no documents row, no wiki dir.
        let h1 = "aa".repeat(32);
        let h2 = "bb".repeat(32);
        {
            let conn = search.conn_for_test();
            crate::vector::store_chunk(&conn, &h1, 0, 0, 5, &vec![0.1f32; crate::embed::EMBEDDING_DIM]).unwrap();
            crate::vector::store_chunk(&conn, &h2, 0, 0, 5, &vec![0.1f32; crate::embed::EMBEDDING_DIM]).unwrap();
        }
        assert!(!root.join("wiki").exists(), "test setup: wiki dir must be absent");

        search.rebuild(root).unwrap();

        let total_chunks: i64 = search
            .conn_for_test()
            .query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_chunks, 0, "wiki-less rebuild must not leak chunks");
    }

    #[test]
    fn bm25_rebuild() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let wiki_dir = root.join("wiki");
        std::fs::create_dir_all(&wiki_dir).unwrap();

        std::fs::write(
            wiki_dir.join("alpha.md"),
            make_page(
                "Alpha Topic",
                "Alpha is the first letter of the Greek alphabet.",
                &["concept"],
            ),
        )
        .unwrap();
        std::fs::write(
            wiki_dir.join("beta.md"),
            make_page(
                "Beta Topic",
                "Beta is the second letter of the Greek alphabet.",
                &["concept"],
            ),
        )
        .unwrap();

        let db_path = root.join(crate::INDEX_DB_NAME);
        let search = Bm25Search::open(&db_path).unwrap();
        search.rebuild(root).unwrap();

        let results = search
            .search_by_doc_type("alpha greek", "wiki", 10)
            .unwrap();
        assert!(!results.is_empty(), "rebuild should index wiki pages");
        assert_eq!(results[0].path, PathBuf::from("wiki/alpha.md"));
    }

    #[test]
    fn parse_page_for_indexing_valid() {
        let page = make_page("Test Title", "Some body text.", &["entity", "concept"]);
        let (title, body, tags, _summary, collections) = parse_page_for_indexing(&page).unwrap();
        assert_eq!(title, "Test Title");
        assert!(body.contains("Some body text."));
        assert_eq!(tags, "entity, concept");
        assert_eq!(collections, vec!["default".to_string()]);
    }

    #[test]
    fn parse_page_for_indexing_no_frontmatter() {
        assert!(parse_page_for_indexing("Just plain text.").is_none());
    }

    #[test]
    fn parse_page_for_indexing_empty_title() {
        let page = "---\ntitle: \"\"\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\nbody";
        assert!(parse_page_for_indexing(page).is_none());
    }

    #[test]
    fn bm25_malformed_query_returns_empty() {
        let (_dir, search) = open_temp_search();
        let results = search.search_by_doc_type("***", "wiki", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn bm25_index_page_replace() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/page.md"),
                "Original Title",
                "Original body content.",
                "tag1",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        search
            .index_page(
                Path::new("wiki/page.md"),
                "Updated Title",
                "Completely new body content about quantum computing.",
                "tag2",
                "2026-04-07T00:00:00Z",
            )
            .unwrap();

        let results = search
            .search_by_doc_type("quantum computing", "wiki", 10)
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Updated Title");
    }

    // -----------------------------------------------------------------------
    // Index generation
    // -----------------------------------------------------------------------

    #[test]
    fn generate_index_from_db() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "The borrow checker enforces ownership rules at compile time.",
                "concept, rust",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let index = search.generate_index().unwrap();
        assert!(index.contains("# Index"), "got: {index}");
        assert!(index.contains("Rust Borrow Checker"));
        assert!(index.contains("wiki/rust-borrow.md"));
    }

    #[test]
    fn generate_index_empty_db() {
        let (_dir, search) = open_temp_search();
        let index = search.generate_index().unwrap();
        assert!(index.contains("No wiki pages yet"));
    }

    #[test]
    fn lookup_stem_resolves() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/my-page.md"),
                "My Page",
                "Body.",
                "",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let path = search.lookup_stem("my-page").unwrap();
        assert_eq!(path, Some(PathBuf::from("wiki/my-page.md")));

        let missing = search.lookup_stem("nonexistent").unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn lookup_title_case_insensitive() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/my-page.md"),
                "My Page Title",
                "Body.",
                "",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let path = search.lookup_title("My Page Title").unwrap();
        assert_eq!(path, Some(PathBuf::from("wiki/my-page.md")));

        let path = search.lookup_title("my page title").unwrap();
        assert_eq!(path, Some(PathBuf::from("wiki/my-page.md")));

        let missing = search.lookup_title("nonexistent").unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn page_count_accurate() {
        let (_dir, search) = open_temp_search();
        assert_eq!(search.page_count().unwrap(), 0);

        search
            .index_page(
                Path::new("wiki/alpha.md"),
                "Alpha",
                "Alpha body.",
                "tag",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/beta.md"),
                "Beta",
                "Beta body.",
                "tag",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        assert_eq!(search.page_count().unwrap(), 2);
    }

    #[test]
    fn get_last_modified_resolves() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/alpha.md"),
                "Alpha",
                "Body.",
                "",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let ts = search
            .get_last_modified(Path::new("wiki/alpha.md"))
            .unwrap();
        assert_eq!(ts, Some("2026-04-06T00:00:00Z".to_string()));

        let missing = search
            .get_last_modified(Path::new("wiki/missing.md"))
            .unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn all_stems_and_titles_lists_all() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/alpha.md"),
                "Alpha Topic",
                "Body.",
                "",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/beta.md"),
                "Beta Topic",
                "Body.",
                "",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let pairs = search.all_stems_and_titles().unwrap();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("alpha".to_string(), "Alpha Topic".to_string())));
        assert!(pairs.contains(&("beta".to_string(), "Beta Topic".to_string())));
    }

    // -----------------------------------------------------------------------
    // Ingest jobs
    // -----------------------------------------------------------------------

    #[test]
    fn ingest_jobs_inserts_and_lists_transcript_job() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        s.insert_ingest_job(
            "job-t1",
            crate::search::JobType::Transcript,
            "/abs/path/to/session.jsonl",
            Some("claude-code"),
            "deadbeef".repeat(8).as_str(),
            &["default".to_string()],
        )
        .unwrap();
        let jobs = s.pending_ingest_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_id, "job-t1");
        assert_eq!(jobs[0].job_type, crate::search::JobType::Transcript);
        assert_eq!(jobs[0].agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn ingest_jobs_inserts_and_lists_document_job() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        s.insert_ingest_job(
            "job-d1",
            crate::search::JobType::Document,
            "https://example.com/post",
            None,
            "cafebabe".repeat(8).as_str(),
            &["team-a".to_string(), "incidents".to_string()],
        )
        .unwrap();
        let jobs = s.pending_ingest_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_type, crate::search::JobType::Document);
        assert_eq!(jobs[0].agent, None);
        assert_eq!(jobs[0].collections, vec!["team-a", "incidents"]);
    }

    /// Daemon startup must transition every pending/processing job to
    /// failed with the canonical "interrupted by daemon restart"
    /// reason and bump updated_at. Completed/failed rows must NOT be
    /// touched. This invariant is load-bearing: the 30-day prune is
    /// keyed off updated_at, so a restart pushing the field forward
    /// gives the user the full retention window to inspect what
    /// failed before the row is reaped.
    #[test]
    fn recover_stuck_ingest_jobs_only_touches_in_flight_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        // Seed: pending, processing, completed, failed.
        s.insert_ingest_job(
            "job-pending",
            crate::search::JobType::Transcript,
            "/p1.jsonl",
            None,
            "00".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.insert_ingest_job(
            "job-processing",
            crate::search::JobType::Transcript,
            "/p2.jsonl",
            None,
            "11".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("job-processing", "processing", None)
            .unwrap();
        s.insert_ingest_job(
            "job-completed",
            crate::search::JobType::Transcript,
            "/p3.jsonl",
            None,
            "22".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("job-completed", "completed", None)
            .unwrap();
        s.insert_ingest_job(
            "job-failed",
            crate::search::JobType::Transcript,
            "/p4.jsonl",
            None,
            "33".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("job-failed", "failed", Some("worker died"))
            .unwrap();

        let recovered = s.recover_stuck_ingest_jobs().unwrap();
        assert_eq!(recovered, 2, "pending + processing should transition");

        let row_status = |job_id: &str| -> (String, Option<String>) {
            s.with_connection(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT status, error FROM ingest_jobs WHERE job_id=?1",
                        rusqlite::params![job_id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
                    )
                    .unwrap())
            })
            .unwrap()
        };
        assert_eq!(row_status("job-pending").0, "failed");
        assert_eq!(row_status("job-processing").0, "failed");
        assert_eq!(
            row_status("job-pending").1.as_deref(),
            Some("interrupted by daemon restart")
        );
        // Completed/failed rows untouched.
        assert_eq!(row_status("job-completed").0, "completed");
        assert_eq!(row_status("job-failed").1.as_deref(), Some("worker died"));
    }

    /// Prune deletes terminal rows older than the cutoff and leaves
    /// recent ones plus in-flight rows alone.
    #[test]
    fn prune_terminal_ingest_jobs_keyed_off_updated_at() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        // Two completed rows: one freshly stamped, one back-dated 60d.
        s.insert_ingest_job(
            "old",
            crate::search::JobType::Transcript,
            "/old.jsonl",
            None,
            "aa".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("old", "completed", None).unwrap();
        s.insert_ingest_job(
            "new",
            crate::search::JobType::Transcript,
            "/new.jsonl",
            None,
            "bb".repeat(32).as_str(),
            &[],
        )
        .unwrap();
        s.update_ingest_job_status("new", "completed", None).unwrap();
        // Back-date `old` 60 days. SQLite handles datetime arithmetic.
        s.with_connection(|conn| {
            conn.execute(
                "UPDATE ingest_jobs SET updated_at = datetime('now', '-60 days') \
                 WHERE job_id = 'old'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        let pruned = s.prune_terminal_ingest_jobs(30).unwrap();
        assert_eq!(pruned, 1, "only `old` should be pruned at 30-day cutoff");
        let remaining: Vec<String> = s
            .with_connection(|conn| {
                let mut stmt = conn
                    .prepare("SELECT job_id FROM ingest_jobs ORDER BY job_id")
                    .unwrap();
                let rows: Vec<String> = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(rows)
            })
            .unwrap();
        assert_eq!(remaining, vec!["new".to_string()]);
    }

    /// Two documents with byte-identical bodies share a `documents.hash`
    /// and thus share `chunks` / `chunks_vec` rows (which are hash-keyed).
    /// Deleting one doc must NOT wipe the survivor's chunks — that was
    /// a silent vector-search invisibility bug.
    #[test]
    fn delete_document_with_cleanup_preserves_shared_chunks() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        let body = "shared body across two pages";
        let hash = crate::storage::content_hash(body.as_bytes());

        // Two wiki rows with the same body hash, plus a chunk seeded
        // for that hash. Real ingest would do this via commit_doc +
        // embed_document; we drive it directly to keep the test
        // narrow to delete_document_with_cleanup's behavior.
        s.with_transaction(|tx| {
            tx.execute(
                "INSERT INTO documents (doc_type, path, title, hash, tags, source, mtime, size) \
                 VALUES ('wiki', 'wiki/a.md', 'A', ?1, '', NULL, '2026-04-30T00:00:00Z', ?2)",
                rusqlite::params![&hash, body.len() as i64],
            )?;
            tx.execute(
                "INSERT INTO documents (doc_type, path, title, hash, tags, source, mtime, size) \
                 VALUES ('wiki', 'wiki/b.md', 'B', ?1, '', NULL, '2026-04-30T00:00:00Z', ?2)",
                rusqlite::params![&hash, body.len() as i64],
            )?;
            crate::vector::store_chunk(
                tx,
                &hash,
                0,
                0,
                body.len(),
                &vec![0.1f32; crate::embed::EMBEDDING_DIM],
            )?;
            Ok(())
        })
        .unwrap();

        // Delete one of the two siblings.
        s.delete_document_with_cleanup("wiki/a.md").unwrap();

        // Surviving sibling's chunk must remain.
        let chunk_count: i64 = s
            .with_connection(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT COUNT(*) FROM chunks WHERE hash = ?1",
                        rusqlite::params![&hash],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap())
            })
            .unwrap();
        assert_eq!(
            chunk_count, 1,
            "shared chunks must not be wiped when one of two referencing rows is deleted"
        );

        // Deleting the second sibling has no other refs, chunks go away.
        s.delete_document_with_cleanup("wiki/b.md").unwrap();
        let chunk_count_after_last: i64 = s
            .with_connection(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT COUNT(*) FROM chunks WHERE hash = ?1",
                        rusqlite::params![&hash],
                        |r| r.get::<_, i64>(0),
                    )
                    .unwrap())
            })
            .unwrap();
        assert_eq!(
            chunk_count_after_last, 0,
            "last referencing row's delete should clean the orphaned chunks"
        );
    }

    /// Dedup must gate on completed-job existence, not on raw row
    /// existence. A prior attempt that committed the raw doc but
    /// crashed before producing wiki pages must NOT lock out retries
    /// — that pattern was a hidden permanent failure.
    #[test]
    fn ingest_dedup_uses_completed_job_not_raw_row() {
        let dir = tempfile::TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        let h = "deadbeef".repeat(8);

        // No job at all → not deduped.
        assert!(!s.ingest_dedup_exists(&h).unwrap());

        // Job inserted but pending (raw maybe stored, wiki pages
        // pending) → still not deduped, retry must be allowed.
        s.insert_ingest_job(
            "job-x",
            crate::search::JobType::Transcript,
            "/x.jsonl",
            None,
            &h,
            &[],
        )
        .unwrap();
        assert!(!s.ingest_dedup_exists(&h).unwrap(), "pending must not dedup");

        // Job marked failed → still not deduped (retry must be allowed).
        s.update_ingest_job_status("job-x", "failed", Some("crash"))
            .unwrap();
        assert!(!s.ingest_dedup_exists(&h).unwrap(), "failed must not dedup");

        // Only a completed job triggers dedup.
        s.update_ingest_job_status("job-x", "completed", None)
            .unwrap();
        assert!(
            s.ingest_dedup_exists(&h).unwrap(),
            "completed must dedup retries"
        );
    }

    // -----------------------------------------------------------------------
    // New: explicit FTS row write (Task 9)
    // -----------------------------------------------------------------------

    /// `lookup_documents_with_collections` returns the per-doc-id
    /// (mtime, size) map that `vector_search_as_results` uses for the
    /// stat-check. Without this map the stat-check can't skip stale
    /// results.
    #[test]
    fn lookup_documents_with_collections_returns_meta_per_id() {
        let (_dir, search) = open_temp_search();
        let body = "page body";
        let id = search
            .index_page(
                std::path::Path::new("wiki/m.md"),
                "M",
                body,
                "",
                "2026-04-30T12:00:00Z",
            )
            .unwrap();
        let hash = crate::storage::content_hash(body.as_bytes());
        let (_docs, _memberships, meta_by_id) = search
            .lookup_documents_with_collections(&[hash])
            .unwrap();
        let (mtime, size) = meta_by_id
            .get(&id)
            .expect("meta should be present for the inserted doc");
        assert_eq!(mtime, "2026-04-30T12:00:00Z");
        assert_eq!(*size, body.len() as i64);
    }

    #[test]
    fn upsert_document_writes_explicit_fts_row() {
        let conn = setup_db();
        upsert_document_conn(
            &conn,
            &UpsertDocument {
                doc_type: "wiki",
                path: "wiki/alpha.md",
                title: "Alpha",
                hash: &"a".repeat(64),
                tags: "concept",
                source: None,
                body: "the quick brown fox jumps over the lazy dog",
                mtime: "2026-04-26T00:00:00Z",
                size: 100,
            },
        )
        .unwrap();

        // The FTS row should be searchable for a body token. Contentless
        // FTS5 doesn't return column data, so we join documents via rowid.
        let mut stmt = conn
            .prepare(
                "SELECT d.path FROM documents_fts f JOIN documents d ON d.id = f.rowid \
                 WHERE documents_fts MATCH 'fox' LIMIT 1",
            )
            .unwrap();
        let path: Option<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .next()
            .transpose()
            .unwrap();
        assert_eq!(path.as_deref(), Some("wiki/alpha.md"));
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
        let search = Bm25Search::open(&db_path).unwrap();

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
