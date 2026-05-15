//! E2E tests for `memex read` (and related sub-commands).
//! Spawns the actual `memex` binary; daemon-mediated commands
//! auto-spawn the daemon via connect_or_spawn.

use crate::common::*;

#[test]
fn read_by_stem() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write via CLI so the page is indexed in the documents table.
    let content = make_page("Caching Strategies", "LRU and TTL-based eviction policies.");
    let out = run_write(&root, "caching", &content, &[]);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let output = memex_cmd(&root, None)
        .args(["read", "caching"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Header format: === {docid} wiki caching ===
    assert!(
        stdout.contains("wiki caching ==="),
        "expected 'wiki caching ===' header, got: {stdout}"
    );
    assert!(
        stdout.contains("LRU and TTL-based eviction policies"),
        "expected body content in output, got: {stdout}"
    );
    // Full .md content should include frontmatter
    assert!(
        stdout.contains("title: Caching Strategies"),
        "expected frontmatter in output, got: {stdout}"
    );
}
#[test]
fn read_by_title() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("Caching Strategies", "LRU and TTL-based eviction policies.");
    let out = run_write(&root, "caching", &content, &[]);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Read by title (case-insensitive).
    let output = memex_cmd(&root, None)
        .args(["read", "caching strategies"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("wiki caching ==="),
        "expected 'wiki caching ===' header, got: {stdout}"
    );
    assert!(
        stdout.contains("LRU and TTL-based eviction policies"),
        "expected body content in output, got: {stdout}"
    );
}
#[test]
fn read_by_docid_prefix() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let body = "Content for docid prefix lookup.";
    let content = make_page("Docid Test", body);
    seed_wiki_page(&root, "docid-test", &content, false).expect("write should succeed");

    // Compute the docid the same way the indexer does: short hash of
    // the post-link body. With no other pages, forward_link is a no-op
    // so the linked body equals the body we wrote (with the make_page
    // trailing newline preserved).
    let body_with_newline = format!("{body}\n");
    let hash = memex_core::storage::content_hash(body_with_newline.as_bytes());
    let docid = memex_core::docid::short(&hash).to_string();

    // Use the first 4 characters as prefix.
    let prefix = &docid[..4.min(docid.len())];

    let output = memex_cmd(&root, None)
        .args(["read", prefix])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("{docid} wiki docid-test ===")),
        "expected full docid in header, got: {stdout}"
    );
    assert!(
        stdout.contains("Content for docid prefix lookup"),
        "expected body content in output, got: {stdout}"
    );
}
#[test]
fn read_not_found() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    std::fs::create_dir_all(root.join("wiki")).unwrap();

    let output = memex_cmd(&root, None)
        .args(["read", "nonexistent-page"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found: nonexistent-page"),
        "expected 'not found: nonexistent-page' in stderr, got: {stderr}"
    );
}
