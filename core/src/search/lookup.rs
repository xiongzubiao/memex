//! Read path: ref resolution, lookups, listing, metadata. Every method
//! here is a SELECT — no writes, no FTS MATCH (that's `bm25.rs`). The
//! free `resolve_ref` is the three-tier identifier resolver shared by
//! anywhere a user-facing string can mean docid prefix, filename stem,
//! or title.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::types::Document;

use super::{Db, mutex_err, sqlite_err};

/// Document metadata pulled from the `documents` row.
#[derive(Debug, Clone)]
pub struct DocumentMeta {
    pub mtime: std::time::SystemTime,
    pub size: i64,
}

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

/// Result of `Db::lookup_documents_with_collections`:
/// (hash → docs, doc_id → collections, doc_id → (mtime, size)).
/// The hash→docs fan-out lets retrieval map one chunk-hash to multiple
/// wiki pages; the per-doc-id maps carry collection membership and the
/// stored stat metadata used by retrieval's stat-check — drop results
/// whose on-disk mtime/size diverge from the indexed values, since the
/// chunk pos/len no longer slice valid coordinates.
pub type DocLookup = (
    std::collections::HashMap<String, Vec<Document>>,
    std::collections::HashMap<i64, Vec<String>>,
    std::collections::HashMap<i64, (std::time::SystemTime, i64)>,
);

/// Construct a `Document` from a SQLite row with the standard 6-column SELECT.
/// Expects columns: id, doc_type, path, title, hash.
pub(super) fn row_to_document(row: &rusqlite::Row) -> rusqlite::Result<Document> {
    Ok(Document {
        id: row.get(0)?,
        doc_type: row.get(1)?,
        path: row.get(2)?,
        title: row.get(3)?,
        hash: row.get(4)?,
    })
}

/// Three-tier identifier resolution.
///
/// 1. Hash prefix: `WHERE hash LIKE ?1 || '%'`
/// 2. Stem: match path basename without extension
/// 3. Title: `WHERE title = ?1 COLLATE NOCASE`
pub fn resolve_ref(conn: &rusqlite::Connection, reference: &str) -> Result<Vec<Document>> {
    // Tier 1: Hash prefix match
    let mut stmt = conn.prepare(
        "SELECT id, doc_type, path, title, hash \
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
        "SELECT id, doc_type, path, title, hash \
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
        "SELECT id, doc_type, path, title, hash \
         FROM documents WHERE title = ?1 COLLATE NOCASE",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![reference], row_to_document)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(docs)
}

