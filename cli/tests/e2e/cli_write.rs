//! E2E tests for the test-only `seed_wiki_page` helper. The helper
//! mirrors the daemon's `handle_write` pipeline (parse frontmatter →
//! atomic_write → index_wiki_file) so these assertions cover the
//! same shape: file lands on disk, slug is normalized, frontmatter
//! validation is enforced, and force-overwrite works.

use crate::common::*;

#[test]
fn write_creates_page() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("REST Patterns", "Resource-oriented design with HTTP verbs.");
    seed_wiki_page(&root, "rest-patterns", &content, false).expect("write should succeed");

    assert!(
        root.join("wiki/rest-patterns.md").exists(),
        "wiki page file should exist"
    );
}

#[test]
fn write_conflict_blocks_create() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("API Design", "Notes about API design.");
    seed_wiki_page(&root, "api-design", &content, false).expect("first write should succeed");

    let content2 = make_page("API Design V2", "Updated notes.");
    let err = seed_wiki_page(&root, "api-design", &content2, false)
        .expect_err("second write without force should fail");
    assert!(
        err.to_string().contains("already exists"),
        "expected 'already exists' in error, got: {err}"
    );
}

#[test]
fn write_force_overwrites() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content1 = make_page("Caching", "Original caching content.");
    seed_wiki_page(&root, "caching", &content1, false).expect("first write should succeed");

    let content2 = make_page("Caching", "Updated caching content with LRU details.");
    seed_wiki_page(&root, "caching", &content2, true).expect("force overwrite should succeed");

    let on_disk = std::fs::read_to_string(root.join("wiki/caching.md")).unwrap();
    assert!(
        on_disk.contains("LRU details"),
        "file should contain updated content, got: {on_disk}"
    );
}

#[test]
fn write_lazy_init() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("brand-new-memex");
    assert!(!root.exists(), "root should not exist before write");

    let content = make_page("First Page", "Hello world.");
    seed_wiki_page(&root, "first-page", &content, false).expect("write should succeed");

    assert!(root.join("wiki").is_dir(), "wiki/ directory should be created");
    assert!(root.join("wiki/first-page.md").exists());
}

#[test]
fn write_normalizes_title_to_kebab_case() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("REST API Design", "Design principles for REST APIs.");
    seed_wiki_page(&root, "REST API Design", &content, false).expect("write should succeed");

    assert!(
        root.join("wiki/rest-api-design.md").exists(),
        "wiki page should use kebab-case filename"
    );
}

#[test]
fn write_rejects_no_frontmatter() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let err = seed_wiki_page(&root, "bad-page", "Just plain text.", false)
        .expect_err("write without frontmatter should fail");
    assert!(
        err.to_string().to_lowercase().contains("frontmatter"),
        "error should mention frontmatter, got: {err}"
    );
}

#[test]
fn write_rejects_empty_title() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = "---\ntitle: \"\"\ntags: []\ncreated_at: 2026-04-14T00:00:00Z\nupdated_at: 2026-04-14T00:00:00Z\nsources: []\n---\n\nBody.\n";
    let err = seed_wiki_page(&root, "empty-title", content, false)
        .expect_err("write with empty title should fail");
    assert!(
        err.to_string().to_lowercase().contains("title"),
        "error should mention title, got: {err}"
    );
}

#[test]
fn write_handles_colon_in_title() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = "---\ntitle: Go: Deep Equal Comparison\ntags: [go]\ncreated_at: 2026-04-14T00:00:00Z\nupdated_at: 2026-04-14T00:00:00Z\nsources: []\n---\n\nComparing structs.\n";
    seed_wiki_page(&root, "go-deep-equal", content, false).expect("colon in title should be handled");

    let disk = std::fs::read_to_string(root.join("wiki/go-deep-equal.md")).unwrap();
    assert!(
        disk.contains("Go: Deep Equal Comparison"),
        "title should be preserved, got: {disk}"
    );
}

#[test]
fn write_force_on_new_page_creates_normally() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("Brand New", "Created via force=true on a new page.");
    seed_wiki_page(&root, "brand-new", &content, true).expect("force on non-existent page should succeed");

    assert!(root.join("wiki/brand-new.md").exists());
}
