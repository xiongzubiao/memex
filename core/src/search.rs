use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

use crate::error::{MemexError, Result};
use crate::types::Document;

/// Minimum relevance score threshold.  Currently 0.0 (no filtering) per spec:
/// "No minScore filtering (QMD's `hybridQuery` default is 0)."
pub const MIN_SCORE: f32 = 0.0;

/// A single search result from the BM25 full-text index.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: PathBuf,
    pub title: String,
    /// Relevance score normalized to 0.0-1.0 range.
    pub score: f32,
    /// A short excerpt from the page body highlighting the matched term.
    pub snippet: String,
    /// Document collection (wiki or source).
    pub collection: String,
    /// Short document identifier.
    pub docid: String,
}

/// Trait for full-text wiki search.
#[async_trait::async_trait]
pub trait WikiSearch: Send + Sync {
    /// Search the index for pages matching `query`, returning the top `top_k` results.
    /// `intent` is an optional hint for future semantic search integration.
    async fn search(
        &self,
        query: &str,
        top_k: usize,
        intent: Option<&str>,
    ) -> Result<Vec<SearchResult>>;

    /// Index a single page (insert or replace).
    #[allow(clippy::too_many_arguments)]
    fn index_page(
        &self,
        path: &Path,
        title: &str,
        body: &str,
        tags: &str,
        docid: &str,
        summary: &str,
        last_modified: &str,
    ) -> Result<()>;

    /// Remove a page from the index by its path.
    fn remove_page(&self, path: &str) -> Result<()>;

    /// Rebuild the entire search index by walking the wiki directory.
    fn rebuild(&self, root: &Path) -> Result<()>;

