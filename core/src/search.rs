use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

use crate::error::{MemexError, Result};

/// A single search result from the BM25 full-text index.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub path: PathBuf,
    pub title: String,
    /// Relevance score normalized to 0.0-1.0 range.
    pub score: f32,
    /// A short excerpt from the page body highlighting the matched term.
    pub snippet: String,
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
    fn index_page(&self, path: &Path, title: &str, body: &str, tags: &str) -> Result<()>;

    /// Remove a page from the index by its path.
    fn remove_page(&self, path: &str) -> Result<()>;

    /// Rebuild the entire search index by walking the wiki directory.
    fn rebuild(&self, root: &Path) -> Result<()>;
}

/// BM25 full-text search backed by SQLite FTS5.
///
/// The inner `Connection` is wrapped in a `Mutex` so that `Bm25Search`
/// satisfies the `Send + Sync` bounds required by `WikiSearch`.
pub struct Bm25Search {
    conn: Mutex<rusqlite::Connection>,
}

impl Bm25Search {
    /// Open (or create) the search database at `db_path`.
    ///
    /// Creates the schema (pages, pages_fts, triggers, llm_cache) if it does not exist.
    /// Enables WAL mode for concurrent reads.
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(db_path).map_err(sqlite_err)?;

        // WAL mode for concurrent reads
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .map_err(sqlite_err)?;

        conn.execute_batch(SCHEMA_SQL).map_err(sqlite_err)?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Returns `true` if the pages table contains zero rows.
    pub fn is_empty(&self) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pages", [], |row| row.get(0))
            .map_err(sqlite_err)?;
        Ok(count == 0)
    }

    /// Returns `true` if the top result score >= 0.85 AND the gap to the second
    /// result is >= 0.15.  Useful for deciding whether search alone is sufficient
    /// or an LLM call is needed (following qmd pattern).
    pub fn is_strong_signal(results: &[SearchResult]) -> bool {
        match results.first() {
            Some(top) if top.score >= 0.85 => match results.get(1) {
                Some(second) => (top.score - second.score) >= 0.15,
                None => true, // single result with high score
            },
            _ => false,
        }
    }
}

/// Reciprocal Rank Fusion (following qmd's implementation).
///
/// Merges multiple ranked result lists into a single ranked list.
/// Formula: `score = sum(weight / (k + rank + 1)) + top_rank_bonus`
///
/// - `k = 60` (standard RRF constant)
/// - First 2 lists get 2x weight (qmd pattern: original probe + primary search)
/// - Top-rank bonus: +0.05 for rank 1, +0.02 for rank 2-3
pub fn reciprocal_rank_fusion(
    ranked_lists: &[Vec<SearchResult>],
    limit: usize,
) -> Vec<SearchResult> {
    // Single list: no fusion needed, just assign 1/rank scores directly.
    if ranked_lists.len() == 1 {
        return ranked_lists[0]
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, r)| {
                let mut r = r.clone();
                r.score = 1.0 / (i as f32 + 1.0);
                r
            })
            .collect();
    }

    const K: f32 = 60.0;

    // Accumulate scores per document path
    let mut scores: std::collections::HashMap<
        PathBuf,
        (SearchResult, f32, usize), // (best result, rrf_score, top_rank)
    > = std::collections::HashMap::new();

    for (list_idx, list) in ranked_lists.iter().enumerate() {
        // First 2 lists get 2x weight (following qmd)
        let weight: f32 = if list_idx < 2 { 2.0 } else { 1.0 };

        for (rank, result) in list.iter().enumerate() {
            let contribution = weight / (K + rank as f32 + 1.0);

            scores
                .entry(result.path.clone())
                .and_modify(|(existing, rrf_score, top_rank)| {
                    *rrf_score += contribution;
                    if rank < *top_rank {
                        *top_rank = rank;
                        *existing = result.clone();
                    }
                })
                .or_insert_with(|| (result.clone(), contribution, rank));
        }
    }

    // Apply top-rank bonus (following qmd)
    for (_, rrf_score, top_rank) in scores.values_mut() {
        if *top_rank == 0 {
            *rrf_score += 0.05;
        } else if *top_rank <= 2 {
            *rrf_score += 0.02;
        }
    }

    // Sort by RRF score descending, then assign 1/rank as final score
    // (following qmd's skipRerank path: rank 1 → 1.0, rank 2 → 0.5, etc.)
    // This puts scores on a 0-1 scale where minScore thresholds are meaningful.
    let mut sorted: Vec<(SearchResult, f32)> = scores
        .into_values()
        .map(|(result, rrf_score, _)| (result, rrf_score))
        .collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    sorted.truncate(limit);

    sorted
        .into_iter()
        .enumerate()
        .map(|(i, (mut result, _))| {
            result.score = 1.0 / (i as f32 + 1.0);
            result
        })
        .collect()
}

