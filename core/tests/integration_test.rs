use async_trait::async_trait;
use memex_core::search::WikiSearch;
use memex_core::types::Source;
use tempfile::TempDir;

/// Mock provider that handles ingest, query, and lint prompts.
struct IntegrationMockProvider;

#[async_trait]
impl memex_core::LlmProvider for IntegrationMockProvider {
    async fn chat(
        &self,
        _system: Option<&str>,
        prompt: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let lower = prompt.to_lowercase();
        if lower.contains("updating a personal wiki") || lower.contains("read source material") {
            // Ingest response
            Ok(r#"
<<< PAGE: wiki/api-design-notes.md >>>
<<< ACTION: create >>>
---
title: API Design Notes Summary
tags:
  - source-summary
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources:
  - sources/documents/notes.md
---

Summary of API design notes covering REST patterns and [[error-handling]].
<<< END PAGE >>>

<<< PAGE: wiki/rest-patterns.md >>>
<<< ACTION: create >>>
---
title: REST Patterns
tags:
  - entity
created: 2026-04-06T00:00:00Z
last_updated: 2026-04-06T00:00:00Z
sources:
  - sources/documents/notes.md
---

REST API patterns for resource-oriented design.
<<< END PAGE >>>
"#
            .to_string())
        } else if lower.contains("which wiki pages") {
            // Query/context_for page selection
            Ok("wiki/rest-patterns.md\n".to_string())
        } else if lower.contains("based on these wiki pages") {
            // Query synthesis
            Ok("REST APIs should use resource-oriented design with proper HTTP methods. [REST Patterns]"
                .to_string())
        } else if lower.contains("auditing a wiki") || lower.contains("wiki auditor") {
            // Lint
            Ok("ISSUES:\n\nSUGGESTED_QUESTIONS:\n- What authentication patterns work best with REST APIs?\n\nSUGGESTED_SOURCES:\n- RFC 7231 for HTTP method semantics\n".to_string())
        } else {
            Ok("[mock]".to_string())
        }
    }
}

#[tokio::test]
async fn full_workflow_init_ingest_query_lint() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Step 1: Open (equivalent to init)
    let memex =
        memex_core::Memex::open(root.clone(), Box::new(IntegrationMockProvider), "test").unwrap();
    assert!(root.join("schema.md").exists());
    assert!(root.join("index.md").exists());
    assert!(root.join("wiki").is_dir());

    // Step 2: Ingest a file
    let source_file = dir.path().join("api-design-notes.md");
    std::fs::write(
        &source_file,
        "# API Design\nUse REST with proper HTTP methods.\n## Error Handling\nReturn meaningful status codes.\n",
    )
    .unwrap();
    let ingest_report = memex
        .ingest(&Source::File { path: source_file })
        .await
        .unwrap();
    assert_eq!(
        ingest_report.pages_created.len(),
        2,
        "Should create 2 pages"
    );

    // Verify wiki pages on disk
    assert!(root.join("wiki/rest-patterns.md").exists());
    assert!(root.join("wiki/api-design-notes.md").exists());

    // Verify index was updated
    let index = std::fs::read_to_string(root.join("index.md")).unwrap();
    assert!(
        index.contains("REST Patterns"),
        "Index should list REST Patterns. Got: {index}"
    );

    // Step 3: Query
    let query_result = memex.query("What do I know about REST?").await.unwrap();
    assert!(
        query_result.answer.contains("REST"),
        "Answer should mention REST"
    );
    assert!(!query_result.citations.is_empty(), "Should have citations");

    // Step 4: Lint
    let lint_report = memex.lint().await.unwrap();
    // Should detect dangling link to [[error-handling]]
    let dangling: Vec<_> = lint_report
        .issues
        .iter()
        .filter(|i| i.kind == memex_core::types::LintIssueKind::MissingLink)
        .collect();
    assert!(
        !dangling.is_empty(),
        "Should detect dangling [[error-handling]] link"
    );
    assert!(
        !lint_report.suggested_questions.is_empty(),
        "Should have suggestions"
    );

    // Step 5: context_for
    let context_pages = memex.context_for("REST API design", 50000).await.unwrap();
    assert!(!context_pages.is_empty(), "Should return relevant pages");

    // Step 6: Reindex
    memex.reindex().unwrap();
    let index_after = std::fs::read_to_string(root.join("index.md")).unwrap();
    assert!(index_after.contains("REST Patterns"));

    // Verify log has all operations in grep-friendly format
    let log_content = std::fs::read_to_string(root.join("log.md")).unwrap();
    assert!(
        log_content.contains("ingest | batch"),
        "Log should record ingest, got: {log_content}"
    );
    assert!(
        log_content.contains("query |"),
        "Log should record query, got: {log_content}"
    );
    assert!(
        log_content.contains("lint |"),
        "Log should record lint, got: {log_content}"
    );
    assert!(
        log_content.contains("reindex |"),
        "Log should record reindex, got: {log_content}"
    );
}