    /// Return the set of all docids currently stored in the index.
    fn existing_docids(&self) -> Result<std::collections::HashSet<String>>;
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

/// Strong signal detection: s1 >= 0.85 AND (s1 - s2) >= 0.15
///
/// If there's only one result (s2 = 0.0), a high s1 is still strong.
pub fn is_strong_signal(s1: f64, s2: f64) -> bool {
    s1 >= 0.85 && (s1 - s2) >= 0.15
}

/// BM25 search filtered by collection.
///
/// Returns results from the `documents_fts` virtual table joined with `documents`,
/// filtered by collection. BM25 column weights differ by collection:
/// - wiki: path=1.5, title=4.0, tags=1.5, body=1.0
/// - source: path=1.5, title=4.0, tags=0.0, body=1.0
pub fn search_bm25(
    conn: &rusqlite::Connection,
    query: &str,
    collection: &str,
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

    // BM25 column weights: path, title, tags, body
    // Wiki uses tags weight 1.5, source uses 0.0
    let bm25_expr = if collection == "source" {
        "bm25(documents_fts, 1.5, 4.0, 0.0, 1.0)"
    } else {
        "bm25(documents_fts, 1.5, 4.0, 1.5, 1.0)"
    };

    let sql = format!(
        "SELECT d.id, d.docid, d.collection, d.path, d.title, \
                {bm25_expr} as score, d.summary \
         FROM documents_fts f \
         JOIN documents d ON d.id = f.rowid \
         WHERE documents_fts MATCH ?1 AND d.collection = ?2 \
         ORDER BY score \
         LIMIT ?3"
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };

    let rows: Vec<(String, String, String, String, f64, String)> = match stmt.query_map(
        rusqlite::params![sanitized, collection, limit as i64],
        |row| {
            Ok((
                row.get::<_, String>(1)?, // docid
                row.get::<_, String>(2)?, // collection
                row.get::<_, String>(3)?, // path
                row.get::<_, String>(4)?, // title
                row.get::<_, f64>(5)?,    // raw score
                row.get::<_, String>(6)?, // summary
            ))
        },
    ) {
        Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
        Err(_) => return Ok(Vec::new()),
    };

    let results = rows
        .into_iter()
        .map(|(docid, coll, path, title, raw, summary)| {
            let score = normalize_bm25(raw) as f32;
            SearchResult {
                path: PathBuf::from(&path),
                title,
                score,
                snippet: summary,
                collection: coll,
                docid,
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
/// - Wiki BM25 lists (identified by `wiki_list_indices`) get weight 2.0, all others 1.0
/// - Formula: score(d) = sum_i(w_i / (k + rank_i(d))) + bonus(rank_i(d))
/// - Bonus: +0.05 for rank 1, +0.02 for ranks 2-3 (1-based)
/// - Post-fusion: reassign scores as 1/rank
pub fn rrf_fuse(
    lists: &[Vec<SearchResult>],
    wiki_list_indices: &[usize],
    k: u32,
) -> Vec<SearchResult> {
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
        let weight: f32 = if wiki_list_indices.contains(&list_idx) {
            2.0
        } else {
            1.0
        };

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

/// Three-tier identifier resolution.
///
/// 1. Docid prefix: `WHERE docid LIKE ?1 || '%'`
/// 2. Stem: match path basename without extension
/// 3. Title: `WHERE title = ?1 COLLATE NOCASE`
pub fn resolve_ref(conn: &rusqlite::Connection, reference: &str) -> Result<Vec<Document>> {
    // Tier 1: Docid prefix match
    let mut stmt = conn.prepare(
        "SELECT id, collection, path, title, hash, docid, tags, summary, created_at, updated_at \
         FROM documents WHERE docid LIKE ?1 || '%'",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![reference], |row| {
            Ok(Document {
                id: row.get(0)?,
                collection: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                hash: row.get(4)?,
                docid: row.get(5)?,
                tags: row.get(6)?,
                summary: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?
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
        "SELECT id, collection, path, title, hash, docid, tags, summary, created_at, updated_at \
         FROM documents WHERE path LIKE ?1 OR path = ?2",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![stem_pattern, exact_stem], |row| {
            Ok(Document {
                id: row.get(0)?,
                collection: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                hash: row.get(4)?,
                docid: row.get(5)?,
                tags: row.get(6)?,
                summary: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    if !docs.is_empty() {
        return Ok(docs);
    }

    // Tier 3: Title match (case-insensitive)
    let mut stmt = conn.prepare(
        "SELECT id, collection, path, title, hash, docid, tags, summary, created_at, updated_at \
         FROM documents WHERE title = ?1 COLLATE NOCASE",
    )?;
    let docs: Vec<Document> = stmt
        .query_map(rusqlite::params![reference], |row| {
            Ok(Document {
                id: row.get(0)?,
                collection: row.get(1)?,
                path: row.get(2)?,
                title: row.get(3)?,
                hash: row.get(4)?,
                docid: row.get(5)?,
                tags: row.get(6)?,
                summary: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(docs)
}

// ---------------------------------------------------------------------------
// Backward-compatible Bm25Search wrapper
// ---------------------------------------------------------------------------

/// BM25 full-text search backed by SQLite FTS5.
///
/// Uses the new content-addressable schema (content/documents/documents_fts)
/// from `schema::init_schema`. The inner `Connection` is wrapped in a `Mutex`
/// so that `Bm25Search` satisfies the `Send + Sync` bounds required by `WikiSearch`.
pub struct Bm25Search {
    conn: Mutex<rusqlite::Connection>,
}

impl Bm25Search {
    /// Open (or create) the search database at `db_path`.
    ///
    /// Creates the new content-addressable schema (content, documents, documents_fts)
    /// via `schema::init_schema`. Enables WAL mode for concurrent reads.
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(db_path).map_err(sqlite_err)?;

        // WAL mode for concurrent reads
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(sqlite_err)?;

        // SQLite-level busy_timeout — belt-and-suspenders for transient contention.
        // Must be set before init_schema runs DDL so first-open DDL is covered too.
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .map_err(sqlite_err)?;

        // Initialize new content-addressable schema (includes strip_frontmatter UDF)
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

    /// Insert or update a document row in the documents table.
    ///
    /// On conflict (same collection + path), updates title, hash, tags,
    /// summary, and updated_at. Preserves created_at and docid.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_document(
        &self,
        collection: &str,
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
        conn.execute(
            "INSERT INTO documents (collection, path, title, hash, docid, tags, summary, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(collection, path) DO UPDATE SET \
                 title = excluded.title, \
                 hash = excluded.hash, \
                 tags = excluded.tags, \
                 summary = excluded.summary, \
                 updated_at = excluded.updated_at",
            rusqlite::params![collection, path, title, hash, docid, tags, summary, created_at, updated_at],
        )
        .map_err(sqlite_err)?;
        Ok(())
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

    /// Get wiki page count (documents in the wiki collection only).
    pub fn wiki_page_count(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM documents WHERE collection = 'wiki'",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_err)?;
        Ok(count as usize)
    }

    /// Return all wiki documents (collection = 'wiki').
    pub fn all_wiki_documents(&self) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, collection, path, title, hash, docid, tags, summary, created_at, updated_at \
                 FROM documents WHERE collection = 'wiki'"
            )
            .map_err(sqlite_err)?;
        let docs: Vec<Document> = stmt
            .query_map([], |row| {
                Ok(Document {
                    id: row.get(0)?,
                    collection: row.get(1)?,
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
        if let Some((title, _body, tags, summary)) = parse_page_for_indexing(content) {
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

    /// BM25 search filtered by collection.
    ///
    /// Wraps the free function `search_bm25()` with mutex-guarded connection access.
    pub fn search_collection(
        &self,
        query: &str,
        collection: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        search_bm25(&conn, query, collection, limit)
    }

    /// Look up document metadata by content hash.
    ///
    /// Returns all documents that reference the given content hash.
    /// Used by vector search to convert chunk-level results (keyed by hash)
    /// into full `SearchResult` records with docid, collection, path, etc.
    pub fn lookup_documents_by_hash(&self, hash: &str) -> Result<Vec<Document>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, collection, path, title, hash, docid, tags, summary, created_at, updated_at \
                 FROM documents WHERE hash = ?1",
            )
            .map_err(sqlite_err)?;
        let docs: Vec<Document> = stmt
            .query_map(rusqlite::params![hash], |row| {
                Ok(Document {
                    id: row.get(0)?,
                    collection: row.get(1)?,
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
            .map_err(sqlite_err)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(docs)
    }
}

#[async_trait::async_trait]
impl WikiSearch for Bm25Search {
    async fn search(
        &self,
        query: &str,
        top_k: usize,
        _intent: Option<&str>,
    ) -> Result<Vec<SearchResult>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let sanitized = sanitize_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;

        // FTS5 MATCH can fail on malformed input — return empty results.
        // BM25 column weights for documents_fts: path=1.5, title=4.0, tags=1.5, body=1.0
        let sql = r#"
            SELECT d.path, d.title,
                   bm25(documents_fts, 1.5, 4.0, 1.5, 1.0) AS raw_score,
                   d.docid, d.collection, d.summary
            FROM documents_fts f
            JOIN documents d ON d.id = f.rowid
            WHERE documents_fts MATCH ?1
            ORDER BY raw_score
            LIMIT ?2
        "#;

        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };

        let rows: Vec<(String, String, f64, String, String, String)> =
            match stmt.query_map(rusqlite::params![sanitized, top_k as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            }) {
                Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
                Err(_) => return Ok(Vec::new()),
            };

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let results = rows
            .into_iter()
            .map(|(path, title, raw, docid, collection, summary)| {
                let score = normalize_bm25(raw) as f32;
                SearchResult {
                    path: PathBuf::from(path),
                    title,
                    score,
                    snippet: summary,
                    collection,
                    docid,
                }
            })
            .collect();

        Ok(results)
    }

    fn index_page(
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

        // Determine collection from path prefix
        let collection = if path_str.starts_with("source") {
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
            "INSERT INTO documents (collection, path, title, hash, docid, tags, summary, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(collection, path) DO UPDATE SET \
                 title = excluded.title, \
                 hash = excluded.hash, \
                 docid = excluded.docid, \
                 tags = excluded.tags, \
                 summary = excluded.summary, \
                 updated_at = excluded.updated_at",
            rusqlite::params![
                collection,
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
        Ok(())
    }

    fn remove_page(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        conn.execute(
            "DELETE FROM documents WHERE path = ?1",
            rusqlite::params![path],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    fn existing_docids(&self) -> Result<std::collections::HashSet<String>> {
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

    fn rebuild(&self, root: &Path) -> Result<()> {
        use crate::docid;

        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
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
            String, // path
            String, // title
            String, // body
            String, // tags
            String, // docid
            String, // summary
            String, // last_modified
            String, // content
            String, // content_hash
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

            if let Some((title, body, tags, summary)) = parse_page_for_indexing(&content) {
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
                    did,
                    summary,
                    last_modified,
                    content,
                    hash,
                ));
            }
        }

        conn.execute_batch("BEGIN").map_err(sqlite_err)?;
        for (path, title, _body, tags, docid, summary, last_modified, content, hash) in
            &pages_to_index
        {
            // Insert content
            conn.execute(
                "INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![hash, content, last_modified],
            )
            .map_err(sqlite_err)?;

            // Insert document
            conn.execute(
                "INSERT INTO documents (collection, path, title, hash, docid, tags, summary, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(collection, path) DO UPDATE SET \
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
        }
        conn.execute_batch("COMMIT").map_err(sqlite_err)?;

        Ok(())
    }
}

/// Parse a wiki page's content into (title, body, tags, summary) for indexing.
///
/// Uses `crate::validate::parse_frontmatter` to extract frontmatter fields.
/// Returns `None` if the page has no valid frontmatter.
/// Summary is taken from the frontmatter `summary` field if present, otherwise
/// extracted from the first non-empty body line (truncated to 120 chars).
pub fn parse_page_for_indexing(content: &str) -> Option<(String, String, String, String)> {
    let (fm, body) = crate::validate::parse_frontmatter(content).ok()?;
    if fm.title.trim().is_empty() {
        return None;
    }
    let tags = fm.tags.join(", ");
    let summary = fm
        .summary
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::index::extract_summary(&body, 120));
    Some((fm.title, body, tags, summary))
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
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::init_schema(&conn).unwrap();
        conn
    }

    /// Helper: insert a document into the new schema.
    fn insert_doc(
        conn: &rusqlite::Connection,
        collection: &str,
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
            "INSERT INTO documents (collection, path, title, hash, docid, tags, summary, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![collection, path, title, hash, docid, tags, "", now, now],
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
    fn search_filters_by_collection() {
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
            assert_eq!(r.collection, "wiki", "expected only wiki results");
        }

        // Search source only
        let source_results = search_bm25(&conn, "borrow checker rust", "source", 10).unwrap();
        assert!(
            !source_results.is_empty(),
            "expected source results for 'borrow checker rust'"
        );
        for r in &source_results {
            assert_eq!(r.collection, "source", "expected only source results");
        }
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
                collection: "wiki".to_string(),
                docid: "aaa".to_string(),
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.5,
                snippet: String::new(),
                collection: "wiki".to_string(),
                docid: "bbb".to_string(),
            },
        ];

        let fused = rrf_fuse(&[list], &[0], 60);
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
            collection: "wiki".to_string(),
            docid: "aaa".to_string(),
        }];
        let source_list = vec![SearchResult {
            path: PathBuf::from("sources/b.md"),
            title: "B".to_string(),
            score: 0.8,
            snippet: String::new(),
            collection: "source".to_string(),
            docid: "bbb".to_string(),
        }];

        // Wiki list (index 0) gets 2x weight
        let fused = rrf_fuse(&[wiki_list, source_list], &[0], 60);
        assert_eq!(fused.len(), 2);
        // Wiki result should rank first due to 2x weight
        assert_eq!(fused[0].path, PathBuf::from("wiki/a.md"));
    }

    // -----------------------------------------------------------------------
    // Backward-compatible Bm25Search tests
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
            .search("borrow checker rust", 10, None)
            .await
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
        let before = search.search("ephemeral", 10, None).await.unwrap();
        assert_eq!(before.len(), 1);

        search.remove_page("wiki/ephemeral.md").unwrap();

        let after = search.search("ephemeral", 10, None).await.unwrap();
        assert!(
            after.is_empty(),
            "removed page should not appear in results"
        );
    }

    #[tokio::test]
    async fn bm25_empty_search() {
        let (_dir, search) = open_temp_search();

        let results = search.search("anything", 10, None).await.unwrap();
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

        let results = search.search("alpha greek", 10, None).await.unwrap();
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
                collection: "wiki".to_string(),
                docid: "aaa".to_string(),
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.50,
                snippet: String::new(),
                collection: "wiki".to_string(),
                docid: "bbb".to_string(),
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
                collection: "wiki".to_string(),
                docid: "aaa".to_string(),
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.88,
                snippet: String::new(),
                collection: "wiki".to_string(),
                docid: "bbb".to_string(),
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
            collection: "wiki".to_string(),
            docid: "aaa".to_string(),
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
        let (title, body, tags, _summary) = parse_page_for_indexing(&page).unwrap();
        assert_eq!(title, "Test Title");
        assert!(body.contains("Some body text."));
        assert_eq!(tags, "entity, concept");
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
    fn sanitize_query_strips_operators() {
        assert_eq!(sanitize_query("hello (world)"), "\"hello\"* AND \"world\"*");
        assert_eq!(sanitize_query(""), "");
    }

    #[tokio::test]
    async fn bm25_malformed_query_returns_empty() {
        let (_dir, search) = open_temp_search();
        // Even with special chars, we should get empty rather than an error
        let results = search.search("***", 10, None).await.unwrap();
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

        let results = search.search("quantum computing", 10, None).await.unwrap();
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
