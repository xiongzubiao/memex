//! Document write path. `commit_doc` is the single source of truth for
//! the (hash, body, FTS, chunks) invariants every writer must satisfy:
//! the body is hashed verbatim, `documents.size` is the full file size,
//! `titles_fts` is refreshed from the new title, and `chunks` +
//! `chunks_fts` are populated from a markdown-only chunking pass at
//! commit time so chunk-level BM25 works before any embedder runs.
//!
//! `Db` methods here are the high-level write operations (batched
//! ingest, delete, rebuild) that compose `commit_doc` under a
//! transaction.

use std::path::Path;

use walkdir::WalkDir;

use crate::error::Result;

use super::collections::{
    ensure_default_document_collection, set_document_collections_in_conn,
};
use super::{Db, mutex_err, parse_page_for_indexing, sqlite_err};

/// A wiki page ready to be inserted by `store_wiki_page`.
///
/// `content` is the full markdown (frontmatter + body) as it lands on
/// disk at `wiki/<slug>.md`; the caller composes it.
#[derive(Debug, Clone)]
pub struct IngestWikiPage {
    pub slug: String,
    pub title: String,
    pub content: String,
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
    pub source: Option<&'a str>,
    pub mtime: std::time::SystemTime,
    /// Body bytes (frontmatter already stripped). Hashed and fed to FTS verbatim.
    pub body: &'a str,
    /// Full file size on disk (frontmatter + body) — stored as `documents.size`.
    pub size: i64,
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

/// Outcome of `commit_doc`: the upsert result + the body hash for
/// callers that need to embed or report it.
pub struct CommitResult {
    pub outcome: CommitOutcome,
    pub body_hash: String,
}

/// Insert or update a document on a raw connection. Writes the `documents`
/// row and refreshes the matching `titles_fts` row in one go. The body
/// hash is derived from `spec.body` here so callers can't supply an
/// inconsistent hash. Returns `(id, body_hash)` — `id` for fixture seeding
/// that joins on document id; `body_hash` for `commit_doc`'s chunk-cleanup
/// decision.
pub fn upsert_document(
    conn: &rusqlite::Connection,
    spec: &DocSpec<'_>,
) -> Result<(i64, String)> {
    let body_hash = crate::storage::content_hash(spec.body.as_bytes());

    // Capture the previous (id, title) so we can issue a precise
    // FTS5 'delete' (contentless tables need the previous column values
    // to tear down their per-doc index entries; empty placeholders
    // corrupt them).
    let prior: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, title FROM documents WHERE doc_type=?1 AND path=?2",
            rusqlite::params![spec.doc_type, spec.path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    conn.execute(
        "INSERT INTO documents (doc_type, path, title, hash, source, mtime, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(doc_type, path) DO UPDATE SET
             title = excluded.title, hash = excluded.hash,
             source = excluded.source, mtime = excluded.mtime, size = excluded.size",
        rusqlite::params![
            spec.doc_type,
            spec.path,
            spec.title,
            &body_hash,
            spec.source,
            crate::storage::systime_to_nanos(spec.mtime),
            spec.size,
        ],
    )
    .map_err(sqlite_err)?;
    let id: i64 = conn
        .query_row(
            "SELECT id FROM documents WHERE doc_type=?1 AND path=?2",
            rusqlite::params![spec.doc_type, spec.path],
            |r| r.get(0),
        )
        .map_err(sqlite_err)?;
    if let Some((prior_id, prior_title)) = prior {
        // FTS5 contentless tables verify tokens against what we provide
        // for the delete payload — pass the prior title verbatim. Log
        // failures rather than swallowing: repeated cleanup misses
        // accumulate stale tokens that show as ghost hits in search.
        if let Err(e) = conn.execute(
            "INSERT INTO titles_fts(titles_fts, rowid, title) VALUES('delete', ?1, ?2)",
            rusqlite::params![prior_id, prior_title],
        ) {
            tracing::warn!(
                document_id = prior_id,
                error = %e,
            );
        }
    }
    conn.execute(
        "INSERT INTO titles_fts(rowid, title) VALUES (?1, ?2)",
        rusqlite::params![id, spec.title],
    )
    .map_err(sqlite_err)?;
    ensure_default_document_collection(conn, id)?;
    Ok((id, body_hash))
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
pub fn commit_doc(
    tx: &rusqlite::Connection,
    spec: &DocSpec<'_>,
) -> Result<CommitResult> {
    let prev_hash: Option<String> = tx
        .query_row(
            "SELECT hash FROM documents WHERE doc_type=?1 AND path=?2",
            rusqlite::params![spec.doc_type, spec.path],
            |r| r.get(0),
        )
        .ok();

    let (_id, body_hash) = upsert_document(tx, spec)?;

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

    // Always populate chunks + chunks_fts at commit time, even without
    // an embedding model. chunks_fts is used by chunk-level BM25, which
    // must work whether or not the doc has been embedded. The separate
    // embed step later writes chunks_vec; chunks/chunks_fts rows already
    // exist from this point.
    //
    // Markdown-only chunking at commit time (no embedder yet).
    // `embed_document` later replaces these with the full pipeline
    // chunks (Markdown → Semantic → SentenceSplitter token budget).
    if !chunks_exist_for_hash(tx, &body_hash)? {
        let chunks = crate::chunking::chunk_markdown(spec.body);
        for (seq, chunk) in chunks.iter().enumerate() {
            tx.execute(
                "INSERT OR REPLACE INTO chunks (hash, seq, pos, len) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![body_hash, seq as i64, chunk.pos as i64, chunk.len as i64],
            )
            .map_err(sqlite_err)?;
            let chunk_text = &spec.body[chunk.pos..chunk.pos + chunk.len];
            tx.execute(
                "DELETE FROM chunks_fts WHERE hash = ?1 AND seq = ?2",
                rusqlite::params![body_hash, seq as i64],
            )
            .map_err(sqlite_err)?;
            tx.execute(
                "INSERT INTO chunks_fts(chunk_text, hash, seq) VALUES (?1, ?2, ?3)",
                rusqlite::params![chunk_text, body_hash, seq as i64],
            )
            .map_err(sqlite_err)?;
        }
    }

    Ok(CommitResult { outcome, body_hash })
}

fn chunks_exist_for_hash(conn: &rusqlite::Connection, hash: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks WHERE hash = ?1",
            rusqlite::params![hash],
            |r| r.get(0),
        )
        .map_err(sqlite_err)?;
    Ok(count > 0)
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

impl Db {
    /// Run `f` inside a SQLite transaction with the standard
    /// commit-on-Ok / rollback-on-Err semantics. Centralizes the
    /// `lock conn → tx → result → commit/rollback` boilerplate so
    /// per-statement methods don't each reinvent it.
    fn run_in_tx<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<R>,
    {
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let tx = conn.transaction().map_err(sqlite_err)?;
        let result = f(&tx);
        match &result {
            Ok(_) => tx.commit().map_err(sqlite_err)?,
            Err(_) => {
                let _ = tx.rollback();
            }
        }
        result
    }

    /// Store the raw source row in its own transaction. Returns the
    /// content hash. The caller has already written the raw bytes to
    /// the hash-addressed path on disk; this method only updates the
    /// index.
    ///
    /// The `documents` row is keyed by the hash-addressed relative
    /// path `raw/<H[..2]>/<H[2..]>` so it matches the on-disk artifact
    /// layout. The original `source_path` (URL, file path, etc.) is
    /// preserved in the `source` column.
    pub fn store_raw_source(
        &self,
        raw_file: &str,
        source_path: &str,
        source_title: &str,
        collections: &[String],
        memex_root: &std::path::Path,
    ) -> Result<String> {
        self.run_in_tx(|tx| {
            let (_, raw_body) = crate::storage::split_frontmatter(raw_file).ok_or_else(|| {
                crate::error::MemexError::Other(anyhow::anyhow!(
                    "store_raw_source raw_file has no frontmatter"
                ))
            })?;
            let source_hash = crate::storage::content_hash(raw_body.as_bytes());
            let raw_rel = format!("raw/{}/{}", &source_hash[..2], &source_hash[2..]);
            commit_doc(
                tx,
                &DocSpec {
                    doc_type: "raw",
                    path: &raw_rel,
                    title: source_title,
                    source: Some(source_path),
                    mtime: std::fs::metadata(memex_root.join(&raw_rel))?.modified()?,
                    body: raw_body,
                    size: raw_file.len() as i64,
                },
            )?;
            set_document_collections_in_conn(tx, "raw", &raw_rel, collections)?;
            Ok(source_hash)
        })
    }

    /// Store one wiki page in its own transaction. Returns the body
    /// content hash. Caller has already written the markdown to disk
    /// under the per-slug write lock and is responsible for ensuring
    /// no concurrent writer modifies `wiki/<slug>.md` between the
    /// disk write and this call.
    ///
    /// Note that `Db` holds a single `Mutex<Connection>`, so concurrent
    /// per-slug callers serialize on the SQLite write end. Per-slug
    /// commits are still useful for fault isolation and for releasing
    /// the slug-lock incrementally; the parallelism win for the
    /// surrounding ingest pipeline is in the LLM-MERGE step, not here.
    pub fn store_wiki_page(
        &self,
        page: &IngestWikiPage,
        collections: &[String],
        memex_root: &std::path::Path,
    ) -> Result<String> {
        self.run_in_tx(|tx| {
            let rel_path = format!("wiki/{}.md", page.slug);
            let (_, body) = crate::storage::split_frontmatter(&page.content).ok_or_else(|| {
                crate::error::MemexError::Other(anyhow::anyhow!(
                    "store_wiki_page {} has no frontmatter",
                    page.slug
                ))
            })?;
            let res = commit_doc(
                tx,
                &DocSpec {
                    doc_type: "wiki",
                    path: &rel_path,
                    title: &page.title,
                    source: None,
                    mtime: std::fs::metadata(memex_root.join(&rel_path))?.modified()?,
                    body,
                    size: page.content.len() as i64,
                },
            )?;
            set_document_collections_in_conn(tx, "wiki", &rel_path, collections)?;
            Ok(res.body_hash)
        })
    }

    /// Delete a document by path. The matching `titles_fts` row is
    /// removed via FTS5 'delete' command; chunks are cleaned via
    /// `vector::delete_chunks` only if no other document still
    /// references the same hash (two wiki pages with byte-identical
    /// bodies share chunks).
    pub fn delete_document_with_cleanup(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        // Fetch (id, hash, title) before deletion so we can clean FTS +
        // vec rows. The title is needed for FTS5's contentless 'delete'
        // payload — passing the actual prior tokens cleans the index;
        // passing empty leaves stale title tokens behind.
        let row: Option<(i64, String, String)> = conn
            .query_row(
                "SELECT id, hash, title FROM documents WHERE path = ?1",
                rusqlite::params![path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok();
        if let Some((id, hash, title)) = &row {
            // Best-effort: remove FTS row keyed by id. Logged on
            // failure so stale tokens don't accumulate silently —
            // recovery is `memex lint --fix` which reindexes.
            if let Err(e) = conn.execute(
                "INSERT INTO titles_fts(titles_fts, rowid, title) VALUES('delete', ?1, ?2)",
                rusqlite::params![id, title],
            ) {
                tracing::warn!(
                    document_id = id,
                    path = %path,
                    error = %e,
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

    /// Test-only: remove a documents row + FTS row by path. Does NOT clean
    /// chunks — production deletes use `delete_document_with_cleanup`.
    #[cfg(test)]
    pub fn remove_page(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        // Remove FTS row before the documents row to keep the index in sync.
        if let Ok((id, title)) = conn.query_row(
            "SELECT id, title FROM documents WHERE path = ?1",
            rusqlite::params![path],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        ) {
            let _ = conn.execute(
                "INSERT INTO titles_fts(titles_fts, rowid, title) VALUES('delete', ?1, ?2)",
                rusqlite::params![id, title],
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
            "INSERT INTO titles_fts(titles_fts) VALUES('delete-all')",
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

            if let Some((title, body, _summary, collections)) =
                parse_page_for_indexing(&content)
            {
                let rel_path = abs_path.strip_prefix(root).unwrap_or(abs_path);
                let path_str = rel_path.to_string_lossy().to_string();
                let Ok(mtime) = std::fs::metadata(abs_path).and_then(|m| m.modified()) else {
                    continue;
                };
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
                        source: None,
                        mtime,
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

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::search::test_helpers::{make_page, open_temp_search, setup_db};

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
        let search = Db::open(&db_path).unwrap();

        search
            .with_transaction(|tx| {
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page.md",
                        title: "Page",
                        source: None,
                        mtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(2000),
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
            "",
            &vec![0.1f32; crate::embed::EMBEDDING_DIM],
        )
        .unwrap();
        drop(conn);

        let result = search
            .with_transaction(|tx| {
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page.md",
                        title: "Page",
                        source: None,
                        mtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(3000),
                        body: body_new,
                        size: body_new.len() as i64,
                    },
                )
            })
            .unwrap();
        assert_eq!(result.body_hash, hash_new);
        assert!(matches!(result.outcome, CommitOutcome::Updated));

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
        assert_eq!(old_chunks, 0);
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
        let search = Db::open(&db_path).unwrap();

        let body_shared = "Shared body across both pages.";
        let body_new = "Page A's new body — distinct content now.";
        let hash_shared = crate::storage::content_hash(body_shared.as_bytes());
        let hash_new = crate::storage::content_hash(body_new.as_bytes());
        assert_ne!(hash_shared, hash_new);

        search
            .with_transaction(|tx| {
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page-a.md",
                        title: "Page A",
                        source: None,
                        mtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(4000),
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
                        source: None,
                        mtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(4000),
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
                    "",
                    &vec![0.1f32; crate::embed::EMBEDDING_DIM],
                )?;
                Ok(())
            })
            .unwrap();

        search
            .with_transaction(|tx| {
                commit_doc(
                    tx,
                    &DocSpec {
                        doc_type: "wiki",
                        path: "wiki/page-a.md",
                        title: "Page A",
                        source: None,
                        mtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(5000),
                        body: body_new,
                        size: body_new.len() as i64,
                    },
                )?;
                Ok(())
            })
            .unwrap();

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
        assert_eq!(shared_chunks, 1);
    }

    /// Regression: a `rebuild()` after a wiki file is removed from disk
    /// must drop chunks for that file (its document row is gone after
    /// rebuild, so the chunks are unreachable but were eating disk).
    /// Chunks for surviving files MUST be preserved — same hash on
    /// both sides, no needless re-embedding.
    #[test]
    fn rebuild_drops_orphan_chunks_keeps_surviving() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let wiki_dir = root.join("wiki");
        std::fs::create_dir_all(&wiki_dir).unwrap();

        let alpha_body = "Alpha body content.\n";
        let beta_body = "Beta body content.\n";
        let alpha_hash = crate::storage::content_hash(alpha_body.as_bytes());
        let beta_hash = crate::storage::content_hash(beta_body.as_bytes());

        std::fs::write(wiki_dir.join("alpha.md"), make_page("Alpha", alpha_body.trim_end())).unwrap();
        std::fs::write(wiki_dir.join("beta.md"), make_page("Beta", beta_body.trim_end())).unwrap();

        let db_path = root.join(crate::INDEX_DB_NAME);
        let search = Db::open(&db_path).unwrap();
        search.rebuild(root).unwrap();

        let conn = search.conn_for_test();
        crate::vector::store_chunk(&conn, &alpha_hash, 0, 0, 5, "", &vec![0.1f32; crate::embed::EMBEDDING_DIM]).unwrap();
        crate::vector::store_chunk(&conn, &beta_hash, 0, 0, 5, "", &vec![0.1f32; crate::embed::EMBEDDING_DIM]).unwrap();
        drop(conn);

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
    fn rebuild_without_wiki_dir_clears_chunks() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let db_path = root.join(crate::INDEX_DB_NAME);
        let search = Db::open(&db_path).unwrap();

        let h1 = "aa".repeat(32);
        let h2 = "bb".repeat(32);
        {
            let conn = search.conn_for_test();
            crate::vector::store_chunk(&conn, &h1, 0, 0, 5, "", &vec![0.1f32; crate::embed::EMBEDDING_DIM]).unwrap();
            crate::vector::store_chunk(&conn, &h2, 0, 0, 5, "", &vec![0.1f32; crate::embed::EMBEDDING_DIM]).unwrap();
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
    fn rebuild_indexes_wiki_pages() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let wiki_dir = root.join("wiki");
        std::fs::create_dir_all(&wiki_dir).unwrap();

        std::fs::write(
            wiki_dir.join("alpha.md"),
            make_page("Alpha Topic", "Alpha is the first letter of the Greek alphabet."),
        )
        .unwrap();
        std::fs::write(
            wiki_dir.join("beta.md"),
            make_page("Beta Topic", "Beta is the second letter of the Greek alphabet."),
        )
        .unwrap();

        let db_path = root.join(crate::INDEX_DB_NAME);
        let search = Db::open(&db_path).unwrap();
        search.rebuild(root).unwrap();

        let results = search
            .search_by_doc_type("alpha topic", "wiki", 10, &[])
            .unwrap();
        assert!(!results.is_empty(), "rebuild should index wiki pages");
        assert_eq!(results[0].path, std::path::PathBuf::from("wiki/alpha.md"));
    }

    /// Two documents with byte-identical bodies share a `documents.hash`
    /// and thus share `chunks` / `chunks_vec` rows (which are hash-keyed).
    /// Deleting one doc must NOT wipe the survivor's chunks — that was
    /// a silent vector-search invisibility bug.
    #[test]
    fn delete_document_with_cleanup_preserves_shared_chunks() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let s = memex.search();
        let body = "shared body across two pages";
        let hash = crate::storage::content_hash(body.as_bytes());

        s.with_transaction(|tx| {
            tx.execute(
                "INSERT INTO documents (doc_type, path, title, hash, source, mtime, size) \
                 VALUES ('wiki', 'wiki/a.md', 'A', ?1, NULL, '2026-04-30T00:00:00Z', ?2)",
                rusqlite::params![&hash, body.len() as i64],
            )?;
            tx.execute(
                "INSERT INTO documents (doc_type, path, title, hash, source, mtime, size) \
                 VALUES ('wiki', 'wiki/b.md', 'B', ?1, NULL, '2026-04-30T00:00:00Z', ?2)",
                rusqlite::params![&hash, body.len() as i64],
            )?;
            crate::vector::store_chunk(
                tx,
                &hash,
                0,
                0,
                body.len(),
                "",
                &vec![0.1f32; crate::embed::EMBEDDING_DIM],
            )?;
            Ok(())
        })
        .unwrap();

        s.delete_document_with_cleanup("wiki/a.md").unwrap();

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
        assert_eq!(chunk_count, 1);

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
        assert_eq!(chunk_count_after_last, 0);
    }

    #[test]
    fn upsert_document_writes_titles_fts_row() {
        let conn = setup_db();
        upsert_document(
            &conn,
            &DocSpec {
                doc_type: "wiki",
                path: "wiki/alpha.md",
                title: "Alpha Bearer Tokens",
                source: None,
                body: "the quick brown fox jumps over the lazy dog",
                mtime: std::time::UNIX_EPOCH + std::time::Duration::from_nanos(1000),
                size: 100,
            },
        )
        .unwrap();

        // Contentless FTS5 doesn't return column data; join via rowid.
        let mut stmt = conn
            .prepare(
                "SELECT d.path FROM titles_fts f JOIN documents d ON d.id = f.rowid \
                 WHERE titles_fts MATCH 'bearer' LIMIT 1",
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

    fn collections_row_count(db: &Db) -> i64 {
        db.with_connection(|c| {
            Ok(c.query_row("SELECT COUNT(*) FROM collections", [], |r| r.get(0))?)
        })
        .unwrap()
    }

    #[test]
    fn run_in_tx_commits_on_ok() {
        let (_tmp, db) = open_temp_search();
        assert_eq!(collections_row_count(&db), 0);
        let r: Result<()> = db.run_in_tx(|tx| {
            tx.execute("INSERT INTO collections(name) VALUES('alpha')", [])
                .map_err(sqlite_err)?;
            Ok(())
        });
        r.unwrap();
        assert_eq!(collections_row_count(&db), 1, "Ok-returning closure should commit");
    }

    #[test]
    fn run_in_tx_rolls_back_on_err() {
        let (_tmp, db) = open_temp_search();
        let r: Result<()> = db.run_in_tx(|tx| {
            tx.execute("INSERT INTO collections(name) VALUES('alpha')", [])
                .map_err(sqlite_err)?;
            Err(crate::error::MemexError::Other(anyhow::anyhow!(
                "force rollback"
            )))
        });
        assert!(r.is_err());
        assert_eq!(collections_row_count(&db), 0, "Err-returning closure should roll back");
    }
}
