use tempfile::TempDir;

#[test]
fn content_exists_and_source_count() {
    let tmp = TempDir::new().unwrap();
    let memex = memex_core::Memex::open(tmp.path().join("memex")).unwrap();
    let search = memex.search();

    // Initially no content
    let hash = memex_core::storage::content_hash(b"test content");
    assert!(!search.content_exists(&hash).unwrap());
    assert_eq!(search.source_count().unwrap(), 0);

    // Insert content and source document
    let stored_hash = search.insert_content("test content").unwrap();
    assert!(search.content_exists(&stored_hash).unwrap());

    let existing: Vec<String> = search.existing_docids().unwrap().into_iter().collect();
    let docid =
        memex_core::docid::allocate_docid(&stored_hash, "source", "/tmp/test.jsonl", &existing);
    let now = chrono::Utc::now().to_rfc3339();
    search
        .upsert_document(
            "source",
            "/tmp/test.jsonl",
            "test",
            &stored_hash,
            &docid,
            "",
            "",
            &now,
            &now,
        )
        .unwrap();

    assert_eq!(search.source_count().unwrap(), 1);
}

#[test]
fn open_creates_wiki_directory() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let _memex = memex_core::Memex::open(root.clone()).unwrap();
    assert!(root.join("wiki").is_dir());
    assert!(root.join(memex_core::SEARCH_DB_NAME).exists());
}

#[test]
fn open_existing_memex() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    memex_core::Memex::open(root.clone()).unwrap();
    let memex = memex_core::Memex::open(root.clone()).unwrap();
    assert_eq!(memex.root(), root);
}

#[test]
fn lint_detects_dangling_link() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open(root.clone()).unwrap();

    std::fs::write(
        root.join("wiki/page-a.md"),
        "---\ntitle: Page A\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nSee [[ghost-page]] for more.\n",
    ).unwrap();

    let report = memex.lint().unwrap();
    assert!(
        report.issues.iter().any(|i| {
            i.kind == memex_core::types::LintIssueKind::DanglingLink && i.target == "ghost-page"
        }),
        "should detect dangling link to ghost-page"
    );
}

#[test]
fn reindex_populates_search() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open_writer(root.clone()).unwrap();

    std::fs::write(
        root.join("wiki/test-topic.md"),
        "---\ntitle: Test Topic\ntags:\n  - entity\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nSome content about the topic.\n",
    ).unwrap();

    memex.reindex().unwrap();

    let idx = memex.read_index().unwrap();
    assert!(
        idx.contains("Test Topic"),
        "index should contain the page title, got: {idx}"
    );
}
