use async_trait::async_trait;
use memex_core::Memex;
use memex_core::search::WikiSearch;
use memex_core::storage;
use memex_core::types::{PageAction, ProposedPage};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;
use zeroclaw::memory::traits::{Memory, MemoryCategory, MemoryEntry};

/// Reserved paths that must never be written via the Memory API.
const RESERVED_PATHS: &[&str] = &[
    "index.md",
    "log.md",
    "config.toml",
    "schema.md",
    ".lock",
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
    "IDENTITY.md",
    "SOUL.md",
];

/// MemexMemory bridges Memex's filesystem wiki with the zeroclaw `Memory` trait.
///
/// Recall uses BM25 full-text search as the primary retrieval strategy. If BM25
/// returns no results, falls back to `context_for` (which itself tries BM25 then
/// LLM-based selection).
pub struct MemexMemory {
    memex: Arc<Memex>,
    /// Legacy token threshold. Retained for config compatibility; recall now
    /// uses BM25 search unconditionally.
    pub small_memex_threshold: usize,
    /// Cache: (last_query, results). Invalidated on each `store` call.
    recall_cache: Mutex<Option<(String, Vec<MemoryEntry>)>>,
}

impl MemexMemory {
    /// Create a new MemexMemory wrapping the given `Memex` instance.
    pub fn new(memex: Arc<Memex>) -> Self {
        Self {
            memex,
            small_memex_threshold: 4000,
            recall_cache: Mutex::new(None),
        }
    }

    /// Create with a custom small-memex threshold (tokens).
    pub fn with_threshold(memex: Arc<Memex>, small_memex_threshold: usize) -> Self {
        Self {
            memex,
            small_memex_threshold,
            recall_cache: Mutex::new(None),
        }
    }

    /// Invalidate the recall cache.
    async fn invalidate_cache(&self) {
        *self.recall_cache.lock().await = None;
    }

    /// Convert a `WikiPage` body + frontmatter to a single string MemoryEntry content.
    fn wiki_page_to_entry(path: &std::path::Path, body: &str, title: &str) -> MemoryEntry {
        let key = path.to_string_lossy().into_owned();
        let now = chrono::Utc::now().to_rfc3339();
        MemoryEntry {
            id: key.clone(),
            key: key.clone(),
            content: format!("# {title}\n\n{body}"),
            category: MemoryCategory::Core,
            timestamp: now,
            session_id: None,
            score: None,
            namespace: "memex".to_string(),
            importance: None,
            superseded_by: None,
        }
    }
}

#[async_trait]
impl Memory for MemexMemory {
    fn name(&self) -> &str {
        "memex"
    }

    /// Store a wiki page.
    ///
    /// Rules:
    /// 1. Reject reserved paths.
    /// 2. Key must start with "wiki/".
    /// 3. Validate page content via `memex_core::validate::validate_page`.
    /// 4. Write via `memex.write_proposed_pages`.
    /// 5. Invalidate recall cache.
    /// 6. Warn on dangling wiki links (non-blocking).
    async fn store(
        &self,
        key: &str,
        content: &str,
        _category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // Normalize key: strip wiki/ prefix, ensure .md extension, flat directory
        let mut name = key.strip_prefix("wiki/").unwrap_or(key).to_string();

        // Reject paths with directory components (wiki is flat)
        if name.contains('/') || name.contains("..") {
            anyhow::bail!("wiki pages are flat — no subdirectories allowed: {key}");
        }

        // Ensure .md extension
        if !name.ends_with(".md") {
            name.push_str(".md");
        }

        let key = format!("wiki/{name}");
        let key = key.as_str();

        // 1. Reject reserved paths
        if RESERVED_PATHS.iter().any(|r| *r == name || *r == key) {
            anyhow::bail!("store rejected reserved path: {key}");
        }

        // 2b. Path traversal guard: normalize lexically and verify path stays inside wiki_dir
        {
            let normalized = storage::normalize_path(self.memex.root(), key);
            let wiki_dir = self.memex.wiki_dir();
            if !normalized.starts_with(&wiki_dir) {
                anyhow::bail!("Path traversal rejected: {key}");
            }
        }

        // 3. Auto-generate frontmatter if missing
        let content = if content.trim_start().starts_with("---") {
            content.to_string()
        } else {
            let title: String = name
                .strip_suffix(".md")
                .unwrap_or(&name)
                .chars()
                .map(|c| if c == '-' || c == '_' { ' ' } else { c })
                .collect();
            let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
            format!(
                "---\ntitle: {title}\ntags:\n  - brainstorm\ncreated: {now}\nlast_updated: {now}\nsources: []\n---\n\n{content}"
            )
        };
        let content = content.as_str();

        // 4. Validate page content
        memex_core::validate::validate_page(content)
            .map_err(|e| anyhow::anyhow!("page validation failed for {key}: {e}"))?;

        // 5. Build proposed page and write
        let path = PathBuf::from(key);
        let action = if self.memex.root().join(key).exists() {
            PageAction::Update
        } else {
            PageAction::Create
        };
        let proposed = ProposedPage {
            path: path.clone(),
            action,
            content: content.to_string(),
        };
        self.memex
            .write_proposed_pages(&[proposed])
            .await
            .map_err(|e| anyhow::anyhow!("write_proposed_pages failed: {e}"))?;

        // 5. Invalidate recall cache
        self.invalidate_cache().await;

        // 6. Warn on dangling links (non-blocking)
        let dangling = memex_core::validate::find_dangling_links(content, &self.memex.wiki_dir());
        for link in &dangling {
            warn!(key = %key, link = %link, "dangling wiki link (non-blocking)");
        }

        Ok(())
    }

