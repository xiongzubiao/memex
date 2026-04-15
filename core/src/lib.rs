pub mod context;
pub mod error;
pub mod index;
pub mod ingest;
pub mod lint;
pub mod llm_output;
pub mod log;
pub mod model_catalog;
pub mod parsers;
pub mod query;
pub mod schema;
pub mod search;
pub mod source_storage;
pub mod storage;
pub mod types;
pub mod validate;

use std::path::{Path, PathBuf};

use search::WikiSearch;

/// Filename for the BM25 full-text search database.
pub const SEARCH_DB_NAME: &str = ".search.db";

/// Progress callback type for reporting ingest status.
pub type ProgressFn = Box<dyn Fn(&str) + Send + Sync>;

/// A provider that can make LLM chat calls.
///
/// This is memex-core's own trait, decoupled from any agent framework.
/// Callers (CLI, agent) implement this for their provider of choice.
#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String>;
}

/// The Memex knowledge base.
pub struct Memex {
    root: PathBuf,
    provider: Box<dyn LlmProvider>,
    model: String,
    progress: Option<ProgressFn>,
    search: search::Bm25Search,
}

impl std::fmt::Debug for Memex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memex")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Memex {
    /// Open or create a memex at the given root directory.
    /// On first run: creates directory structure, writes schema.md, empty index.md, log.md.
    /// On existing: verifies schema.md version compatibility.
    /// `model` is passed to every LLM call (e.g. "claude-sonnet-4-6").
    pub fn open(
        root: PathBuf,
        provider: Box<dyn LlmProvider>,
        model: impl Into<String>,
    ) -> error::Result<Self> {
        let model = model.into();
        if root.join("schema.md").exists() {
            // Existing memex: verify schema version
            let schema_content = std::fs::read_to_string(root.join("schema.md"))?;
            let version = schema::parse_schema_version(&schema_content).ok_or_else(|| {
                error::MemexError::SchemaVersionMismatch {
                    found: 0,
                    expected: schema::SCHEMA_VERSION,
                }
            })?;
            if version != schema::SCHEMA_VERSION {
                return Err(error::MemexError::SchemaVersionMismatch {
                    found: version,
                    expected: schema::SCHEMA_VERSION,
                });
            }
        } else {
            // First run: scaffold directory structure
            std::fs::create_dir_all(root.join("wiki"))?;
            std::fs::create_dir_all(root.join("sources/brainstorms"))?;
            std::fs::create_dir_all(root.join("sources/documents"))?;
            std::fs::write(root.join("schema.md"), schema::DEFAULT_SCHEMA)?;
            std::fs::write(
                root.join("index.md"),
                format!("# Index\n\n{}\n", crate::index::EMPTY_INDEX_PLACEHOLDER),
            )?;
            std::fs::write(root.join("log.md"), "")?;
        }
        let search = search::Bm25Search::open(&root.join(SEARCH_DB_NAME))?;

        // If wiki/ exists but the search DB is empty, populate from existing pages.
        if root.join("wiki").is_dir() && search.is_empty()? {
            search.rebuild(&root)?;
        }

        Ok(Self {
            root,
            provider,
            model,
            progress: None,
            search,
        })
    }

    /// Set a callback for progress reporting during ingest.
    pub fn set_progress(&mut self, f: impl Fn(&str) + Send + Sync + 'static) {
        self.progress = Some(Box::new(f));
    }

    /// Report progress if a callback is set.
    pub(crate) fn report_progress(&self, msg: &str) {
        if let Some(f) = &self.progress {
            f(msg);
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join(".lock")
    }

    pub fn wiki_dir(&self) -> PathBuf {
        self.root.join("wiki")
    }

    /// The model name passed to every LLM call.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Access the BM25 full-text search index.
    pub fn search(&self) -> &search::Bm25Search {
        &self.search
    }

    pub fn read_index(&self) -> std::io::Result<String> {
        std::fs::read_to_string(self.root.join("index.md"))
    }

    pub fn index_token_count(&self) -> std::io::Result<usize> {
        index::count_index_tokens(&self.root)
    }

    pub fn reindex(&self) -> error::Result<()> {
        let content = index::rebuild_index(&self.root)?;
        storage::atomic_write(&self.root.join("index.md"), content.as_bytes())?;
        self.search.rebuild(&self.root)?;
        let entry_count = crate::index::parse_index_entries(&content).len();
        log::append_log(&self.root, "reindex", &format!("{entry_count} pages"), "")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct StubProvider;

    #[async_trait::async_trait]
    impl LlmProvider for StubProvider {
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

    #[test]
    fn open_creates_directory_structure() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let memex = Memex::open(root.clone(), Box::new(StubProvider), "test").unwrap();
        assert!(root.join("schema.md").exists());
        assert!(root.join("index.md").exists());
        assert!(root.join("log.md").exists());
        assert!(root.join("wiki").is_dir());
        assert!(root.join("sources/brainstorms").is_dir());
        assert!(root.join("sources/documents").is_dir());
        assert_eq!(memex.root(), root);
    }

    #[test]
    fn open_existing_memex() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        Memex::open(root.clone(), Box::new(StubProvider), "test").unwrap();
        let memex = Memex::open(root.clone(), Box::new(StubProvider), "test").unwrap();
        assert_eq!(memex.root(), root);
    }

    #[test]
    fn open_rejects_wrong_schema_version() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("schema.md"), "---\nversion: 99\n---\n# Schema\n").unwrap();
        let err = Memex::open(root, Box::new(StubProvider), "test").unwrap_err();
        assert!(format!("{err}").contains("MEMEX_E009"));
    }

    #[test]
    fn open_rejects_corrupted_schema() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("schema.md"), "garbage no frontmatter").unwrap();
        let err = Memex::open(root, Box::new(StubProvider), "test").unwrap_err();
        assert!(format!("{err}").contains("MEMEX_E009"));
    }

    #[test]
    fn memex_open_creates_search_db() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let _memex = Memex::open(root.clone(), Box::new(StubProvider), "test-model").unwrap();
        assert!(root.join(SEARCH_DB_NAME).exists());
    }

    #[test]
    fn reindex_rebuilds_from_wiki_pages() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let memex = Memex::open(root.clone(), Box::new(StubProvider), "test").unwrap();
        std::fs::write(
            root.join("wiki/test-page.md"),
            "---\ntitle: Test Page\ntags:\n  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\n\nTest content.\n",
        )
        .unwrap();
        memex.reindex().unwrap();
        let idx = std::fs::read_to_string(root.join("index.md")).unwrap();
        assert!(idx.contains("Test Page"), "got: {idx}");
    }
}
