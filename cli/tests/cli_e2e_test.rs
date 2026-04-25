use memex_core::schema::register_sqlite_vec_once;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn memex_cmd(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_memex"));
    cmd.env("MEMEX_ROOT", root);
    cmd
}

#[test]
fn search_empty_wiki() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    std::fs::create_dir_all(root.join("wiki")).unwrap();

    let output = memex_cmd(&root)
        .args(["search", "anything"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "search should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().is_empty(),
        "empty wiki should produce no output, got: {stdout}"
    );
}

#[test]
fn ingest_and_backfill_help_show_collection_flag() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let ingest_help = memex_cmd(&root)
        .args(["ingest", "--help"])
        .output()
        .unwrap();
    assert!(
        ingest_help.status.success(),
        "ingest --help should succeed, stderr: {}",
        String::from_utf8_lossy(&ingest_help.stderr)
    );
    let ingest_stdout = String::from_utf8_lossy(&ingest_help.stdout);
    assert!(
        ingest_stdout.contains("--collection"),
        "ingest help should show --collection flag, got: {ingest_stdout}"
    );

    let backfill_help = memex_cmd(&root)
        .args(["backfill", "--help"])
        .output()
        .unwrap();
    assert!(
        backfill_help.status.success(),
        "backfill --help should succeed, stderr: {}",
        String::from_utf8_lossy(&backfill_help.stderr)
    );
    let backfill_stdout = String::from_utf8_lossy(&backfill_help.stdout);
    assert!(
        backfill_stdout.contains("--collection"),
        "backfill help should show --collection flag, got: {backfill_stdout}"
    );
}

#[test]
fn search_with_content() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write via CLI so the page is indexed in the DB before searching.
    let content = make_page(
        "Caching Strategies",
        "Content about caching and performance.",
    );
    let out = run_write(&root, "caching", &content, &[]);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let output = memex_cmd(&root)
        .args(["search", "Caching Strategies"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "search should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "caching", "should print slug, got: {stdout}");
}

#[test]
fn search_query_sanitization_hyphens() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a wiki page containing hyphenated terms.
    let content = make_page(
        "Multi-Agent Architecture",
        "A multi-agent system for distributed task coordination.",
    );
    let out = run_write(&root, "multi-agent", &content, &[]);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Search by title with hyphenated term.
    let output = memex_cmd(&root)
        .args(["search", "Multi-Agent Architecture"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "search should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "multi-agent",
        "should print slug, got: {stdout}"
    );
}

#[test]
fn search_query_sanitization_phrases() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write wiki pages.
    let content = make_page(
        "API Rate Limiting",
        "Token bucket and sliding window approaches to API rate limiting.",
    );
    let out = run_write(&root, "api-rate-limiting", &content, &[]);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Search by page title.
    let output = memex_cmd(&root)
        .args(["search", "API Rate Limiting"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "search should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "api-rate-limiting",
        "should print slug, got: {stdout}"
    );
}

fn write_wiki_page(root: &std::path::Path, filename: &str, title: &str, body: &str) {
    std::fs::create_dir_all(root.join("wiki")).unwrap();
    let content = format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    );
    std::fs::write(root.join("wiki").join(filename), content).unwrap();
}

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

    let output = memex_cmd(&root).args(["read", "caching"]).output().unwrap();
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
    let output = memex_cmd(&root)
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

    let content = make_page("Docid Test", "Content for docid prefix lookup.");
    let out = run_write(&root, "docid-test", &content, &[]);
    let stdout_write = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Extract docid from "written: <docid>".
    let docid = stdout_write
        .lines()
        .find(|l| l.starts_with("written:"))
        .and_then(|l| l.strip_prefix("written:"))
        .map(|s| s.trim().to_string())
        .expect("expected 'written: <docid>' in output");

    // Use the first 4 characters as prefix.
    let prefix = &docid[..4.min(docid.len())];

    let output = memex_cmd(&root).args(["read", prefix]).output().unwrap();
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

    let output = memex_cmd(&root)
        .args(["read", "nonexistent-page"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found: nonexistent-page"),
        "expected 'not found: nonexistent-page' in stderr, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Write command E2E tests
// ---------------------------------------------------------------------------

fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

/// Pipe `content` into `memex write <name> [extra_args...]`.
fn run_write(
    root: &std::path::Path,
    name: &str,
    content: &str,
    extra_args: &[&str],
) -> std::process::Output {
    use std::io::Write;
    let mut cmd = memex_cmd(root);
    cmd.args(["write", "--direct", name]);
    cmd.args(extra_args);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn write_creates_page() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    // Do not pre-create wiki/ — lazy init should handle it.

    let content = make_page("REST Patterns", "Resource-oriented design with HTTP verbs.");
    let output = run_write(&root, "rest-patterns", &content, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "write should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("written:"),
        "expected 'written:' in output, got: {stdout}"
    );
    assert!(
        stdout.contains("wiki_pages:"),
        "expected 'wiki_pages:' in output, got: {stdout}"
    );
    // File should exist on disk.
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
    // First write succeeds.
    let out1 = run_write(&root, "api-design", &content, &[]);
    assert!(out1.status.success(), "first write should succeed");

    // Second write (same name, no --update) should show conflict.
    let content2 = make_page("API Design V2", "Updated notes.");
    let out2 = run_write(&root, "api-design", &content2, &[]);
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains("conflict:"),
        "expected 'conflict:' in output, got: {stdout2}"
    );
    assert!(
        !stdout2.contains("written:"),
        "should not write on conflict, got: {stdout2}"
    );
}

#[test]
fn write_force_overwrites() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content1 = make_page("Caching", "Original caching content.");
    let out1 = run_write(&root, "caching", &content1, &[]);
    assert!(out1.status.success(), "first write should succeed");

    let content2 = make_page("Caching", "Updated caching content with LRU details.");
    let out2 = run_write(&root, "caching", &content2, &["--force"]);
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert!(
        out2.status.success(),
        "force write should succeed, stderr: {stderr2}"
    );
    assert!(
        stdout2.contains("written:"),
        "expected 'written:' in output, got: {stdout2}"
    );

    // Verify file content was updated.
    let on_disk = std::fs::read_to_string(root.join("wiki/caching.md")).unwrap();
    assert!(
        on_disk.contains("LRU details"),
        "file should contain updated content, got: {on_disk}"
    );
}

