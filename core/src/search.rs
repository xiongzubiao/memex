use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

use crate::error::{MemexError, Result};
use crate::types::Document;

/// Minimum relevance score threshold.  Currently 0.0 (no filtering) per spec:
/// "No minScore filtering (QMD's `hybridQuery` default is 0)."
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
/// embed without recomputing — `insert_content` already hashed each body.
#[derive(Debug, Clone)]
pub struct IngestBatchResult {
    pub source_hash: String,
    pub source_docid: String,
    pub wiki_hashes: Vec<(String, String)>,
}

/// A single search result from the BM25 full-text index.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: PathBuf,
    pub title: String,
    /// Relevance score normalized to 0.0-1.0 range.
    pub score: f32,
    /// A short excerpt from the page body highlighting the matched term.
    pub snippet: String,
    /// Document type (wiki or source).
    pub doc_type: String,
    /// Short document identifier.
    pub docid: String,
    /// Full SHA-256 content hash. Key into the `content` table for the
    /// authoritative body text; callers read content through this rather
    /// than re-opening files on disk.
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

/// Strong signal detection: s1 >= 0.85 AND (s1 - s2) >= 0.15
///
/// If there's only one result (s2 = 0.0), a high s1 is still strong.
pub fn is_strong_signal(s1: f64, s2: f64) -> bool {
    s1 >= 0.85 && (s1 - s2) >= 0.15
}

/// Insert or update a document on a raw connection. Used by both
/// `upsert_document` (mutex-guarded) and `store_ingest_batch` (transactional).
/// On conflict, preserves created_at and docid.
#[allow(clippy::too_many_arguments)]
fn upsert_document_conn(
    conn: &rusqlite::Connection,
    doc_type: &str,
    path: &str,
    title: &str,
    hash: &str,
    docid: &str,
    tags: &str,
    summary: &str,
    created_at: &str,
    updated_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO documents (doc_type, path, title, hash, docid, tags, summary, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(doc_type, path) DO UPDATE SET \
             title = excluded.title, \
             hash = excluded.hash, \
             tags = excluded.tags, \
             summary = excluded.summary, \
             updated_at = excluded.updated_at",
        rusqlite::params![doc_type, path, title, hash, docid, tags, summary, created_at, updated_at],
        )
    .map_err(sqlite_err)?;
    let document_id = document_id_by_path(conn, doc_type, path)?;
    ensure_default_document_collection(conn, document_id)?;
    Ok(())
}

/// BM25 search filtered by doc_type.
///
/// Returns results from the `documents_fts` virtual table joined with `documents`,
/// filtered by doc_type. BM25 column weights differ by doc_type:
/// - wiki: path=1.5, title=4.0, tags=1.5, body=1.0
/// - source: path=1.5, title=4.0, tags=0.0, body=1.0
pub fn search_bm25(
    conn: &rusqlite::Connection,
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

    search_bm25_raw(conn, &sanitized, doc_type, limit)
}

/// BM25 search filtered by doc_type and one or more collections.
fn search_bm25_in_collections(
    conn: &rusqlite::Connection,
    query: &str,
    doc_type: &str,
    limit: usize,
    collections: &[String],
) -> Result<Vec<SearchResult>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    let sanitized = sanitize_query(query);
    if sanitized.is_empty() {
        return Ok(Vec::new());
    }

    let collections = normalize_collections(collections);
    search_bm25_raw_in_collections(conn, &sanitized, doc_type, limit, &collections)
}

/// Inner BM25 search with a pre-sanitized FTS5 MATCH expression.
fn search_bm25_raw(
    conn: &rusqlite::Connection,
    match_expr: &str,
    doc_type: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let bm25_expr = if doc_type == "source" {
        "bm25(documents_fts, 1.5, 4.0, 0.0, 1.0)"
    } else {
        "bm25(documents_fts, 1.5, 4.0, 1.5, 1.0)"
    };

    let sql = format!(
        "SELECT d.id, d.docid, d.doc_type, d.path, d.title, \
                {bm25_expr} as score, d.summary, d.hash \
         FROM documents_fts f \
         JOIN documents d ON d.id = f.rowid \
         WHERE documents_fts MATCH ?1 AND d.doc_type = ?2 \
         ORDER BY score \
         LIMIT ?3"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };

    let rows: Vec<(String, String, String, String, f64, String, String)> = match stmt.query_map(
        rusqlite::params![match_expr, doc_type, limit as i64],
        |row| {
            Ok((
                row.get::<_, String>(1)?, // docid
                row.get::<_, String>(2)?, // doc_type
                row.get::<_, String>(3)?, // path
                row.get::<_, String>(4)?, // title
                row.get::<_, f64>(5)?,    // raw score
                row.get::<_, String>(6)?, // summary
                row.get::<_, String>(7)?, // hash
            ))
        },
    ) {
        Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
        Err(_) => return Ok(Vec::new()),
    };

    let results = rows
        .into_iter()
        .map(|(docid, coll, path, title, raw, summary, hash)| {
            let score = normalize_bm25(raw) as f32;
            SearchResult {
                path: PathBuf::from(&path),
                title,
                score,
                snippet: summary,
                doc_type: coll,
                docid,
                hash,
            }
        })
        .collect();

    Ok(results)
}

/// Inner BM25 search restricted to a set of collection names.
fn search_bm25_raw_in_collections(
    conn: &rusqlite::Connection,
    match_expr: &str,
    doc_type: &str,
    limit: usize,
    collections: &[String],
) -> Result<Vec<SearchResult>> {
    let bm25_expr = if doc_type == "source" {
        "bm25(documents_fts, 1.5, 4.0, 0.0, 1.0)"
    } else {
        "bm25(documents_fts, 1.5, 4.0, 1.5, 1.0)"
    };
    let placeholders = std::iter::repeat("?")
        .take(collections.len())
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(
        "SELECT DISTINCT d.id, d.docid, d.doc_type, d.path, d.title, \
                {bm25_expr} as score, d.summary, d.hash \
         FROM documents_fts f \
         JOIN documents d ON d.id = f.rowid \
         JOIN document_collections dc ON dc.document_id = d.id \
         JOIN collections c ON c.id = dc.collection_id \
         WHERE documents_fts MATCH ? AND d.doc_type = ? AND c.name IN ({placeholders}) \
         ORDER BY score \
         LIMIT ?"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };

    let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(3 + collections.len());
    let limit = limit as i64;
    params.push(&match_expr);
    params.push(&doc_type);
    for name in collections {
        params.push(name);
    }
    params.push(&limit);

    let rows: Vec<(String, String, String, String, f64, String, String)> =
        match stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((
                row.get::<_, String>(1)?, // docid
                row.get::<_, String>(2)?, // doc_type
                row.get::<_, String>(3)?, // path
                row.get::<_, String>(4)?, // title
                row.get::<_, f64>(5)?,    // raw score
                row.get::<_, String>(6)?, // summary
                row.get::<_, String>(7)?, // hash
            ))
        }) {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => return Ok(Vec::new()),
        };

    let results = rows
        .into_iter()
        .map(|(docid, coll, path, title, raw, summary, hash)| {
            let score = normalize_bm25(raw) as f32;
            SearchResult {
                path: PathBuf::from(&path),
                title,
                score,
                snippet: summary,
                doc_type: coll,
                docid,
                hash,
            }
        })
        .collect();

    Ok(results)
}