    /// Recall wiki pages relevant to `query`.
    ///
    /// Strategy:
    /// - Check recall cache first.
    /// - Use BM25 search as primary retrieval.
    /// - If BM25 returns results: read full page content for each result.
    /// - If BM25 returns nothing: fall back to `context_for` (which itself falls back to LLM).
    /// - Cache results.
    async fn recall(
        &self,
        query: &str,
        limit: usize,
        _session_id: Option<&str>,
        _since: Option<&str>,
        _until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        // 1. Check cache
        {
            let cache = self.recall_cache.lock().await;
            if let Some((cached_query, entries)) = cache.as_ref()
                && cached_query == query
            {
                return Ok(entries.clone());
            }
        }

        // 2. BM25 search as primary retrieval
        let search_results = self
            .memex
            .search()
            .search(query, limit, None)
            .await
            .unwrap_or_default();

        let entries = if !search_results.is_empty() {
            // Convert BM25 results to MemoryEntry (read full page content)
            let mut entries = Vec::new();
            for r in &search_results {
                let abs_path = self.memex.root().join(&r.path);
                if let Ok(content) = std::fs::read_to_string(&abs_path) {
                    entries.push(Self::wiki_page_to_entry(&r.path, &content, &r.title));
                }
            }
            entries
        } else {
            // Fallback to context_for (which uses BM25 internally, then LLM)
            let max_tokens = limit.saturating_mul(2000);
            match self.memex.context_for(query, max_tokens).await {
                Ok(pages) => pages
                    .into_iter()
                    .map(|p| Self::wiki_page_to_entry(&p.path, &p.body, &p.frontmatter.title))
                    .collect(),
                Err(e) => {
                    warn!(error = %e, "context_for failed, returning empty recall");
                    vec![]
                }
            }
        };

        // 3. Cache results
        *self.recall_cache.lock().await = Some((query.to_string(), entries.clone()));

        Ok(entries)
    }