#[test]
fn write_quiet_suppresses_suggestions() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page(
        "Auth Tokens",
        "Bearer tokens and [[nonexistent-page]] references.",
    );
    let output = run_write(&root, "auth-tokens", &content, &["--quiet"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "write should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("written:"),
        "expected 'written:', got: {stdout}"
    );
    assert!(
        !stdout.contains("linked:"),
        "quiet mode should suppress 'linked:', got: {stdout}"
    );
    assert!(
        !stdout.contains("suggest-create:"),
        "quiet mode should suppress 'suggest-create:', got: {stdout}"
    );
}

#[test]
fn write_auto_links_existing_pages() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write page A.
    let content_a = make_page("REST Patterns", "Resource-oriented API design.");
    let out_a = run_write(&root, "rest-patterns", &content_a, &[]);
    assert!(out_a.status.success(), "write A should succeed");

    // Write page B that mentions page A's title.
    let content_b = make_page(
        "API Guide",
        "This guide follows REST Patterns for all endpoints.",
    );
    let out_b = run_write(&root, "api-guide", &content_b, &[]);
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    assert!(out_b.status.success(), "write B should succeed");

    // Page B's file should contain a wiki link to A.
    let on_disk = std::fs::read_to_string(root.join("wiki/api-guide.md")).unwrap();
    assert!(
        on_disk.contains("[[rest-patterns]]"),
        "page B should auto-link to page A, got: {on_disk}"
    );
    // Output should mention the forward link.
    assert!(
        stdout_b.contains("linked:") && stdout_b.contains("rest-patterns"),
        "expected 'linked: rest-patterns' in output, got: {stdout_b}"
    );
}