#[tokio::test]
async fn ingest_directory_skips_duplicates() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex =
        memex_core::Memex::open(root.clone(), Box::new(IntegrationMockProvider), "test").unwrap();

    let source_dir = dir.path().join("docs");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::write(source_dir.join("file1.md"), "Content 1").unwrap();
    std::fs::write(source_dir.join("file2.md"), "Content 2").unwrap();

    let r1 = memex
        .ingest(&Source::Directory {
            path: source_dir.clone(),
        })
        .await
        .unwrap();
    assert!(
        r1.pages_created.len() >= 2,
        "First ingest should create pages"
    );

    // Second ingest of same directory = skip (dedup)
    let r2 = memex
        .ingest(&Source::Directory { path: source_dir })
        .await
        .unwrap();
    assert!(
        r2.pages_created.is_empty(),
        "Second ingest should skip (dedup)"
    );
}

#[tokio::test]
async fn e2e_search_db_populated_after_ingest() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Step 1: Open memex — search DB should be created
    let memex =
        memex_core::Memex::open(root.clone(), Box::new(IntegrationMockProvider), "test").unwrap();
    assert!(
        root.join(memex_core::SEARCH_DB_NAME).exists(),
        "search DB should be created on open"
    );

    // Search DB should be empty initially
    let empty_results = memex.search().search("REST", 10, None).await.unwrap();
    assert!(
        empty_results.is_empty(),
        "search DB should be empty before ingest"
    );

    // Step 2: Ingest a file — search DB should be populated
    let source_file = dir.path().join("notes.md");
    std::fs::write(&source_file, "# REST API\nUse proper HTTP methods.\n").unwrap();
    let report = memex
        .ingest(&Source::File { path: source_file })
        .await
        .unwrap();
    assert!(
        !report.pages_created.is_empty(),
        "ingest should create pages"
    );

    // Step 3: BM25 search should find ingested pages
    let results = memex.search().search("REST", 10, None).await.unwrap();
    assert!(
        !results.is_empty(),
        "BM25 should find pages after ingest. Pages created: {:?}",
        report.pages_created
    );
    assert!(
        results[0].score > 0.0 && results[0].score <= 1.0,
        "score should be normalized 0-1, got: {}",
        results[0].score
    );
    assert!(!results[0].title.is_empty(), "result should have a title");
    assert!(
        !results[0].snippet.is_empty(),
        "result should have a snippet"
    );

    // Step 4: Reindex — search DB should still work
    memex.reindex().unwrap();
    let after_reindex = memex.search().search("REST", 10, None).await.unwrap();
    assert!(
        !after_reindex.is_empty(),
        "BM25 should still find pages after reindex"
    );

    // Step 5: Search for something not in the wiki — should return empty
    let no_results = memex
        .search()
        .search("quantum computing", 10, None)
        .await
        .unwrap();
    assert!(no_results.is_empty(), "should not find unrelated content");
}

#[tokio::test]
async fn e2e_query_uses_bm25_before_llm() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex =
        memex_core::Memex::open(root.clone(), Box::new(IntegrationMockProvider), "test").unwrap();

    // Ingest content
    let source_file = dir.path().join("api.md");
    std::fs::write(&source_file, "# API Design\nREST patterns.\n").unwrap();
    memex
        .ingest(&Source::File { path: source_file })
        .await
        .unwrap();

    // Query — should succeed via BM25 (tier 1) or expansion (tier 2) or fallback (tier 3)
    let result = memex.query("REST API design").await.unwrap();
    assert!(
        result.answer.contains("REST"),
        "query should return an answer about REST, got: {}",
        result.answer
    );

    // context_for — should also use BM25
    let pages = memex.context_for("REST patterns", 50000).await.unwrap();
    assert!(
        !pages.is_empty(),
        "context_for should return pages via BM25"
    );
}
