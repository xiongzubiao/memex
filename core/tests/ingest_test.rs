use async_trait::async_trait;
use memex_core::search::WikiSearch;
use memex_core::types::Source;
use std::path::PathBuf;
use tempfile::TempDir;

struct MockIngestProvider;
struct NonPrefixedPathProvider;

#[async_trait]
impl memex_core::LlmProvider for MockIngestProvider {
    async fn chat(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        // Always return two pages regardless of prompt
        Ok(r#"
<<< PAGE: wiki/notes-md.md >>>
<<< ACTION: create >>>
---
title: Notes Summary
tags:
  - source-summary
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources:
  - sources/documents/abc-notes.md
---

Summary of the notes document.
<<< END PAGE >>>

<<< PAGE: wiki/caching-strategies.md >>>
<<< ACTION: create >>>
---
title: Caching Strategies
tags:
  - entity
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources:
  - sources/documents/abc-notes.md
---

Caching improves performance.
<<< END PAGE >>>
"#
        .to_string())
    }
}

#[async_trait]
impl memex_core::LlmProvider for NonPrefixedPathProvider {
    async fn chat(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(r#"
<<< PAGE: non-prefixed.md >>>
<<< ACTION: create >>>
---
title: Non Prefixed Page
tags:
  - test
created: 2026-04-10T00:00:00Z
last_updated: 2026-04-10T00:00:00Z
sources:
  - sources/documents/raw.md
---

Contains the unique term neonquartz for search indexing regression coverage.
<<< END PAGE >>>
"#
        .to_string())
    }
}

#[tokio::test]
async fn ingest_file_creates_wiki_pages() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex =
        memex_core::Memex::open(root.clone(), Box::new(MockIngestProvider), "test").unwrap();
    let source = dir.path().join("notes.md");
    std::fs::write(&source, "# Notes\nCaching is important.").unwrap();
    let report = memex.ingest(&Source::File { path: source }).await.unwrap();
    assert_eq!(report.pages_created.len(), 2);
    assert!(root.join("wiki/notes-md.md").exists());
    assert!(root.join("wiki/caching-strategies.md").exists());
    let index = std::fs::read_to_string(root.join("index.md")).unwrap();
    assert!(index.contains("Caching Strategies"), "index: {index}");
    let log_content = std::fs::read_to_string(root.join("log.md")).unwrap();
    assert!(log_content.contains("ingest | batch"), "log: {log_content}");
}

#[tokio::test]
async fn ingest_dedup_skips_same_file() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex =
        memex_core::Memex::open(root.clone(), Box::new(MockIngestProvider), "test").unwrap();
    let source = dir.path().join("notes.md");
    std::fs::write(&source, "same content").unwrap();
    let r1 = memex
        .ingest(&Source::File {
            path: source.clone(),
        })
        .await
        .unwrap();
    assert_eq!(r1.pages_created.len(), 2);
    let r2 = memex.ingest(&Source::File { path: source }).await.unwrap();
    assert!(r2.pages_created.is_empty()); // dedup
}

#[tokio::test]
async fn ingest_non_prefixed_paths_are_written_and_search_indexed() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex =
        memex_core::Memex::open(root.clone(), Box::new(NonPrefixedPathProvider), "test").unwrap();
    let source = dir.path().join("raw.md");
    std::fs::write(&source, "# Raw\ncontent").unwrap();

    let report = memex.ingest(&Source::File { path: source }).await.unwrap();
    assert_eq!(
        report.pages_created,
        vec![PathBuf::from("wiki/non-prefixed.md")]
    );
    assert!(root.join("wiki/non-prefixed.md").exists());

    let hits = memex.search().search("neonquartz", 10, None).await.unwrap();
    assert!(
        !hits.is_empty(),
        "search should index page written from non-prefixed path"
    );
    assert_eq!(hits[0].path, PathBuf::from("wiki/non-prefixed.md"));
}