/// Convert a `SystemTime` mtime to a human-readable RFC 3339 string for display.
fn format_mtime(t: std::time::SystemTime) -> String {
    let dt = chrono::DateTime::<chrono::Utc>::from(t);
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

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

impl Db {
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
                        mtime: crate::storage::nanos_to_systime(row.get::<_, i64>(0)?),
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
                        let mtime: i64 = row.get(1)?;
                        let size: i64 = row.get(2)?;
                        Ok((path, mtime, size))
                    })
                    .map_err(sqlite_err)?;
                for row in rows.flatten() {
                    let (path, mtime, size) = row;
                    out.insert(
                        (doc_type.to_string(), path),
                        DocumentMeta { mtime: crate::storage::nanos_to_systime(mtime), size },
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

    /// Get the last-modified timestamp (`mtime`) for a page by its path.
    pub fn get_last_modified(&self, path: &Path) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension;
        let path_str = path.to_string_lossy();
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let row = conn
            .query_row(
                "SELECT mtime FROM documents WHERE path = ?1",
                rusqlite::params![path_str.as_ref()],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(sqlite_err)?;
        Ok(row)
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
                let stem = Path::new(&path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                (stem, title)
            })
            .collect())
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
                            mtime: format_mtime(crate::storage::nanos_to_systime(row.get::<_, i64>(4)?)),
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
                "SELECT id, doc_type, path, title, hash \
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
                "SELECT id, doc_type, path, title, hash \
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

    /// Look up document metadata by content hash.
    ///
    /// Returns all documents that reference the given content hash.
    /// Used by vector search to convert chunk-level results (keyed by hash)
    /// into full `SearchResult` records with doc_type, path, etc.
    pub fn lookup_documents_by_hash(&self, hash: &str) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, doc_type, path, title, hash \
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
        let mut meta_acc: std::collections::HashMap<i64, (std::time::SystemTime, i64)> =
            std::collections::HashMap::new();
        for hash_chunk in hashes.chunks(CHUNK) {
            let placeholders = std::iter::repeat_n("?", hash_chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id, doc_type, path, title, hash, mtime, size \
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
                    let mtime: i64 = row.get(5)?;
                    let size: i64 = row.get(6)?;
                    Ok((doc, mtime, size))
                })
                .map_err(sqlite_err)?;
            for row in rows.flatten() {
                let (d, mtime, size) = row;
                meta_acc.insert(d.id, (crate::storage::nanos_to_systime(mtime), size));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::test_helpers::{insert_doc, open_temp_search, setup_db};

    // -- resolve_ref free fn -----------------------------------------------

    #[test]
    fn resolve_ref_by_hash_prefix() {
        let conn = setup_db();
        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "The borrow checker enforces ownership rules at compile time.",
        );
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
            "The borrow checker enforces ownership rules at compile time.",
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
            "The borrow checker enforces ownership rules at compile time.",
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
            "The borrow checker enforces ownership rules at compile time.",
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

    // -- Db lookup methods --------------------------------------------------

    #[test]
    fn generate_index_from_db() {
        let (_dir, search) = open_temp_search();
        search
            .index_page(Path::new("wiki/rust-borrow.md"), "Rust Borrow Checker", "The borrow checker enforces ownership rules at compile time.", 1000)
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
            .index_page(Path::new("wiki/my-page.md"), "My Page", "Body.", 1000)
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
            .index_page(Path::new("wiki/my-page.md"), "My Page Title", "Body.", 1000)
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
            .index_page(Path::new("wiki/alpha.md"), "Alpha", "Alpha body.", 1000)
            .unwrap();
        search
            .index_page(Path::new("wiki/beta.md"), "Beta", "Beta body.", 1000)
            .unwrap();
        assert_eq!(search.page_count().unwrap(), 2);
    }

    #[test]
    fn get_last_modified_resolves() {
        let (_dir, search) = open_temp_search();
        search
            .index_page(Path::new("wiki/alpha.md"), "Alpha", "Body.", 1000)
            .unwrap();
        let ts = search.get_last_modified(Path::new("wiki/alpha.md")).unwrap();
        assert_eq!(ts, Some(1000i64));
        let missing = search.get_last_modified(Path::new("wiki/missing.md")).unwrap();
        assert_eq!(missing, None);
    }

    #[test]
    fn all_stems_and_titles_lists_all() {
        let (_dir, search) = open_temp_search();
        search
            .index_page(Path::new("wiki/alpha.md"), "Alpha Topic", "Body.", 1000)
            .unwrap();
        search
            .index_page(Path::new("wiki/beta.md"), "Beta Topic", "Body.", 1000)
            .unwrap();
        let pairs = search.all_stems_and_titles().unwrap();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("alpha".to_string(), "Alpha Topic".to_string())));
        assert!(pairs.contains(&("beta".to_string(), "Beta Topic".to_string())));
    }

    /// `lookup_documents_with_collections` returns the per-doc-id
    /// (mtime, size) map that `vector_search_as_results` uses for the
    /// stat-check. Without this map the stat-check can't skip stale
    /// results.
    #[test]
    fn lookup_documents_with_collections_returns_meta_per_id() {
        let (_dir, search) = open_temp_search();
        let body = "page body";
        let id = search
            .index_page(std::path::Path::new("wiki/m.md"), "M", body, 6000)
            .unwrap();
        let hash = crate::storage::content_hash(body.as_bytes());
        let (_docs, _memberships, meta_by_id) = search
            .lookup_documents_with_collections(&[hash])
            .unwrap();
        let (mtime, size) = meta_by_id
            .get(&id)
            .expect("meta should be present for the inserted doc");
        assert_eq!(*mtime, std::time::UNIX_EPOCH + std::time::Duration::from_nanos(6000));
        assert_eq!(*size, body.len() as i64);
    }
}