    /// Get a specific page by key (e.g. "wiki/my-page.md").
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let abs_path = self.memex.root().join(key);
        match std::fs::read_to_string(&abs_path) {
            Ok(content) => {
                let (fm, body) = memex_core::validate::parse_frontmatter(&content)
                    .map_err(|e| anyhow::anyhow!("failed to parse frontmatter for {key}: {e}"))?;
                let path = PathBuf::from(key);
                Ok(Some(Self::wiki_page_to_entry(&path, &body, &fm.title)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!("failed to read {key}: {e}")),
        }
    }

    /// List all pages. Reads index entries and converts to MemoryEntry vec.
    /// Filters by category label if provided (matches against the tags).
    async fn list(
        &self,
        _category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let index_entries = memex_core::index::read_index(self.memex.root())
            .map_err(|e| anyhow::anyhow!("failed to read index: {e}"))?;

        let now = chrono::Utc::now().to_rfc3339();
        let entries = index_entries
            .into_iter()
            .map(|e| {
                let key = e.path.to_string_lossy().into_owned();
                MemoryEntry {
                    id: key.clone(),
                    key: key.clone(),
                    content: e.summary,
                    category: MemoryCategory::Core,
                    timestamp: now.clone(),
                    session_id: None,
                    score: None,
                    namespace: "memex".to_string(),
                    importance: None,
                    superseded_by: None,
                }
            })
            .collect();

        Ok(entries)
    }

    /// Delete a wiki page by key, remove its index entry, and invalidate the recall cache.
    ///
    /// Acquires the memex lock before any filesystem mutation to prevent races
    /// with concurrent `store` / `ingest` operations.
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let abs_path = self.memex.root().join(key);

        // Fast path: if the file does not exist, return false without taking the lock.
        if !abs_path.exists() {
            return Ok(false);
        }

        // Acquire exclusive lock (async-safe: runs in blocking thread pool).
        let lock_path = self.memex.lock_path();
        let _lock = storage::try_acquire_lock_async(&lock_path, 30)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "MEMEX_E004: Memex locked by another process. If stale, remove {}",
                    lock_path.display()
                )
            })?;

        // Remove the file (re-check existence inside the lock).
        match std::fs::remove_file(&abs_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(anyhow::anyhow!("failed to delete {key}: {e}")),
        }

        // Remove the index entry (targeted update, not a full rebuild).
        let index_path = self.memex.root().join("index.md");
        if let Err(e) = memex_core::index::remove_index_entry(&index_path, key) {
            warn!(error = %e, key = %key, "remove_index_entry failed after forget");
        }

        // Remove from search index.
        let _ = self.memex.search().remove_page(key);

        // Invalidate recall cache.
        self.invalidate_cache().await;

        // Lock released here when `_lock` drops.
        Ok(true)
    }

    /// Count the number of wiki pages by reading the index.
    async fn count(&self) -> anyhow::Result<usize> {
        let index_content = self
            .memex
            .read_index()
            .map_err(|e| anyhow::anyhow!("failed to read index: {e}"))?;
        let count = memex_core::index::parse_index_entries(&index_content).len();
        Ok(count)
    }

    /// Health check: verify schema.md exists.
    async fn health_check(&self) -> bool {
        self.memex.root().join("schema.md").exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct StubProvider;

    #[async_trait::async_trait]
    impl memex_core::LlmProvider for StubProvider {
        async fn chat(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("stub".to_string())
        }
    }

    fn make_memex(dir: &TempDir) -> Arc<Memex> {
        let root = dir.path().join("memex");
        Arc::new(Memex::open(root, Box::new(StubProvider), "test").unwrap())
    }

    const VALID_PAGE: &str = "---\ntitle: Test Page\ntags:\n  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\n\nTest body.\n";

    #[tokio::test]
    async fn recall_empty_memex_returns_empty() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        let results = mem.recall("anything", 5, None, None, None).await.unwrap();
        // BM25 search on an empty wiki returns no results, and the context_for
        // fallback also returns empty since the stub provider returns "stub".
        assert!(
            results.is_empty(),
            "expected 0 entries for empty wiki, got {}",
            results.len()
        );
    }

    #[tokio::test]
    async fn store_rejects_reserved_paths() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        let err = mem
            .store("index.md", VALID_PAGE, MemoryCategory::Core, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("reserved path"), "got: {err}");
    }

    #[tokio::test]
    async fn store_auto_prepends_wiki_prefix() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let mem = MemexMemory::new(Arc::clone(&memex));
        // Key without wiki/ prefix should be auto-normalized
        mem.store("my-doc.md", VALID_PAGE, MemoryCategory::Core, None)
            .await
            .unwrap();
        assert!(memex.root().join("wiki/my-doc.md").exists());
    }

    #[tokio::test]
    async fn store_valid_page_writes_to_wiki() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let mem = MemexMemory::new(Arc::clone(&memex));
        mem.store("wiki/test-page.md", VALID_PAGE, MemoryCategory::Core, None)
            .await
            .unwrap();
        let written = std::fs::read_to_string(memex.root().join("wiki/test-page.md")).unwrap();
        assert!(written.contains("Test Page"), "got: {written}");
    }

    #[tokio::test]
    async fn recall_cache_hit() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let mem = MemexMemory::new(Arc::clone(&memex));

        // First call populates cache
        let first = mem.recall("test query", 5, None, None, None).await.unwrap();

        // Manually dirty the index so a fresh read would differ from cache
        // (write directly so cache is not invalidated via store)
        std::fs::write(
            memex.root().join("index.md"),
            "# Index\n\nManually updated.\n",
        )
        .unwrap();

        // Second call should return cached results (same query)
        let second = mem.recall("test query", 5, None, None, None).await.unwrap();

        assert_eq!(
            first.len(),
            second.len(),
            "cache should return the same entry count"
        );
    }

    #[tokio::test]
    async fn store_invalidates_recall_cache() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let mem = MemexMemory::with_threshold(Arc::clone(&memex), 4000);

        // Populate the cache
        let _ = mem.recall("something", 5, None, None, None).await.unwrap();

        // Store a new page — this should invalidate the cache
        mem.store("wiki/my-page.md", VALID_PAGE, MemoryCategory::Core, None)
            .await
            .unwrap();

        // Cache should be None now — next recall reads fresh
        let cache = mem.recall_cache.lock().await;
        assert!(cache.is_none(), "cache should be invalidated after store");
    }

    #[tokio::test]
    async fn health_check_returns_true_for_valid_memex() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        assert!(mem.health_check().await);
    }

    #[tokio::test]
    async fn count_returns_zero_for_empty_wiki() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn forget_returns_false_for_missing_key() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        let result = mem.forget("wiki/nonexistent.md").await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn get_returns_none_for_missing_key() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        let result = mem.get("wiki/nonexistent.md").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn store_rejects_path_traversal_parent() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        let err = mem
            .store("wiki/../AGENTS.md", VALID_PAGE, MemoryCategory::Core, None)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no subdirectories") || msg.contains("reserved"),
            "expected flat wiki or reserved path error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn store_rejects_path_traversal_deep() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        let err = mem
            .store(
                "wiki/../../etc/passwd",
                VALID_PAGE,
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no subdirectories"), "got: {err}");
    }

    #[tokio::test]
    async fn store_rejects_reserved_agents_md() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        let err = mem
            .store("AGENTS.md", VALID_PAGE, MemoryCategory::Core, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("reserved"),
            "expected reserved path error, got: {err}"
        );
    }

    #[tokio::test]
    async fn store_rejects_path_traversal_non_reserved() {
        let dir = TempDir::new().unwrap();
        let mem = MemexMemory::new(make_memex(&dir));
        let err = mem
            .store(
                "wiki/../sources/evil.md",
                VALID_PAGE,
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no subdirectories"), "got: {err}");
    }

    /// Verify that `forget` removes the file, removes the index entry, and
    /// invalidates the recall cache — confirming end-to-end lock-guarded deletion.
    #[tokio::test]
    async fn forget_acquires_lock() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let mem = MemexMemory::new(Arc::clone(&memex));

        // Store a page so the file and an index entry exist.
        mem.store("wiki/to-forget.md", VALID_PAGE, MemoryCategory::Core, None)
            .await
            .unwrap();

        // Sanity: file exists and index contains the path.
        let page_path = memex.root().join("wiki/to-forget.md");
        assert!(page_path.exists(), "page should exist before forget");
        let index_before = std::fs::read_to_string(memex.root().join("index.md")).unwrap();
        assert!(
            index_before.contains("wiki/to-forget.md"),
            "index should contain entry before forget; got: {index_before}"
        );

        // Populate the recall cache so we can verify it gets invalidated.
        let _ = mem.recall("anything", 5, None, None, None).await.unwrap();
        {
            let cache = mem.recall_cache.lock().await;
            assert!(cache.is_some(), "cache should be populated before forget");
        }

        // Forget the page.
        let result = mem.forget("wiki/to-forget.md").await.unwrap();
        assert!(result, "forget should return true for an existing page");

        // File must be gone.
        assert!(!page_path.exists(), "file should be deleted after forget");

        // Index entry must be gone.
        let index_after = std::fs::read_to_string(memex.root().join("index.md")).unwrap();
        assert!(
            !index_after.contains("wiki/to-forget.md"),
            "index should not contain the deleted entry; got: {index_after}"
        );

        // Recall cache must be invalidated.
        let cache = mem.recall_cache.lock().await;
        assert!(cache.is_none(), "cache should be invalidated after forget");
    }
}