/// Sanitize a user query for FTS5 MATCH (following QMD pattern).
///
/// Rules:
/// - Quoted phrases preserved: `"exact match"` → FTS5 phrase
/// - Negation: `-term` → NOT clause
/// - Hyphens: `multi-agent` → `"multi agent"` (phrase)
/// - Bare words: `term` → `"term"*` (prefix match)
/// - Positives AND-joined
pub fn sanitize_query(query: &str) -> String {
    let query = query.trim();
    if query.is_empty() {
        return String::new();
    }

    let mut positives: Vec<String> = Vec::new();
    let mut negatives: Vec<String> = Vec::new();

    let mut chars = query.chars().peekable();
    while chars.peek().is_some() {
        // Skip whitespace
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }

        if chars.peek().is_none() {
            break;
        }

        // Quoted phrase: preserve as-is
        if chars.peek() == Some(&'"') {
            chars.next(); // consume opening quote
            let mut phrase = String::new();
            while let Some(&c) = chars.peek() {
                if c == '"' {
                    chars.next(); // consume closing quote
                    break;
                }
                phrase.push(c);
                chars.next();
            }
            if !phrase.is_empty() {
                // Clean the phrase: only keep alphanumeric, whitespace, underscore
                let cleaned: String = phrase
                    .chars()
                    .map(|c| {
                        if c.is_alphanumeric() || c == '_' || c.is_whitespace() {
                            c
                        } else {
                            ' '
                        }
                    })
                    .collect();
                let cleaned = cleaned.trim().to_string();
                if !cleaned.is_empty() {
                    positives.push(format!("\"{cleaned}\""));
                }
            }
            continue;
        }

        // Collect a word (non-whitespace token)
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            if c == '"' {
                break; // let the quoted-phrase branch handle it
            }
            word.push(c);
            chars.next();
        }

        if word.is_empty() {
            continue;
        }

        // Negation: -term → NOT clause
        if let Some(rest) = word.strip_prefix('-') {
            let clean: String = rest
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .to_lowercase();
            if !clean.is_empty() {
                negatives.push(format!("NOT \"{clean}\""));
            }
            continue;
        }

        // Hyphenated word: multi-agent → "multi agent" (phrase)
        if word.contains('-') {
            let parts: Vec<String> = word
                .split('-')
                .map(|p| {
                    p.chars()
                        .filter(|c| c.is_alphanumeric() || *c == '_')
                        .collect::<String>()
                        .to_lowercase()
                })
                .filter(|p| !p.is_empty())
                .collect();
            if !parts.is_empty() {
                let phrase = parts.join(" ");
                positives.push(format!("\"{phrase}\""));
            }
            continue;
        }

        // Bare word: term → "term"* (prefix match)
        let clean: String = word
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
            .to_lowercase();
        if !clean.is_empty() {
            positives.push(format!("\"{clean}\"*"));
        }
    }

    if positives.is_empty() && negatives.is_empty() {
        return String::new();
    }

    let mut result = positives.join(" AND ");
    for neg in &negatives {
        if result.is_empty() {
            // Can't have only negations in FTS5
            return String::new();
        }
        result.push_str(&format!(" {neg}"));
    }
    result
}

/// RRF fusion: merge multiple ranked search result lists.
///
/// - `weights` is parallel to `lists` — `weights[i]` is the multiplier
///   for list `i`. Missing entries default to 1.0. Compose weights from
///   independent trust axes (e.g. wiki-doc_type × primary-probe).
/// - Formula: score(d) = sum_i(w_i / (k + rank_i(d))) + bonus(rank_i(d))
/// - Bonus: +0.05 for rank 1, +0.02 for ranks 2-3 (1-based)
/// - Post-fusion: reassign scores as 1/rank
pub fn rrf_fuse(lists: &[Vec<SearchResult>], weights: &[f32], k: u32) -> Vec<SearchResult> {
    // Single list: no fusion needed, just assign 1/rank scores directly.
    if lists.len() == 1 {
        return lists[0]
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut r = r.clone();
                r.score = 1.0 / (i as f32 + 1.0);
                r
            })
            .collect();
    }

    let k_f = k as f32;

    // Accumulate scores per document path
    let mut scores: std::collections::HashMap<
        String,                     // path as string key
        (SearchResult, f32, usize), // (best result, rrf_score, best_rank_1based)
    > = std::collections::HashMap::new();

    for (list_idx, list) in lists.iter().enumerate() {
        let weight: f32 = weights.get(list_idx).copied().unwrap_or(1.0);

        for (rank_0based, result) in list.iter().enumerate() {
            let rank_1based = rank_0based + 1;
            let contribution = weight / (k_f + rank_1based as f32);

            // Bonus for top ranks (1-based)
            let bonus = if rank_1based == 1 {
                0.05_f32
            } else if rank_1based <= 3 {
                0.02_f32
            } else {
                0.0
            };

            let path_key = result.path.to_string_lossy().to_string();

            scores
                .entry(path_key)
                .and_modify(|(existing, rrf_score, best_rank)| {
                    *rrf_score += contribution + bonus;
                    if rank_1based < *best_rank {
                        *best_rank = rank_1based;
                        *existing = result.clone();
                    }
                })
                .or_insert_with(|| (result.clone(), contribution + bonus, rank_1based));
        }
    }

    // Sort by RRF score descending, then assign 1/rank as final score
    let mut sorted: Vec<(SearchResult, f32)> = scores
        .into_values()
        .map(|(result, rrf_score, _)| (result, rrf_score))
        .collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    sorted
        .into_iter()
        .enumerate()
        .map(|(i, (mut result, _))| {
            result.score = 1.0 / (i as f32 + 1.0);
            result
        })
        .collect()
}