#[test]
fn write_lazy_init() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("brand-new-memex");
    // Root does not exist at all.
    assert!(!root.exists(), "root should not exist before write");

    let content = make_page("First Page", "Hello world.");
    let output = run_write(&root, "first-page", &content, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "write should succeed with lazy init, stderr: {stderr}"
    );
    assert!(
        root.join("wiki").is_dir(),
        "wiki/ directory should be created"
    );
    assert!(
        root.join("wiki/first-page.md").exists(),
        "page file should exist"
    );
}

// ---------------------------------------------------------------------------
// Delete command E2E tests
// ---------------------------------------------------------------------------

#[test]
fn delete_removes_page() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page via the write command so it is indexed.
    let content = make_page("Delete Me", "Temporary page content.");
    let out = run_write(&root, "delete-me", &content, &[]);
    assert!(out.status.success(), "write should succeed");

    // Delete it with --force (non-interactive).
    let output = memex_cmd(&root)
        .args(["delete", "delete-me", "--force"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "delete should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("deleted:"),
        "expected 'deleted:' in output, got: {stdout}"
    );
    assert!(
        stdout.contains("wiki_pages:"),
        "expected 'wiki_pages:' in output, got: {stdout}"
    );
    // File should be gone.
    assert!(
        !root.join("wiki/delete-me.md").exists(),
        "wiki page file should be deleted"
    );
}

#[test]
fn delete_reports_dangling_links() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write page B first (to be deleted).
    let content_b = make_page("Page B", "Content of page B.");
    let out_b = run_write(&root, "page-b", &content_b, &[]);
    assert!(out_b.status.success(), "write B should succeed");

    // Write page A that explicitly links to [[page-b]].
    let content_a = make_page("Page A", "See [[page-b]] for details.");
    let out_a = run_write(&root, "page-a", &content_a, &["--force"]);
    assert!(out_a.status.success(), "write A should succeed");

    // Delete page B with --force.
    let output = memex_cmd(&root)
        .args(["delete", "page-b", "--force"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "delete should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("dangling:") && stdout.contains("page-a"),
        "expected 'dangling: page-a' in output, got: {stdout}"
    );
}

