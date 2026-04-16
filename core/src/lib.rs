pub mod content;
pub mod crosslink;
pub mod docid;
pub mod embed;
pub mod error;
pub mod index;
pub mod lint;
pub mod schema;
pub mod search;
pub mod storage;
pub mod types;
pub mod validate;
pub mod vector;

use std::path::{Path, PathBuf};

use search::WikiSearch;

/// Filename for the BM25 full-text search database.
pub const SEARCH_DB_NAME: &str = ".search.db";

/// The Memex knowledge base.
pub struct Memex {
    root: PathBuf,
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
    /// On first run: creates the wiki directory.
    /// On existing: opens the search index.
    pub fn open(root: PathBuf) -> error::Result<Self> {
        if !root.join("wiki").is_dir() {
            std::fs::create_dir_all(root.join("wiki"))?;
        }
        let search = search::Bm25Search::open(&root.join(SEARCH_DB_NAME))?;
        if search.is_empty()? {
            search.rebuild(&root)?;
        }
        Ok(Self { root, search })
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

    /// Access the BM25 full-text search index.
    pub fn search(&self) -> &search::Bm25Search {
        &self.search
    }

    pub fn read_index(&self) -> error::Result<String> {
        self.search.generate_index()
    }

    pub fn reindex(&self) -> error::Result<()> {
        self.search.rebuild(&self.root)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn reindex_rebuilds_from_wiki_pages() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("memex");
        let memex = Memex::open(root.clone()).unwrap();
        std::fs::write(
            root.join("wiki/test-page.md"),
            "---\ntitle: Test Page\ntags:\n  - entity\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nTest content.\n",
        )
        .unwrap();
        memex.reindex().unwrap();
        let idx = memex.read_index().unwrap();
        assert!(idx.contains("Test Page"), "got: {idx}");
    }
}