impl Bm25Search {
    /// Retrieve a cached LLM response for `key`.
    pub fn cache_get(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let mut stmt = conn
            .prepare("SELECT response FROM llm_cache WHERE key = ?1")
            .map_err(sqlite_err)?;
        let mut rows = stmt
            .query_map(rusqlite::params![key], |row| row.get::<_, String>(0))
            .map_err(sqlite_err)?;
        match rows.next() {
            Some(Ok(val)) => Ok(Some(val)),
            Some(Err(e)) => Err(sqlite_err(e)),
            None => Ok(None),
        }
    }

    /// Store an LLM response under `key` (insert-or-replace).
    pub fn cache_put(&self, key: &str, response: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        conn.execute(
            "INSERT OR REPLACE INTO llm_cache (key, response) VALUES (?1, ?2)",
            rusqlite::params![key, response],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    /// Clear all cached LLM responses.
    pub fn cache_clear(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        conn.execute("DELETE FROM llm_cache", [])
            .map_err(sqlite_err)?;
        Ok(())
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

        // Sanitize query for FTS5: remove characters that could cause MATCH parse errors.
        let sanitized = sanitize_fts_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;

        // FTS5 MATCH can fail on malformed input — return empty results.
        // BM25 column weights: title=4.0, tags=1.5, body=1.0 (following qmd pattern).
        let sql = r#"
            SELECT p.path, p.title, bm25(pages_fts, 4.0, 1.5, 1.0) AS raw_score,
                   snippet(pages_fts, 2, '<b>', '</b>', '...', 32) AS snip
            FROM pages_fts
            JOIN pages p ON p.rowid = pages_fts.rowid
            WHERE pages_fts MATCH ?1
            ORDER BY raw_score
            LIMIT ?2
        "#;

        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };

        let rows: Vec<(String, String, f64, String)> =
            match stmt.query_map(rusqlite::params![sanitized, top_k as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            }) {
                Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
                Err(_) => return Ok(Vec::new()),
            };

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // Normalize bm25() scores to [0, 1) using |x| / (1 + |x|) (following qmd).
        // FTS5 BM25 scores are negative (lower = better): -10 is strong, -0.5 is weak.
        // This sigmoid-like transform is query-independent — a score of 0.67 always
        // means the same thing regardless of what other results exist.
        // Maps: strong(-10)→0.91, medium(-2)→0.67, weak(-0.5)→0.33, none(0)→0.
        let results = rows
            .into_iter()
            .map(|(path, title, raw, snippet)| {
                let abs = raw.abs();
                let score = (abs / (1.0 + abs)) as f32;
                SearchResult {
                    path: PathBuf::from(path),
                    title,
                    score,
                    snippet,
                }
            })
            .collect();

        Ok(results)
    }

    fn index_page(&self, path: &Path, title: &str, body: &str, tags: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        let path_str = path.to_string_lossy();
        conn.execute(
            "INSERT OR REPLACE INTO pages (path, title, body, tags) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![path_str.as_ref(), title, body, tags],
        )
        .map_err(sqlite_err)?;
        Ok(())
    }

    fn remove_page(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
        conn.execute("DELETE FROM pages WHERE path = ?1", rusqlite::params![path])
            .map_err(sqlite_err)?;
        Ok(())
    }

    fn rebuild(&self, root: &Path) -> Result<()> {
        // Clear existing data
        {
            let conn = self.conn.lock().map_err(|e| mutex_err(&e))?;
            conn.execute("DELETE FROM pages", []).map_err(sqlite_err)?;
        }

        let wiki_dir = root.join("wiki");
        if !wiki_dir.is_dir() {
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

            if let Some((title, body, tags)) = parse_page_for_indexing(&content) {
                let rel_path = abs_path.strip_prefix(root).unwrap_or(abs_path);
                self.index_page(rel_path, &title, &body, &tags)?;
            }
        }

        Ok(())
    }
}

/// Parse a wiki page's content into (title, body, tags) for indexing.
///
/// Uses `crate::validate::parse_frontmatter` to extract frontmatter fields.
/// Returns `None` if the page has no valid frontmatter.
pub fn parse_page_for_indexing(content: &str) -> Option<(String, String, String)> {
    let (fm, body) = crate::validate::parse_frontmatter(content).ok()?;
    if fm.title.trim().is_empty() {
        return None;
    }
    let tags = fm.tags.join(", ");
    Some((fm.title, body, tags))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a rusqlite error into a MemexError::Io.
fn sqlite_err(e: rusqlite::Error) -> MemexError {
    MemexError::Io(std::io::Error::other(e.to_string()))
}

/// Convert a mutex poison error into a MemexError::Io.
fn mutex_err<T>(e: &std::sync::PoisonError<T>) -> MemexError {
    MemexError::Io(std::io::Error::other(e.to_string()))
}

/// Sanitize a user query for FTS5 MATCH (following qmd pattern).
///
/// Each term is quoted with prefix matching (`"term"*`), joined with AND.
/// AND may return 0 results for natural-language queries (stop words absent
/// from wiki pages), but that's correct — a zero-result BM25 probe triggers
/// tier 2 (LLM query expansion) which generates proper search terms.
fn sanitize_fts_query(query: &str) -> String {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|w| {
            // Strip non-letter/digit chars (following qmd's sanitizeFTS5Term)
            w.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect();

    words.join(" AND ")
}

/// Schema SQL for the search database.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS pages (
    path  TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    body  TEXT NOT NULL,
    tags  TEXT NOT NULL DEFAULT ''
);

CREATE VIRTUAL TABLE IF NOT EXISTS pages_fts USING fts5(
    title,
    tags,
    body,
    content='pages',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

-- Triggers to keep FTS in sync with the content table.
CREATE TRIGGER IF NOT EXISTS pages_ai AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts(rowid, title, tags, body)
    VALUES (new.rowid, new.title, new.tags, new.body);
END;

CREATE TRIGGER IF NOT EXISTS pages_ad AFTER DELETE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, tags, body)
    VALUES ('delete', old.rowid, old.title, old.tags, old.body);
END;

CREATE TRIGGER IF NOT EXISTS pages_au AFTER UPDATE ON pages BEGIN
    INSERT INTO pages_fts(pages_fts, rowid, title, tags, body)
    VALUES ('delete', old.rowid, old.title, old.tags, old.body);
    INSERT INTO pages_fts(rowid, title, tags, body)
    VALUES (new.rowid, new.title, new.tags, new.body);
END;

CREATE TABLE IF NOT EXISTS llm_cache (
    key      TEXT PRIMARY KEY,
    response TEXT NOT NULL
);
"#;

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
            "---\ntitle: {title}\ntags:{tag_yaml}\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\n\n{body}\n"
        )
    }

    #[tokio::test]
    async fn bm25_index_and_search() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/rust-borrow.md"),
                "Rust Borrow Checker",
                "The borrow checker enforces ownership rules at compile time.",
                "concept, rust",
            )
            .unwrap();
        search
            .index_page(
                Path::new("wiki/python-gc.md"),
                "Python Garbage Collection",
                "Python uses reference counting with a cyclic garbage collector.",
                "concept, python",
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
    async fn bm25_snippet_returned() {
        let (_dir, search) = open_temp_search();

        search
            .index_page(
                Path::new("wiki/caching.md"),
                "Caching Strategies",
                "Effective caching reduces latency and saves compute costs. \
                 Common strategies include TTL, LRU, and write-through caching.",
                "concept",
            )
            .unwrap();

        let results = search.search("caching", 10, None).await.unwrap();
        assert!(!results.is_empty());
        let snippet = &results[0].snippet;
        // The snippet should contain the matched term (possibly within <b> tags)
        let snippet_lower = snippet.to_lowercase();
        assert!(
            snippet_lower.contains("cach"),
            "snippet should contain the matched term; got: {snippet}"
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
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.50,
                snippet: String::new(),
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
            },
            SearchResult {
                path: PathBuf::from("wiki/b.md"),
                title: "B".to_string(),
                score: 0.88,
                snippet: String::new(),
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
        }];
        assert!(!Bm25Search::is_strong_signal(&results));
    }

    #[test]
    fn is_strong_signal_empty() {
        assert!(!Bm25Search::is_strong_signal(&[]));
    }

    #[test]
    fn cache_roundtrip() {
        let (_dir, search) = open_temp_search();
        assert!(search.cache_get("k1").unwrap().is_none());

        search.cache_put("k1", "response-1").unwrap();
        assert_eq!(search.cache_get("k1").unwrap().unwrap(), "response-1");

        // Overwrite
        search.cache_put("k1", "response-2").unwrap();
        assert_eq!(search.cache_get("k1").unwrap().unwrap(), "response-2");

        search.cache_clear().unwrap();
        assert!(search.cache_get("k1").unwrap().is_none());
    }

    #[test]
    fn parse_page_for_indexing_valid() {
        let page = make_page("Test Title", "Some body text.", &["entity", "concept"]);
        let (title, body, tags) = parse_page_for_indexing(&page).unwrap();
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
        let page = "---\ntitle: \"\"\ntags: []\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\nbody";
        assert!(parse_page_for_indexing(page).is_none());
    }

    #[test]
    fn sanitize_fts_query_strips_operators() {
        assert_eq!(
            sanitize_fts_query("hello (world)"),
            "\"hello\"* AND \"world\"*"
        );
        assert_eq!(sanitize_fts_query("a:b"), "\"ab\"*");
        assert_eq!(sanitize_fts_query(""), "");
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
            )
            .unwrap();

        // Replace with new content
        search
            .index_page(
                Path::new("wiki/page.md"),
                "Updated Title",
                "Completely new body content about quantum computing.",
                "tag2",
            )
            .unwrap();

        let results = search.search("quantum computing", 10, None).await.unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].title, "Updated Title");
    }
}