#[test]
fn delete_not_found() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    std::fs::create_dir_all(root.join("wiki")).unwrap();

    let output = memex_cmd(&root)
        .args(["delete", "no-such-page", "--force"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "delete of nonexistent page should fail"
    );
    assert!(
        stderr.contains("not found:") || stderr.contains("no-such-page"),
        "expected error message mentioning missing page, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Lint command E2E tests
// ---------------------------------------------------------------------------

#[test]
fn lint_finds_dangling_link() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page with a link to a nonexistent page.
    write_wiki_page(
        &root,
        "dangling-page.md",
        "Dangling Page",
        "See [[nonexistent-target]] for details.",
    );

    let output = memex_cmd(&root).args(["lint"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "lint should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("dangling:"),
        "expected 'dangling:' in lint output, got: {stdout}"
    );
}

#[test]
fn lint_clean_wiki() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write via CLI so the page is indexed in the DB; otherwise lint reports
    // an untracked-file issue and never prints "No issues found."
    let content = make_page("Clean Page", "This page has no broken links whatsoever.");
    let out = run_write(&root, "clean-page", &content, &[]);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let output = memex_cmd(&root).args(["lint"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "lint should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("No issues found."),
        "expected 'No issues found.' for clean wiki, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Docid stability on --force overwrite
// ---------------------------------------------------------------------------

#[test]
fn write_force_preserves_docid() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page.
    let content1 = make_page("Stable ID", "Original content for docid stability test.");
    let out1 = run_write(&root, "stable-id", &content1, &[]);
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    assert!(
        out1.status.success(),
        "first write should succeed, stderr: {}",
        String::from_utf8_lossy(&out1.stderr)
    );

    // Extract the docid from the first write.
    let docid1 = stdout1
        .lines()
        .find(|l| l.starts_with("written:"))
        .and_then(|l| l.strip_prefix("written:"))
        .map(|s| s.trim().to_string())
        .expect("expected 'written: <docid>' in first output");

    // Update the page with --force.
    let content2 = make_page("Stable ID", "Updated content for docid stability test.");
    let out2 = run_write(&root, "stable-id", &content2, &["--force"]);
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        out2.status.success(),
        "force write should succeed, stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );

    // Extract the docid from the second write.
    let docid2 = stdout2
        .lines()
        .find(|l| l.starts_with("written:"))
        .and_then(|l| l.strip_prefix("written:"))
        .map(|s| s.trim().to_string())
        .expect("expected 'written: <docid>' in second output");

    // Docid must be identical across create and update.
    assert_eq!(
        docid1, docid2,
        "docid should be stable across updates: first={docid1}, second={docid2}"
    );
}

// ---------------------------------------------------------------------------
// Source support
// ---------------------------------------------------------------------------

#[test]
fn write_creates_wiki_page_with_source() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Create a source file on disk.
    let source_path = dir.path().join("meeting-notes.txt");
    std::fs::write(
        &source_path,
        "Meeting notes from 2026-04-10: discussed caching strategies.\n",
    )
    .unwrap();

    let content = make_page(
        "Caching Strategy",
        "We decided on LRU-based caching with TTL eviction.",
    );
    let source_arg = source_path.to_string_lossy().to_string();
    let output = run_write(
        &root,
        "caching-strategy",
        &content,
        &["--source", &source_arg],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "write with source should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("written:"),
        "expected 'written:' in output, got: {stdout}"
    );
    assert!(
        stdout.contains("wiki_pages:"),
        "expected 'wiki_pages:' in output, got: {stdout}"
    );
    // Wiki page should exist on disk.
    assert!(
        root.join("wiki/caching-strategy.md").exists(),
        "wiki page file should exist"
    );
}

// ---------------------------------------------------------------------------
// Filename normalization (kebab-case)
// ---------------------------------------------------------------------------

#[test]
fn write_normalizes_title_to_kebab_case() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("REST API Design", "Design principles for REST APIs.");
    let output = run_write(&root, "REST API Design", &content, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "write with title should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("written:"),
        "expected 'written:' in output, got: {stdout}"
    );
    // File should be kebab-cased.
    assert!(
        root.join("wiki/rest-api-design.md").exists(),
        "wiki page should use kebab-case filename"
    );
}

// ---------------------------------------------------------------------------
// Task 8A: Delete removes file and DB row
// ---------------------------------------------------------------------------

#[test]
fn delete_removes_file_and_db_row() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page.
    let content = make_page("Ephemeral", "This page will be deleted.");
    let out = run_write(&root, "ephemeral", &content, &[]);
    assert!(out.status.success(), "write should succeed");

    // Verify the page exists on disk and is findable.
    assert!(root.join("wiki/ephemeral.md").exists());
    let read_out = memex_cmd(&root)
        .args(["read", "ephemeral"])
        .output()
        .unwrap();
    assert!(
        read_out.status.success(),
        "read should find the page before delete"
    );

    // Delete it.
    let del_out = memex_cmd(&root)
        .args(["delete", "ephemeral", "--force"])
        .output()
        .unwrap();
    let del_stdout = String::from_utf8_lossy(&del_out.stdout);
    let del_stderr = String::from_utf8_lossy(&del_out.stderr);
    assert!(
        del_out.status.success(),
        "delete should succeed, stderr: {del_stderr}"
    );
    assert!(
        del_stdout.contains("deleted:"),
        "expected 'deleted:' in output, got: {del_stdout}"
    );
    assert!(
        del_stdout.contains("wiki_pages: 0"),
        "expected 'wiki_pages: 0', got: {del_stdout}"
    );

    // File should be gone from disk.
    assert!(
        !root.join("wiki/ephemeral.md").exists(),
        "file should be deleted from disk"
    );

    // DB row should be gone — read should report not found.
    let read_after = memex_cmd(&root)
        .args(["read", "ephemeral"])
        .output()
        .unwrap();
    let read_stderr = String::from_utf8_lossy(&read_after.stderr);
    assert!(
        read_stderr.contains("not found"),
        "expected 'not found' after delete, got stderr: {read_stderr}"
    );
}

