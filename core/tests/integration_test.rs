use tempfile::TempDir;

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
    let memex = memex_core::Memex::open(root.clone()).unwrap();

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