/// Construct a `Document` from a SQLite row with the standard 10-column SELECT.
/// Expects columns: id, doc_type, path, title, hash, docid, tags, summary,
/// created_at, updated_at.
fn row_to_document(row: &rusqlite::Row) -> rusqlite::Result<Document> {
    Ok(Document {
        id: row.get(0)?,
        doc_type: row.get(1)?,
        path: row.get(2)?,
        title: row.get(3)?,
        hash: row.get(4)?,
        docid: row.get(5)?,
        tags: row.get(6)?,
        summary: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
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
/// 1. Docid prefix: `WHERE docid LIKE ?1 || '%'`
/// 2. Stem: match path basename without extension
/// 3. Title: `WHERE title = ?1 COLLATE NOCASE`
pub fn resolve_ref(conn: &rusqlite::Connection, reference: &str) -> Result<Vec<Document>> {
    // Tier 1: Docid prefix match
    let mut stmt = conn.prepare(
        "SELECT id, doc_type, path, title, hash, docid, tags, summary, created_at, updated_at \
         FROM documents WHERE docid LIKE ?1 || '%'",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![reference], |row| row_to_document(row))?
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
        "SELECT id, doc_type, path, title, hash, docid, tags, summary, created_at, updated_at \
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
        "SELECT id, doc_type, path, title, hash, docid, tags, summary, created_at, updated_at \
         FROM documents WHERE title = ?1 COLLATE NOCASE",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![reference], |row| row_to_document(row))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(docs)
}

// ---------------------------------------------------------------------------
// Bm25Search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobType {
    Transcript,
    Document,
}

impl JobType {
    pub fn as_str(self) -> &'static str {
        match self {
            JobType::Transcript => "transcript",
            JobType::Document => "document",
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "transcript" => Some(JobType::Transcript),
            "document" => Some(JobType::Document),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingJob {
    pub job_id: String,
    pub job_type: JobType,
    pub source_path: String,
    pub agent: Option<String>,
    pub content_hash: String,
    pub memex_root: String,
    pub collections: Vec<String>,
}

/// One row of the `memex source list` output. Exposed as a public type so the
/// CLI can format it (or emit JSON via serde).
#[derive(Debug, serde::Serialize)]
pub struct SourceListRow {
    pub docid: String,
    pub path: String,
    pub title: String,
    pub size_bytes: usize,
    pub created_at: String,
    pub updated_at: String,
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

    /// Returns `true` if the top result score >= 0.85 AND the gap to the second
    /// result is >= 0.15.  Useful for deciding whether search alone is sufficient
    /// or an LLM call is needed.
    pub fn is_strong_signal(results: &[SearchResult]) -> bool {
        match results.first() {
            Some(top) if top.score >= 0.85 => match results.get(1) {
                Some(second) => (top.score - second.score) >= 0.15,
                None => true, // single result with high score
            },
            _ => false,
        }
    }

    /// Generate human-readable index from the DB.
    /// Format: `# Index\n\n- [Title](path) -- summary\n...`
    pub fn generate_index(&self) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT path, title, summary FROM documents ORDER BY title")
            .map_err(sqlite_err)?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() {
            return Ok("# Index\n\nNo wiki pages yet.\n".to_string());
        }

        let mut output = String::from("# Index\n\n");
        for (path, title, summary) in &rows {
            output.push_str(&format!("- [{title}]({path}) -- {summary}\n"));
        }
        Ok(output)
    }

    /// Generate compact index for agent consumption.
    /// Format: `docid | summary\n...`
    pub fn generate_compact_index(&self) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT docid, summary FROM documents WHERE docid != '' ORDER BY docid")
            .map_err(sqlite_err)?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();

        let mut output = String::new();
        for (docid, summary) in &rows {
            output.push_str(&format!("{docid} | {summary}\n"));
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

    /// Look up a page path by docid.
    pub fn lookup_docid(&self, docid: &str) -> Result<Option<PathBuf>> {
        self.query_single_string("SELECT path FROM documents WHERE docid = ?1", docid)
            .map(|opt| opt.map(PathBuf::from))
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

    /// Resolve a reference that could be a docid, filename stem, or title.
    /// Tries docid first, then stem, then title.
    pub fn resolve_ref(&self, reference: &str) -> Result<Option<PathBuf>> {
        if let Some(path) = self.lookup_docid(reference)? {
            return Ok(Some(path));
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

    /// Get the docid for a page by its path.
    pub fn lookup_path_docid(&self, path: &Path) -> Result<Option<String>> {
        let path_str = path.to_string_lossy();
        self.query_single_string("SELECT docid FROM documents WHERE path = ?1", &path_str)
            .map(|opt| opt.filter(|s| !s.is_empty()))
    }

    /// Get the docid for a source document by its content hash. Used by
    /// `handle_source_add` to dedup repeated identical content.
    pub fn lookup_source_docid_by_hash(&self, hash: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT docid FROM documents WHERE doc_type = 'source' AND hash = ?1 LIMIT 1")
            .map_err(sqlite_err)?;
        let docid: Option<String> = stmt
            .query_row(rusqlite::params![hash], |row| row.get(0))
            .optional()
            .map_err(sqlite_err)?;
        Ok(docid)
    }

    /// Look up a source document by its source path. Used by
    /// `handle_source_delete` to resolve the `path:<source-path>` ref form.
    pub fn lookup_source_by_path(
        &self,
        source_path: &str,
    ) -> Result<Option<crate::types::Document>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, doc_type, path, title, hash, docid, tags, summary, created_at, updated_at \
                 FROM documents WHERE doc_type = 'source' AND path = ?1 LIMIT 1",
            )
            .map_err(sqlite_err)?;
        let doc = stmt
            .query_row(rusqlite::params![source_path], |row| {
                Ok(crate::types::Document {
                    id: row.get(0)?,
                    doc_type: row.get(1)?,
                    path: row.get(2)?,
                    title: row.get(3)?,
                    hash: row.get(4)?,
                    docid: row.get(5)?,
                    tags: row.get(6)?,
                    summary: row.get(7)?,
                    created_at: row.get(8)?,
                    updated_at: row.get(9)?,
                })
            })
            .optional()
            .map_err(sqlite_err)?;
        Ok(doc)
    }

    /// Return wiki page slugs whose frontmatter `sources:` field contains
    /// `source_path`. Implementation: scan all wiki page rows, parse each
    /// frontmatter as a generic YAML map (so we tolerate pages missing the
    /// strict required `created_at`/`updated_at` fields), and check the
    /// sources list. The row scan is simple and small (wiki page counts
    /// are typically < 1000).
    pub fn wiki_pages_referencing_source(&self, source_path: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT d.path, c.doc FROM documents d \
                 JOIN content c ON c.hash = d.hash \
                 WHERE d.doc_type = 'wiki'",
            )
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let body: String = row.get(1)?;
                Ok((path, body))
            })
            .map_err(sqlite_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (path, body) = row.map_err(sqlite_err)?;
            if frontmatter_lists_source(&body, source_path) {
                // Wiki path is `wiki/<slug>.md`; extract slug.
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

    /// Get the last_modified timestamp for a page by its path.
    pub fn get_last_modified(&self, path: &Path) -> Result<Option<String>> {
        let path_str = path.to_string_lossy();
        self.query_single_string(
            "SELECT updated_at FROM documents WHERE path = ?1",
            &path_str,
        )
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

    /// Insert content into the content-addressable store. Returns the SHA-256 hash.
    /// Delegates to `content::insert_content`.
    pub fn insert_content(&self, doc: &str) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        crate::content::insert_content(&conn, doc)
    }

    /// Check if a content hash already exists in the content table.
    pub fn content_exists(&self, hash: &str) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM content WHERE hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        Ok(count > 0)
    }

    /// Insert or update a document row in the documents table.
    ///
    /// On conflict (same doc_type + path), updates title, hash, tags,
    /// summary, and updated_at. Preserves created_at and docid.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_document(
        &self,
        doc_type: &str,
        path: &str,
        title: &str,
        hash: &str,
        docid: &str,
        tags: &str,
        summary: &str,
        created_at: &str,
        updated_at: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        upsert_document_conn(
            &conn, doc_type, path, title, hash, docid, tags, summary, created_at, updated_at,
        )
    }

    /// Store a source document and its derived wiki pages in a single
    /// transaction. Returns (source_content_hash, source_docid, wiki_slugs).
    /// Wiki file writes happen after COMMIT, not inside the transaction,
    /// because filesystem writes can't be rolled back.
    pub fn store_ingest_batch(
        &self,
        source_text: &str,
        source_path: &str,
        source_title: &str,
        source_summary: &str,
        wiki_pages: &[IngestWikiPage],
        collections: &[String],
        now: &str,
    ) -> Result<IngestBatchResult> {
        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let tx = conn.transaction().map_err(sqlite_err)?;

        let result = (|| -> Result<IngestBatchResult> {
            let old_source_hash: Option<String> = tx
                .query_row(
                    "SELECT hash FROM documents WHERE doc_type = 'source' AND path = ?1",
                    rusqlite::params![source_path],
                    |row| row.get(0),
                )
                .ok();
            let source_hash = crate::content::insert_content(&tx, source_text)?;
            let existing: Vec<String> = {
                let mut stmt = tx
                    .prepare("SELECT docid FROM documents WHERE docid != ''")
                    .map_err(sqlite_err)?;
                stmt.query_map([], |row| row.get::<_, String>(0))
                    .map_err(sqlite_err)?
                    .filter_map(|r| r.ok())
                    .collect()
            };
            let source_docid =
                crate::docid::allocate_docid(&source_hash, "source", source_path, &existing);
            upsert_document_conn(
                &tx,
                "source",
                source_path,
                source_title,
                &source_hash,
                &source_docid,
                "",
                source_summary,
                now,
                now,
            )?;
            set_document_collections_in_conn(&tx, "source", source_path, collections)?;
            if let Some(old_hash) = old_source_hash
                && old_hash != source_hash
            {
                crate::content::cleanup_orphaned_content(&tx, &old_hash)?;
            }

            let mut wiki_hashes = Vec::with_capacity(wiki_pages.len());
            let mut all_docids = existing;
            for page in wiki_pages {
                let old_page_hash: Option<String> = tx
                    .query_row(
                        "SELECT hash FROM documents WHERE doc_type = 'wiki' AND path = ?1",
                        rusqlite::params![page.slug],
                        |row| row.get(0),
                    )
                    .ok();
                let page_hash = crate::content::insert_content(&tx, &page.content)?;
                let page_docid =
                    crate::docid::allocate_docid(&page_hash, "wiki", &page.slug, &all_docids);
                upsert_document_conn(
                    &tx,
                    "wiki",
                    &page.slug,
                    &page.title,
                    &page_hash,
                    &page_docid,
                    &page.tags,
                    "",
                    now,
                    now,
                )?;
                set_document_collections_in_conn(&tx, "wiki", &page.slug, collections)?;
                if let Some(old_hash) = old_page_hash
                    && old_hash != page_hash
                {
                    crate::content::cleanup_orphaned_content(&tx, &old_hash)?;
                }
                all_docids.push(page_docid);
                wiki_hashes.push((page.slug.clone(), page_hash));
            }

            Ok(IngestBatchResult {
                source_hash,
                source_docid,
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

    /// Clean up orphaned content (not referenced by any document).
    pub fn cleanup_orphaned_content(&self, old_hash: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        crate::content::cleanup_orphaned_content(&conn, old_hash)
    }

    /// Three-tier identifier resolution returning full `Document` records.
    ///
    /// Tries docid prefix, then stem, then title (case-insensitive).
    pub fn resolve_ref_documents(&self, reference: &str) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        resolve_ref(&conn, reference)
    }

    /// Get document text by content hash from the content-addressable store.
    pub fn get_content(&self, hash: &str) -> Result<String> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        crate::content::get_content(&conn, hash)
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

    // -----------------------------------------------------------------------
    // Ingest job tracking
    // -----------------------------------------------------------------------

    pub fn insert_ingest_job(
        &self,
        job_id: &str,
        job_type: JobType,
        source_path: &str,
        agent: Option<&str>,
        content_hash: &str,
        memex_root: &str,
        collections: &[String],
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let now = now_rfc3339();
        let collections_json = serde_json::to_string(collections)
            .map_err(|e| crate::error::MemexError::Internal(format!("collections json: {e}")))?;
        conn.execute(
            "INSERT OR IGNORE INTO ingest_jobs \
             (job_id, job_type, source_path, agent, content_hash, memex_root, collections, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9)",
            rusqlite::params![
                job_id,
                job_type.as_str(),
                source_path,
                agent,
                content_hash,
                memex_root,
                collections_json,
                &now,
                &now
            ],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    pub fn update_ingest_job_status(
        &self,
        job_id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE ingest_jobs SET status = ?1, updated_at = ?2, error = ?3 WHERE job_id = ?4",
            rusqlite::params![status, &now, error, job_id],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    pub fn pending_ingest_jobs(&self) -> Result<Vec<PendingJob>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn.prepare(
            "SELECT job_id, job_type, source_path, agent, content_hash, memex_root, collections \
             FROM ingest_jobs WHERE status IN ('pending', 'processing')",
        )
        .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| {
                let job_type_s: String = row.get(1)?;
                let collections_s: String = row.get(6)?;
                Ok((
                    row.get::<_, String>(0)?,
                    job_type_s,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    collections_s,
                ))
            })
            .map_err(sqlite_err)?;
        let mut jobs = Vec::new();
        for row in rows {
            let (job_id, job_type_s, source_path, agent, content_hash, memex_root, collections_s) =
                row.map_err(sqlite_err)?;
            let job_type = JobType::from_str(&job_type_s)
                .ok_or_else(|| crate::error::MemexError::Internal(format!("bad job_type: {job_type_s}")))?;
            let collections: Vec<String> = serde_json::from_str(&collections_s).unwrap_or_default();
            jobs.push(PendingJob {
                job_id,
                job_type,
                source_path,
                agent,
                content_hash,
                memex_root,
                collections,
            });
        }
        Ok(jobs)
    }

    pub fn source_count(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE doc_type = 'source'",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        Ok(count as usize)
    }

    /// List source documents (doc_type = 'source'). When `collection_filter`
    /// is non-empty, only sources belonging to one of the named collections
    /// are returned. Sorted by `updated_at DESC`.
    pub fn list_sources(&self, collection_filter: &[String]) -> Result<Vec<SourceListRow>> {
        let sql = "SELECT d.docid, d.path, d.title, LENGTH(c.doc) AS size, d.created_at, d.updated_at \
                   FROM documents d JOIN content c ON c.hash = d.hash \
                   WHERE d.doc_type = 'source' \
                   ORDER BY d.updated_at DESC";
        let rows = {
            let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
            let mut stmt = conn.prepare(sql).map_err(sqlite_err)?;
            let iter = stmt
                .query_map([], |row| {
                    Ok(SourceListRow {
                        docid: row.get(0)?,
                        path: row.get(1)?,
                        title: row.get(2)?,
                        size_bytes: row.get::<_, i64>(3)? as usize,
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                        collections: Vec::new(),
                    })
                })
                .map_err(sqlite_err)?;
            let mut out: Vec<SourceListRow> = Vec::new();
            for r in iter {
                out.push(r.map_err(sqlite_err)?);
            }
            out
        };
        // Drop the lock before `document_collections_by_path`, which re-locks.
        let mut filtered = Vec::with_capacity(rows.len());
        for mut row in rows {
            row.collections = self
                .document_collections_by_path("source", &row.path)
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
                "SELECT id, doc_type, path, title, hash, docid, tags, summary, created_at, updated_at \
                 FROM documents WHERE doc_type = 'wiki'"
            )
            .map_err(sqlite_err)?;
        let docs: Vec<Document> = stmt
            .query_map([], row_to_document)
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(docs)
    }

    /// Delete a document by path within a transaction, then clean up orphaned content.
    ///
    /// Steps: fetch old hash, delete documents row (FTS trigger fires), orphan-clean old hash.
    pub fn delete_document_with_cleanup(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        // Fetch old content hash before deletion.
        let old_hash: Option<String> = conn
            .query_row(
                "SELECT hash FROM documents WHERE path = ?1",
                rusqlite::params![path],
                |row| row.get(0),
            )
            .ok();
        // Delete the documents row (FTS delete trigger fires automatically).
        conn.execute(
            "DELETE FROM documents WHERE path = ?1",
            rusqlite::params![path],
        )
        .map_err(sqlite_err)?;
        // Clean up orphaned content if hash is no longer referenced.
        if let Some(hash) = old_hash {
            crate::content::cleanup_orphaned_content(&conn, &hash)?;
        }
        Ok(())
    }

    /// Re-index a wiki page from disk content.
    ///
    /// Used by lint --fix to update stale index entries.
    /// Mutation ordering: insert new content -> update documents row -> orphan-clean old hash.
    pub fn reindex_page_from_content(
        &self,
        path: &str,
        content: &str,
        old_hash: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        // Insert new content.
        let new_hash = crate::content::insert_content(&conn, content)?;
        // Preserve frontmatter timestamps (spec: lint --fix doesn't bump timestamps).
        let fm_timestamps = crate::validate::parse_frontmatter(content)
            .ok()
            .map(|(fm, _)| {
                (
                    fm.created_at
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    fm.updated_at
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                )
            });
        // Update title, tags, summary, hash from re-parsed content.
        if let Some((title, _body, tags, summary, collections)) = parse_page_for_indexing(content) {
            if let Some((created, updated)) = &fm_timestamps {
                conn.execute(
                    "UPDATE documents SET hash = ?1, title = ?2, tags = ?3, summary = ?4, \
                     created_at = ?5, updated_at = ?6 WHERE path = ?7",
                    rusqlite::params![new_hash, title, tags, summary, created, updated, path],
                )
                .map_err(sqlite_err)?;
            } else {
                conn.execute(
                    "UPDATE documents SET hash = ?1, title = ?2, tags = ?3, summary = ?4 WHERE path = ?5",
                    rusqlite::params![new_hash, title, tags, summary, path],
                )
                .map_err(sqlite_err)?;
            }
            set_document_collections_in_conn(&conn, "wiki", path, &collections)?;
        } else {
            conn.execute(
                "UPDATE documents SET hash = ?1 WHERE path = ?2",
                rusqlite::params![new_hash, path],
            )
            .map_err(sqlite_err)?;
        }
        // Orphan-clean old hash.
        if old_hash != new_hash {
            crate::content::cleanup_orphaned_content(&conn, old_hash)?;
        }
        Ok(())
    }

    /// Return distinct `(model, count)` pairs for chunks whose model differs
    /// from `current_model`. Used by lint to detect outdated embeddings.
    pub fn outdated_chunk_models(&self, current_model: &str) -> Result<Vec<(String, usize)>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT model, COUNT(*) FROM chunks WHERE model != ?1 GROUP BY model")
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

    /// Return distinct content hashes that have chunks with a model other than
    /// `current_model`. Used by lint --fix to identify documents needing re-embedding.
    pub fn outdated_chunk_hashes(&self, current_model: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT DISTINCT hash FROM chunks WHERE model != ?1")
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
        search_bm25_in_collections(&conn, query, doc_type, limit, collections)
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
        search_bm25_raw(&conn, &title_query, doc_type, limit)
    }

    /// Look up document metadata by content hash.
    ///
    /// Returns all documents that reference the given content hash.
    /// Used by vector search to convert chunk-level results (keyed by hash)
    /// into full `SearchResult` records with docid, doc_type, path, etc.
    pub fn lookup_documents_by_hash(&self, hash: &str) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, doc_type, path, title, hash, docid, tags, summary, created_at, updated_at \
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

    #[allow(clippy::too_many_arguments)]
    pub fn index_page(
        &self,
        path: &Path,
        title: &str,
        body: &str,
        tags: &str,
        docid: &str,
        summary: &str,
        last_modified: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let path_str = path.to_string_lossy();

        // Determine doc_type from path prefix
        let doc_type = if path_str.starts_with("source") {
            "source"
        } else {
            "wiki"
        };

        // Build full document content with frontmatter for content-addressable storage
        let doc = format!("---\ntitle: {title}\ntags: {tags}\n---\n\n{body}\n");
        let hash = crate::storage::content_hash(doc.as_bytes());
        let now = last_modified;

        // Insert content
        conn.execute(
            "INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![hash, doc, now],
        )
        .map_err(sqlite_err)?;

        // Upsert document
        conn.execute(
            "INSERT INTO documents (doc_type, path, title, hash, docid, tags, summary, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(doc_type, path) DO UPDATE SET \
                 title = excluded.title, \
                 hash = excluded.hash, \
                 docid = excluded.docid, \
                 tags = excluded.tags, \
                 summary = excluded.summary, \
                 updated_at = excluded.updated_at",
            rusqlite::params![
                doc_type,
                path_str.as_ref(),
                title,
                hash,
                docid,
                tags,
                summary,
                now,
                now
            ],
        )
        .map_err(sqlite_err)?;
        let document_id = document_id_by_path(&conn, doc_type, path_str.as_ref())?;
        ensure_default_document_collection(&conn, document_id)?;
        Ok(())
    }

    pub fn remove_page(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        conn.execute(
            "DELETE FROM documents WHERE path = ?1",
            rusqlite::params![path],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    pub fn existing_docids(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT docid FROM documents WHERE docid != ''")
            .map_err(sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_err)?;
        let mut set = std::collections::HashSet::new();
        for row in rows {
            set.insert(row.map_err(sqlite_err)?);
        }
        Ok(set)
    }

    pub fn rebuild(&self, root: &Path) -> Result<()> {
        use crate::docid;

        let mut conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        conn.execute("DELETE FROM documents", [])
            .map_err(sqlite_err)?;
        conn.execute("DELETE FROM content", [])
            .map_err(sqlite_err)?;

        let wiki_dir = root.join("wiki");
        if !wiki_dir.is_dir() {
            return Ok(());
        }

        // Collect all pages to index before inserting.
        // Compute content hash in the first pass so docid uses content hash (not stem).
        let mut existing_docids: Vec<String> = Vec::new();
        #[allow(clippy::type_complexity)]
        let mut pages_to_index: Vec<(
            String,      // path
            String,      // title
            String,      // body
            String,      // tags
            Vec<String>, // collections
            String,      // docid
            String,      // summary
            String,      // last_modified
            String,      // content
            String,      // content_hash
        )> = Vec::new();

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

            if let Some((title, body, tags, summary, collections)) =
                parse_page_for_indexing(&content)
            {
                let rel_path = abs_path.strip_prefix(root).unwrap_or(abs_path);
                let path_str = rel_path.to_string_lossy().to_string();

                let hash = crate::storage::content_hash(content.as_bytes());
                let did = docid::allocate_docid(&hash, "wiki", &path_str, &existing_docids);
                existing_docids.push(did.clone());

                let last_modified = file_mtime_iso(abs_path);

                pages_to_index.push((
                    path_str,
                    title,
                    body,
                    tags,
                    collections,
                    did,
                    summary,
                    last_modified,
                    content,
                    hash,
                ));
            }
        }

        let tx = conn.transaction().map_err(sqlite_err)?;
        for (path, title, _body, tags, collections, docid, summary, last_modified, content, hash) in
            &pages_to_index
        {
            // Insert content
            tx.execute(
                "INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![hash, content, last_modified],
            )
            .map_err(sqlite_err)?;

            // Insert document
            tx.execute(
                "INSERT INTO documents (doc_type, path, title, hash, docid, tags, summary, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(doc_type, path) DO UPDATE SET \
                     title = excluded.title, \
                     hash = excluded.hash, \
                     docid = excluded.docid, \
                     tags = excluded.tags, \
                     summary = excluded.summary, \
                     updated_at = excluded.updated_at",
                rusqlite::params![
                    "wiki",
                    path,
                    title,
                    hash,
                    docid,
                    tags,
                    summary,
                    last_modified,
                    last_modified,
                ],
            )
            .map_err(sqlite_err)?;
            set_document_collections_in_conn(&tx, "wiki", path, collections)?;
        }
        tx.commit().map_err(sqlite_err)?;

        Ok(())
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

/// Get a file's last modification time as an ISO-8601 string.
///
/// Falls back to an empty string if metadata cannot be read.
fn file_mtime_iso(path: &Path) -> String {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Utc> = t.into();
            dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .unwrap_or_default()
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
        let db_path = dir.path().join(crate::SEARCH_DB_NAME);
        let search = Bm25Search::open(&db_path).unwrap();
        (dir, search)
    }

    /// Helper: a valid wiki page with the given title and body.
    fn make_page(title: &str, body: &str, tags: &[&str]) -> String {
        let tag_yaml = if tags.is_empty() {
            "[]".to_string()
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

    /// Helper: insert a document into the new schema.
    fn insert_doc(
        conn: &rusqlite::Connection,
        doc_type: &str,
        path: &str,
        title: &str,
        body: &str,
        tags: &str,
        docid: &str,
    ) {
        let doc = format!("---\ntitle: {title}\ntags: {tags}\n---\n\n{body}\n");
        let hash = crate::storage::content_hash(doc.as_bytes());
        let now = "2026-04-06T00:00:00Z";
        conn.execute(
            "INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![hash, doc, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (doc_type, path, title, hash, docid, tags, summary, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![doc_type, path, title, hash, docid, tags, "", now, now],
        )
        .unwrap();
    }

    // -----------------------------------------------------------------------
    // New public function tests
    // -----------------------------------------------------------------------

    #[test]
    fn score_normalization() {
        // Strong signal: -10 → 0.909...
        let s = normalize_bm25(-10.0);
        assert!((s - 10.0 / 11.0).abs() < 1e-9, "got {s}");

        // Medium: -2 → 0.667
        let s = normalize_bm25(-2.0);
        assert!((s - 2.0 / 3.0).abs() < 1e-9, "got {s}");

        // Weak: -0.5 → 0.333
        let s = normalize_bm25(-0.5);
        assert!((s - 1.0 / 3.0).abs() < 1e-9, "got {s}");

        // Zero → 0
        assert_eq!(normalize_bm25(0.0), 0.0);

        // Positive scores also work
        let s = normalize_bm25(10.0);
        assert!((s - 10.0 / 11.0).abs() < 1e-9);
    }

    #[test]
    fn signal_detection_strong() {
        // s1=0.91, s2=0.67 → strong (gap=0.24 >= 0.15, s1=0.91 >= 0.85)
        assert!(is_strong_signal(0.91, 0.67));
    }

    #[test]
    fn signal_detection_weak() {
        // s1=0.70, s2=0.60 → weak (s1=0.70 < 0.85)
        assert!(!is_strong_signal(0.70, 0.60));
    }

    #[test]
    fn signal_detection_single_result() {
        // s1=0.91, s2=0.0 → strong (gap=0.91 >= 0.15, s1=0.91 >= 0.85)
        assert!(is_strong_signal(0.91, 0.0));
    }

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
            "ab12cd",
        );

        insert_doc(
            &conn,
            "source",
            "sources/docs/rust-book.md",
            "Rust Programming Language",
            "The Rust programming language borrow checker is described in this book.",
            "reference",
            "ef56gh",
        );

        // Search wiki only
        let wiki_results = search_bm25(&conn, "borrow checker rust", "wiki", 10).unwrap();
        assert!(
            !wiki_results.is_empty(),
            "expected wiki results for 'borrow checker rust'"
        );
        for r in &wiki_results {
            assert_eq!(r.doc_type, "wiki", "expected only wiki results");
        }

        // Search source only
        let source_results = search_bm25(&conn, "borrow checker rust", "source", 10).unwrap();
        assert!(
            !source_results.is_empty(),
            "expected source results for 'borrow checker rust'"
        );
        for r in &source_results {
            assert_eq!(r.doc_type, "source", "expected only source results");
        }
    }

    #[tokio::test]
    async fn search_by_doc_type_filters_by_collections_or_semantics() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/team-a.md"),
                "Alpha Team A",
                "Alpha content for the team-a collection.",
                "alpha",
                "aa11aa",
                "Team A summary",
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
                "bb22bb",
                "Team B summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .set_document_collections_by_path("wiki", "wiki/team-b.md", &["team-b".to_string()])
            .unwrap();

        search
            .index_page(
                Path::new("wiki/excluded.md"),
                "Alpha Excluded",
                "Alpha content for the excluded collection.",
                "alpha",
                "cc33cc",
                "Excluded summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .set_document_collections_by_path("wiki", "wiki/excluded.md", &["research".to_string()])
            .unwrap();

        let results = search
            .search_by_doc_type_in_collections(
                "alpha",
                "wiki",
                10,
                &["team-a".to_string(), "team-b".to_string()],
            )
            .unwrap();

        let paths: std::collections::HashSet<PathBuf> =
            results.into_iter().map(|r| r.path).collect();
        assert!(paths.contains(&PathBuf::from("wiki/team-a.md")));
        assert!(paths.contains(&PathBuf::from("wiki/team-b.md")));
        assert!(!paths.contains(&PathBuf::from("wiki/excluded.md")));
        assert_eq!(paths.len(), 2);
    }

    #[tokio::test]
    async fn search_by_doc_type_in_collections_empty_defaults_to_default() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/default.md"),
                "Default Alpha",
                "Alpha content in the default collection.",
                "alpha",
                "dd44dd",
                "Default summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        search
            .index_page(
                Path::new("wiki/other.md"),
                "Other Alpha",
                "Alpha content in another collection.",
                "alpha",
                "ee55ee",
                "Other summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .set_document_collections_by_path("wiki", "wiki/other.md", &["research".to_string()])
            .unwrap();

        let results = search
            .search_by_doc_type_in_collections(
                "alpha",
                "wiki",
                10,
                &["".to_string(), "   ".to_string()],
            )
            .unwrap();

        let paths: std::collections::HashSet<PathBuf> =
            results.into_iter().map(|r| r.path).collect();
        assert!(paths.contains(&PathBuf::from("wiki/default.md")));
        assert!(!paths.contains(&PathBuf::from("wiki/other.md")));
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn document_collections_default_fallback_for_uninitialized_document() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/alpha.md"),
                "Alpha",
                "Body.",
                "",
                "aaa111",
                "Summary",
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
                "aaa111",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        assert_eq!(
            search
                .document_collections_by_path("wiki", "wiki/alpha.md")
                .unwrap(),
            vec!["default".to_string()]
        );

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
    fn document_collections_persist_default_membership_on_write_path() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/alpha.md"),
                "Alpha",
                "Body.",
                "",
                "aaa111",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let count: i64 = search
            .with_connection(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM document_collections dc \
                     JOIN documents d ON d.id = dc.document_id \
                     JOIN collections c ON c.id = dc.collection_id \
                     WHERE d.doc_type = ?1 AND d.path = ?2 AND c.name = 'default'",
                    rusqlite::params!["wiki", "wiki/alpha.md"],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn store_ingest_batch_assigns_default_when_collections_empty() {
        let (_dir, search) = open_temp_search();
        let now = "2026-04-06T00:00:00Z";
        let wiki_pages = vec![IngestWikiPage {
            slug: "alpha-page".to_string(),
            title: "Alpha Page".to_string(),
            content: "---\ntitle: Alpha Page\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nBody.\n".to_string(),
            tags: String::new(),
        }];

        search
            .store_ingest_batch(
                "source body",
                "/tmp/source.md",
                "Source Title",
                "Source summary",
                &wiki_pages,
                &[],
                now,
            )
            .unwrap();

        assert_eq!(
            search
                .document_collections_by_path("source", "/tmp/source.md")
                .unwrap(),
            vec!["default".to_string()]
        );
        assert_eq!(
            search
                .document_collections_by_path("wiki", "alpha-page")
                .unwrap(),
            vec!["default".to_string()]
        );
    }

    #[test]
    fn document_collections_cascade_delete_membership_rows() {
        let conn = setup_db();

        insert_doc(
            &conn,
            "wiki",
            "wiki/alpha.md",
            "Alpha",
            "Body.",
            "",
            "aaa111",
        );

        let document_id: i64 = conn
            .query_row(
                "SELECT id FROM documents WHERE doc_type = ?1 AND path = ?2",
                rusqlite::params!["wiki", "wiki/alpha.md"],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO collections (name) VALUES ('default')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO document_collections (document_id, collection_id) \
             SELECT ?1, id FROM collections WHERE name = 'default'",
            rusqlite::params![document_id],
        )
        .unwrap();

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
    // sanitize_query tests
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
        // Non-alphanumeric chars should be stripped
        let result = sanitize_query("hello (world)");
        assert_eq!(result, "\"hello\"* AND \"world\"*");
    }

    // -----------------------------------------------------------------------
    // resolve_ref tests
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_ref_by_docid() {
        let conn = setup_db();
        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "Body text.",
            "concept",
            "ab12cd",
        );

        let docs = resolve_ref(&conn, "ab12cd").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "wiki/rust-borrow.md");
    }

    #[test]
    fn resolve_ref_by_docid_prefix() {
        let conn = setup_db();
        insert_doc(
            &conn,
            "wiki",
            "wiki/rust-borrow.md",
            "Rust Borrow Checker",
            "Body text.",
            "concept",
            "ab12cd",
        );

        let docs = resolve_ref(&conn, "ab12").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].docid, "ab12cd");
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
            "ab12cd",
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
            "ab12cd",
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
            "ab12cd",
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
    // rrf_fuse tests
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
                docid: "aaa".to_string(),
                hash: String::new(),
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.5,
                snippet: String::new(),
                doc_type: "wiki".to_string(),
                docid: "bbb".to_string(),
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
            docid: "aaa".to_string(),
            hash: String::new(),
        }];
        let source_list = vec![SearchResult {
            path: PathBuf::from("sources/b.md"),
            title: "B".to_string(),
            score: 0.8,
            snippet: String::new(),
            doc_type: "source".to_string(),
            docid: "bbb".to_string(),
            hash: String::new(),
        }];

        // Wiki list gets 2.0 weight, source 1.0. Wiki result should rank first.
        let fused = rrf_fuse(&[wiki_list, source_list], &[2.0, 1.0], 60);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].path, PathBuf::from("wiki/a.md"));
    }

    // -----------------------------------------------------------------------
    // Bm25Search tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn bm25_index_and_search() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "The borrow checker enforces ownership rules at compile time.",
                "concept, rust",
                "ab12cd",
                "Borrow checker overview",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/python-gc.md"),
                "Python Garbage Collection",
                "Python uses reference counting with a cyclic garbage collector.",
                "concept, python",
                "de34ef",
                "Python GC overview",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let results = search
            .search_by_doc_type("borrow checker rust", "wiki", 10)
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected results for 'borrow checker rust'"
        );
        assert_eq!(
            results[0].path,
            PathBuf::from("wiki/rust-borrow.md"),
            "top result should be the rust borrow page"
        );
        // Score must be in 0.0-1.0
        for r in &results {
            assert!(
                (0.0..=1.0).contains(&r.score),
                "score {} out of range for {}",
                r.score,
                r.title
            );
        }
    }

    #[tokio::test]
    async fn bm25_remove_page() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/ephemeral.md"),
                "Ephemeral Page",
                "This page will be removed shortly.",
                "test",
                "ff0011",
                "Ephemeral page summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        // Verify it's there
        let before = search.search_by_doc_type("ephemeral", "wiki", 10).unwrap();
        assert_eq!(before.len(), 1);

        search.remove_page("wiki/ephemeral.md").unwrap();

        let after = search.search_by_doc_type("ephemeral", "wiki", 10).unwrap();
        assert!(
            after.is_empty(),
            "removed page should not appear in results"
        );
    }

    #[tokio::test]
    async fn bm25_empty_search() {
        let (_dir, search) = open_temp_search();

        let results = search.search_by_doc_type("anything", "wiki", 10).unwrap();
        assert!(results.is_empty(), "empty DB should return no results");
    }

    #[tokio::test]
    async fn bm25_rebuild() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Create wiki directory with pages
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

        let db_path = root.join(crate::SEARCH_DB_NAME);
        let search = Bm25Search::open(&db_path).unwrap();
        search.rebuild(root).unwrap();

        let results = search
            .search_by_doc_type("alpha greek", "wiki", 10)
            .unwrap();
        assert!(!results.is_empty(), "rebuild should index wiki pages");
        assert_eq!(results[0].path, PathBuf::from("wiki/alpha.md"));
    }

    #[test]
    fn is_strong_signal_high_gap() {
        let results = vec![
            SearchResult {
                path: PathBuf::from("wiki/a.md"),
                title: "A".to_string(),
                score: 0.95,
                snippet: String::new(),
                doc_type: "wiki".to_string(),
                docid: "aaa".to_string(),
                hash: String::new(),
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.50,
                snippet: String::new(),
                doc_type: "wiki".to_string(),
                docid: "bbb".to_string(),
                hash: String::new(),
            },
        ];
        assert!(Bm25Search::is_strong_signal(&results));
    }

    #[test]
    fn is_strong_signal_no_gap() {
        let results = vec![
            SearchResult {
                path: PathBuf::from("wiki/a.md"),
                title: "A".to_string(),
                score: 0.90,
                snippet: String::new(),
                doc_type: "wiki".to_string(),
                docid: "aaa".to_string(),
                hash: String::new(),
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.88,
                snippet: String::new(),
                doc_type: "wiki".to_string(),
                docid: "bbb".to_string(),
                hash: String::new(),
            },
        ];
        assert!(!Bm25Search::is_strong_signal(&results));
    }

    #[test]
    fn is_strong_signal_low_score() {
        let results = vec![SearchResult {
            path: PathBuf::from("wiki/a.md"),
            title: "A".to_string(),
            score: 0.50,
            snippet: String::new(),
            doc_type: "wiki".to_string(),
            docid: "aaa".to_string(),
            hash: String::new(),
        }];
        assert!(!Bm25Search::is_strong_signal(&results));
    }

    #[test]
    fn is_strong_signal_empty() {
        assert!(!Bm25Search::is_strong_signal(&[]));
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
    fn reindex_page_from_content_rebuilds_collections_from_frontmatter() {
        let (_dir, search) = open_temp_search();
        let now = "2026-04-22T00:00:00Z";

        let original = "---\ntitle: Alpha\ntags: []\ncreated_at: 2026-04-22T00:00:00Z\nupdated_at: 2026-04-22T00:00:00Z\nsources: []\ncollections: [default]\n---\n\nBody\n";
        let old_hash = search.insert_content(original).unwrap();
        search
            .upsert_document(
                "wiki",
                "wiki/alpha.md",
                "Alpha",
                &old_hash,
                "alpha01",
                "",
                "",
                now,
                now,
            )
            .unwrap();
        search
            .set_document_collections_by_path("wiki", "wiki/alpha.md", &["default".to_string()])
            .unwrap();

        let updated = "---\ntitle: Alpha\ntags: []\ncreated_at: 2026-04-22T00:00:00Z\nupdated_at: 2026-04-22T00:00:00Z\nsources: []\ncollections: [team-x]\n---\n\nBody changed\n";
        search
            .reindex_page_from_content("wiki/alpha.md", updated, &old_hash)
            .unwrap();

        let got = search
            .document_collections_by_path("wiki", "wiki/alpha.md")
            .unwrap();
        assert_eq!(got, vec!["team-x".to_string()]);
    }

    #[test]
    fn store_ingest_batch_overwrite_cleans_old_hash() {
        let (_dir, search) = open_temp_search();
        let now = "2026-04-22T00:00:00Z";

        let old_page = "---\ntitle: Alpha\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\ncollections: [default]\n---\n\nOld body\n";
        let old_hash = search.insert_content(old_page).unwrap();
        search
            .upsert_document(
                "wiki",
                "alpha-page",
                "Alpha",
                &old_hash,
                "oldalpha",
                "",
                "",
                now,
                now,
            )
            .unwrap();

        let wiki_pages = vec![IngestWikiPage {
            slug: "alpha-page".to_string(),
            title: "Alpha".to_string(),
            content: "---\ntitle: Alpha\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\ncollections: [default]\n---\n\nNew body\n".to_string(),
            tags: String::new(),
        }];
        search
            .store_ingest_batch(
                "source text",
                "/tmp/source-overwrite.md",
                "Source",
                "Summary",
                &wiki_pages,
                &["default".to_string()],
                now,
            )
            .unwrap();

        assert!(
            !search.content_exists(&old_hash).unwrap(),
            "expected superseded page hash to be orphan-cleaned"
        );
    }

    #[tokio::test]
    async fn bm25_malformed_query_returns_empty() {
        let (_dir, search) = open_temp_search();
        // Even with special chars, we should get empty rather than an error
        let results = search.search_by_doc_type("***", "wiki", 10).unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn bm25_index_page_replace() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/page.md"),
                "Original Title",
                "Original body content.",
                "tag1",
                "cc0011",
                "Original summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        // Replace with new content
        search
            .index_page(
                Path::new("wiki/page.md"),
                "Updated Title",
                "Completely new body content about quantum computing.",
                "tag2",
                "cc0011",
                "Updated summary",
                "2026-04-07T00:00:00Z",
            )
            .unwrap();

        let results = search
            .search_by_doc_type("quantum computing", "wiki", 10)
            .unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Updated Title");
    }

    #[tokio::test]
    async fn generate_index_from_db() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "The borrow checker enforces ownership rules at compile time.",
                "concept, rust",
                "ab12cd",
                "Borrow checker overview",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let index = search.generate_index().unwrap();
        assert!(index.contains("# Index"), "got: {index}");
        assert!(
            index.contains("Rust Borrow Checker"),
            "should contain title; got: {index}"
        );
        assert!(
            index.contains("wiki/rust-borrow.md"),
            "should contain path; got: {index}"
        );
        assert!(
            index.contains("Borrow checker overview"),
            "should contain summary; got: {index}"
        );
    }

    #[tokio::test]
    async fn generate_compact_index_from_db() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "The borrow checker enforces ownership rules.",
                "concept",
                "ab12cd",
                "Borrow checker overview",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let compact = search.generate_compact_index().unwrap();
        assert!(
            compact.contains("ab12cd | Borrow checker overview"),
            "should have docid | summary; got: {compact}"
        );
        // Compact index should NOT contain title or path
        assert!(
            !compact.contains("Rust Borrow Checker"),
            "compact index should not contain title; got: {compact}"
        );
        assert!(
            !compact.contains("wiki/rust-borrow.md"),
            "compact index should not contain path; got: {compact}"
        );
    }

    #[test]
    fn lookup_docid_resolves() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "Body text.",
                "concept",
                "ab12cd",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let path = search.lookup_docid("ab12cd").unwrap();
        assert_eq!(path, Some(PathBuf::from("wiki/rust-borrow.md")));
    }

    #[test]
    fn lookup_docid_not_found() {
        let (_dir, search) = open_temp_search();
        let path = search.lookup_docid("nonexistent").unwrap();
        assert_eq!(path, None);
    }

    #[test]
    fn resolve_ref_tries_docid_then_stem_then_title() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "Body text.",
                "concept",
                "ab12cd",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let expected = Some(PathBuf::from("wiki/rust-borrow.md"));

        // By docid
        assert_eq!(search.resolve_ref("ab12cd").unwrap(), expected);
        // By stem
        assert_eq!(search.resolve_ref("rust-borrow").unwrap(), expected);
        // By title
        assert_eq!(search.resolve_ref("Rust Borrow Checker").unwrap(), expected);
        // Case-insensitive title
        assert_eq!(search.resolve_ref("rust borrow checker").unwrap(), expected);
        // Non-existent
        assert_eq!(search.resolve_ref("nonexistent").unwrap(), None);
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
                "aaa111",
                "Alpha summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/beta.md"),
                "Beta",
                "Beta body.",
                "tag",
                "bbb222",
                "Beta summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        assert_eq!(search.page_count().unwrap(), 2);
    }

    #[test]
    fn lookup_path_docid_resolves() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/alpha.md"),
                "Alpha",
                "Body.",
                "",
                "aaa111",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let docid = search
            .lookup_path_docid(Path::new("wiki/alpha.md"))
            .unwrap();
        assert_eq!(docid, Some("aaa111".to_string()));

        // Non-existent path
        let missing = search
            .lookup_path_docid(Path::new("wiki/missing.md"))
            .unwrap();
        assert_eq!(missing, None);
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
                "aaa111",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let ts = search
            .get_last_modified(Path::new("wiki/alpha.md"))
            .unwrap();
        assert_eq!(ts, Some("2026-04-06T00:00:00Z".to_string()));

        // Non-existent path
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
                "aaa111",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/beta.md"),
                "Beta Topic",
                "Body.",
                "",
                "bbb222",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        let pairs = search.all_stems_and_titles().unwrap();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("alpha".to_string(), "Alpha Topic".to_string())));
        assert!(pairs.contains(&("beta".to_string(), "Beta Topic".to_string())));
    }

    #[test]
    fn generate_index_empty_db() {
        let (_dir, search) = open_temp_search();
        let index = search.generate_index().unwrap();
        assert!(
            index.contains("No wiki pages yet"),
            "empty DB should produce placeholder; got: {index}"
        );
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
                "abc123",
                "Summary",
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
                "abc123",
                "Summary",
                "2026-04-06T00:00:00Z",
            )
            .unwrap();

        // Exact match
        let path = search.lookup_title("My Page Title").unwrap();
        assert_eq!(path, Some(PathBuf::from("wiki/my-page.md")));

        // Case-insensitive
        let path = search.lookup_title("my page title").unwrap();
        assert_eq!(path, Some(PathBuf::from("wiki/my-page.md")));

        let missing = search.lookup_title("nonexistent").unwrap();
        assert_eq!(missing, None);
    }

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
            dir.path().to_str().unwrap(),
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
            dir.path().to_str().unwrap(),
            &["team-a".to_string(), "incidents".to_string()],
        )
        .unwrap();
        let jobs = s.pending_ingest_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_type, crate::search::JobType::Document);
        assert_eq!(jobs[0].agent, None);
        assert_eq!(jobs[0].collections, vec!["team-a", "incidents"]);
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

        // Query the pragma — SQLite exposes current busy_timeout in ms.
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