#[test]
fn delete_reports_dangling_links_with_stems() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write page B first.
    let content_b = make_page("Target Page", "This page is a target.");
    let out_b = run_write(&root, "target-page", &content_b, &[]);
    assert!(out_b.status.success(), "write B should succeed");

    // Write page A that links to B.
    let content_a = make_page("Source Page", "See [[target-page]] for details.");
    let out_a = run_write(&root, "source-page", &content_a, &["--force"]);
    assert!(out_a.status.success(), "write A should succeed");

    // Delete B.
    let del_out = memex_cmd(&root)
        .args(["delete", "target-page", "--force"])
        .output()
        .unwrap();
    let del_stdout = String::from_utf8_lossy(&del_out.stdout);
    let del_stderr = String::from_utf8_lossy(&del_out.stderr);
    assert!(
        del_out.status.success(),
        "delete should succeed, stderr: {del_stderr}"
    );
    assert!(
        del_stdout.contains("dangling:") && del_stdout.contains("source-page"),
        "expected dangling report with 'source-page', got: {del_stdout}"
    );
}

// ---------------------------------------------------------------------------
// Embedding: chunks table populated on write
// ---------------------------------------------------------------------------

#[test]
fn write_populates_chunks_table() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page(
        "Embedding Test",
        "This page should be chunked and embedded after write.",
    );
    let out = run_write(&root, "embedding-test", &content, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {stderr}"
    );

    // Open the search DB and verify chunks were stored.
    let db_path = root.join(".search.db");
    assert!(db_path.exists(), "search DB should exist");
    register_sqlite_vec_once();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let chunk_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
        .unwrap();
    assert!(
        chunk_count > 0,
        "expected at least 1 chunk after write, got {chunk_count}"
    );

    // Verify the chunk has a non-null embedding blob.
    let has_embedding: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM chunks_vec WHERE embedding IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(has_embedding, "chunks should have non-null embeddings");

    // Verify the model name (hash-embedding when the ONNX model is unavailable in CI).
    let model: String = conn
        .query_row("SELECT model FROM chunks LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert!(
        model == "hash-embedding" || model == "embedding-gemma-300m",
        "unexpected model name: {model}"
    );
}

#[test]
fn write_source_populates_chunks_table() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Create a source file.
    let source_path = dir.path().join("notes.txt");
    std::fs::write(
        &source_path,
        "Source document content for embedding test.\n",
    )
    .unwrap();

    let content = make_page("With Source", "A page with an attached source document.");
    let source_arg = source_path.to_string_lossy().to_string();
    let out = run_write(&root, "with-source", &content, &["--source", &source_arg]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "write with source should succeed, stderr: {stderr}"
    );

    // Open DB and count distinct hashes in chunks table.
    let db_path = root.join(".search.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let distinct_hashes: i64 = conn
        .query_row("SELECT COUNT(DISTINCT hash) FROM chunks", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        distinct_hashes >= 2,
        "expected chunks for both wiki page and source doc, got {distinct_hashes} distinct hashes"
    );
}

#[test]
fn write_force_replaces_chunks() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write initial version.
    let content_v1 = make_page("Versioned Page", "Version one content.");
    let out1 = run_write(&root, "versioned-page", &content_v1, &[]);
    assert!(out1.status.success(), "first write should succeed");

    // Count chunks from first write.
    let db_path = root.join(".search.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count_v1: i64 = conn
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
        .unwrap();
    assert!(count_v1 > 0, "should have chunks after first write");

    // Overwrite with different content.
    let content_v2 = make_page(
        "Versioned Page",
        "Version two content is completely different.",
    );
    let out2 = run_write(&root, "versioned-page", &content_v2, &["--force"]);
    assert!(out2.status.success(), "force write should succeed");

    // Re-open DB (connection may be stale after CLI invocation).
    let conn2 = rusqlite::Connection::open(&db_path).unwrap();
    let count_v2: i64 = conn2
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
        .unwrap();
    assert!(
        count_v2 > 0,
        "should have chunks after second write, got {count_v2}"
    );

    // Verify the chunk text contains the new version's content.
    let chunk_text: String = conn2
        .query_row("SELECT chunk_text FROM chunks LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        chunk_text.contains("Version two") || chunk_text.contains("completely different"),
        "chunk text should reflect updated content, got: {chunk_text}"
    );
}

// ---------------------------------------------------------------------------
// Task 8B: Lint stale-index detection and --fix
// ---------------------------------------------------------------------------

#[test]
fn lint_detects_stale_index() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page via the CLI (indexes it in DB).
    let content = make_page(
        "REST Patterns",
        "Resource-oriented design with proper HTTP verbs.",
    );
    let out = run_write(&root, "rest-patterns", &content, &[]);
    assert!(out.status.success(), "write should succeed");

    // Modify the file on disk directly (no re-index).
    std::fs::write(
        root.join("wiki/rest-patterns.md"),
        "---\ntitle: REST Patterns\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\nCompletely rewritten content about REST and GraphQL.\n",
    )
    .unwrap();

    // Run lint — should detect stale-index.
    let lint_out = memex_cmd(&root).args(["lint"]).output().unwrap();
    let lint_stdout = String::from_utf8_lossy(&lint_out.stdout);
    let lint_stderr = String::from_utf8_lossy(&lint_out.stderr);
    assert!(
        lint_out.status.success(),
        "lint should succeed, stderr: {lint_stderr}"
    );
    assert!(
        lint_stdout.contains("stale-index:") && lint_stdout.contains("rest-patterns"),
        "expected 'stale-index: rest-patterns' in lint output, got: {lint_stdout}"
    );
}

#[test]
fn lint_fix_reindexes_stale() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page via CLI.
    let content = make_page(
        "REST Patterns",
        "Resource-oriented design with proper HTTP verbs.",
    );
    let out = run_write(&root, "rest-patterns", &content, &[]);
    assert!(out.status.success(), "write should succeed");

    // Modify file on disk.
    std::fs::write(
        root.join("wiki/rest-patterns.md"),
        "---\ntitle: REST Patterns\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\nCompletely rewritten content about REST and GraphQL.\n",
    )
    .unwrap();

    // Run lint --fix.
    let fix_out = memex_cmd(&root).args(["lint", "--fix"]).output().unwrap();
    let fix_stdout = String::from_utf8_lossy(&fix_out.stdout);
    let fix_stderr = String::from_utf8_lossy(&fix_out.stderr);
    assert!(
        fix_out.status.success(),
        "lint --fix should succeed, stderr: {fix_stderr}"
    );
    assert!(
        fix_stdout.contains("fixed:") && fix_stdout.contains("rest-patterns"),
        "expected 'fixed: rest-patterns' in output, got: {fix_stdout}"
    );
    // stale-index should NOT appear after fix.
    assert!(
        !fix_stdout.contains("stale-index:"),
        "stale-index should be fixed, got: {fix_stdout}"
    );

    // Run lint again — should be clean now.
    let lint_out = memex_cmd(&root).args(["lint"]).output().unwrap();
    let lint_stdout = String::from_utf8_lossy(&lint_out.stdout);
    assert!(
        !lint_stdout.contains("stale-index:"),
        "second lint should not find stale-index, got: {lint_stdout}"
    );
}

#[test]
fn lint_fix_re_embeds_outdated_model() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page via CLI (creates chunks with current model).
    let content = make_page(
        "Embedding Test",
        "Content that will be embedded and then marked as outdated.",
    );
    let out = run_write(&root, "embedding-test", &content, &[]);
    assert!(out.status.success(), "write should succeed");

    // Manually update all chunks.model to "old-model" in the DB.
    let db_path = root.join(".search.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute("UPDATE chunks SET model = 'old-model'", [])
        .unwrap();
    drop(conn);

    // Run lint (without --fix) — should report outdated embeddings.
    let lint_out = memex_cmd(&root).args(["lint"]).output().unwrap();
    let lint_stdout = String::from_utf8_lossy(&lint_out.stdout);
    assert!(
        lint_out.status.success(),
        "lint should succeed, stderr: {}",
        String::from_utf8_lossy(&lint_out.stderr)
    );
    assert!(
        lint_stdout.contains("outdated-embeddings:"),
        "should report outdated embeddings, got: {lint_stdout}"
    );
    assert!(
        lint_stdout.contains("old-model"),
        "should mention old model name, got: {lint_stdout}"
    );

    // Run lint --fix — should re-embed.
    let fix_out = memex_cmd(&root).args(["lint", "--fix"]).output().unwrap();
    let fix_stdout = String::from_utf8_lossy(&fix_out.stdout);
    let fix_stderr = String::from_utf8_lossy(&fix_out.stderr);
    assert!(
        fix_out.status.success(),
        "lint --fix should succeed, stderr: {fix_stderr}"
    );
    assert!(
        fix_stdout.contains("re-embedded:"),
        "expected 're-embedded:' in output, got: {fix_stdout}"
    );

    // Verify chunks now have current model by running lint again.
    let lint2_out = memex_cmd(&root).args(["lint"]).output().unwrap();
    let lint2_stdout = String::from_utf8_lossy(&lint2_out.stdout);
    assert!(
        !lint2_stdout.contains("outdated-embeddings:"),
        "second lint should not find outdated embeddings, got: {lint2_stdout}"
    );
}

#[test]
fn search_probe_includes_vector() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write a page about caching.
    let content = make_page(
        "Caching Strategies",
        "LRU and TTL-based eviction policies for in-memory caches.",
    );
    let out = run_write(&root, "caching", &content, &[]);
    assert!(
        out.status.success(),
        "write should succeed, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Search by title.
    let output = memex_cmd(&root)
        .args(["search", "Caching Strategies"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "search should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "caching", "should print slug, got: {stdout}");
}

// ---------------------------------------------------------------------------
// Backlink tests
// ---------------------------------------------------------------------------

#[test]
fn write_triggers_backlink_into_existing_page() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Page A mentions "Rust Ownership" in its body (no link yet).
    let page_a = make_page(
        "Memory Management",
        "Systems languages differ. Rust Ownership is one approach.",
    );
    let out_a = run_write(&root, "memory-management", &page_a, &[]);
    assert!(out_a.status.success());

    // Page B is titled "Rust Ownership" — should backlink into page A.
    let page_b = make_page(
        "Rust Ownership",
        "Every value in Rust has exactly one owner.",
    );
    let out_b = run_write(&root, "rust-ownership", &page_b, &[]);
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    assert!(out_b.status.success());
    assert!(
        stdout_b.contains("backlinked: memory-management"),
        "expected backlink into memory-management, got: {stdout_b}"
    );

    // Page A on disk should now contain [[rust-ownership]].
    let page_a_disk = std::fs::read_to_string(root.join("wiki/memory-management.md")).unwrap();
    assert!(
        page_a_disk.contains("[[rust-ownership]]"),
        "page A should have backlink, got: {page_a_disk}"
    );
}

// ---------------------------------------------------------------------------
// Validation error tests
// ---------------------------------------------------------------------------

#[test]
fn write_rejects_no_frontmatter() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let output = run_write(&root, "bad-page", "Just plain text.", &[]);
    assert!(
        !output.status.success(),
        "write without frontmatter should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("frontmatter"),
        "error should mention frontmatter, got: {stderr}"
    );
}

#[test]
fn write_rejects_empty_title() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = "---\ntitle: \"\"\ntags: []\ncreated_at: 2026-04-14T00:00:00Z\nupdated_at: 2026-04-14T00:00:00Z\nsources: []\n---\n\nBody.\n";
    let output = run_write(&root, "empty-title", content, &[]);
    assert!(
        !output.status.success(),
        "write with empty title should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("title"),
        "error should mention title, got: {stderr}"
    );
}

#[test]
fn write_handles_colon_in_title() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = "---\ntitle: Go: Deep Equal Comparison\ntags: [go]\ncreated_at: 2026-04-14T00:00:00Z\nupdated_at: 2026-04-14T00:00:00Z\nsources: []\n---\n\nComparing structs.\n";
    let output = run_write(&root, "go-deep-equal", content, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "colon in title should be handled, stderr: {stderr}"
    );

    let disk = std::fs::read_to_string(root.join("wiki/go-deep-equal.md")).unwrap();
    assert!(
        disk.contains("Go: Deep Equal Comparison"),
        "title should be preserved, got: {disk}"
    );
}

// ---------------------------------------------------------------------------
// Delete + lint dangling integration
// ---------------------------------------------------------------------------

#[test]
fn lint_detects_dangling_after_delete() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Write page A, then page B that auto-links to A.
    let page_a = make_page("Caching", "LRU eviction policies.");
    run_write(&root, "caching", &page_a, &[]);

    let page_b = make_page(
        "Performance",
        "Improve performance with Caching and indexing.",
    );
    run_write(&root, "performance", &page_b, &[]);

    // Verify the link exists.
    let disk_b = std::fs::read_to_string(root.join("wiki/performance.md")).unwrap();
    assert!(disk_b.contains("[[caching]]"));

    // Delete the target page.
    let del = memex_cmd(&root)
        .args(["delete", "caching", "--force"])
        .output()
        .unwrap();
    assert!(del.status.success());

    // Lint should find the dangling link.
    let lint = memex_cmd(&root).args(["lint"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&lint.stdout);
    assert!(
        stdout.contains("dangling:") && stdout.contains("caching"),
        "lint should detect dangling link to deleted page, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// Multiple sources
// ---------------------------------------------------------------------------

#[test]
fn write_with_multiple_sources() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let src_a = dir.path().join("notes-a.txt");
    let src_b = dir.path().join("notes-b.txt");
    std::fs::write(&src_a, "Alpha source content about caching.\n").unwrap();
    std::fs::write(&src_b, "Beta source content about networking.\n").unwrap();

    let content = make_page("Multi-Source Page", "A page with two source attachments.");
    let output = run_write(
        &root,
        "multi-source",
        &content,
        &[
            "--source",
            src_a.to_str().unwrap(),
            "--source",
            src_b.to_str().unwrap(),
        ],
    );
    assert!(output.status.success());
}

// ---------------------------------------------------------------------------
// --force on non-existent page
// ---------------------------------------------------------------------------

#[test]
fn write_force_on_new_page_creates_normally() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("Brand New", "Created via --force on a new page.");
    let output = run_write(&root, "brand-new", &content, &["--force"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "--force on non-existent page should succeed, stderr: {stderr}"
    );
    assert!(
        stdout.contains("written:"),
        "should report written docid, got: {stdout}"
    );
    assert!(root.join("wiki/brand-new.md").exists());
}

// ---------------------------------------------------------------------------
// Stale fix + search still works
// ---------------------------------------------------------------------------

#[test]
fn lint_fix_stale_preserves_search() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("Searchable Page", "Unique keyword xylophone42 in body.");
    let out = run_write(&root, "searchable", &content, &[]);
    assert!(out.status.success());

    // Verify searchable before modification.
    let search_before = memex_cmd(&root)
        .args(["search", "Searchable Page"])
        .output()
        .unwrap();
    let before_stdout = String::from_utf8_lossy(&search_before.stdout);
    assert_eq!(
        before_stdout.trim(),
        "searchable",
        "should find page before stale, got: {before_stdout}"
    );

    // Make the page stale by appending content.
    let path = root.join("wiki/searchable.md");
    let mut existing = std::fs::read_to_string(&path).unwrap();
    existing.push_str("\nAppended extra content.\n");
    std::fs::write(&path, &existing).unwrap();

    // Fix the stale index.
    let fix = memex_cmd(&root).args(["lint", "--fix"]).output().unwrap();
    let fix_stdout = String::from_utf8_lossy(&fix.stdout);
    assert!(
        fix_stdout.contains("fixed:"),
        "lint --fix should fix stale page, got: {fix_stdout}"
    );

    // Search should still find the page.
    let search_after = memex_cmd(&root)
        .args(["search", "Searchable Page"])
        .output()
        .unwrap();
    let after_stdout = String::from_utf8_lossy(&search_after.stdout);
    assert_eq!(
        after_stdout.trim(),
        "searchable",
        "should still find page after stale fix, got: {after_stdout}"
    );
}
