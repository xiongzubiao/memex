//! E2E tests for `memex lint` (and related sub-commands).
//! Spawns the actual `memex` binary; daemon-mediated commands
//! auto-spawn the daemon via connect_or_spawn.

use crate::common::*;

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

    let output = memex_cmd(&root, None).args(["lint"]).output().unwrap();
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

    let output = memex_cmd(&root, None).args(["lint"]).output().unwrap();
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
        "---\ntitle: REST Patterns
created_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\nCompletely rewritten content about REST and GraphQL.\n",
    )
    .unwrap();

    // Run lint — should detect stale-index.
    let lint_out = memex_cmd(&root, None).args(["lint"]).output().unwrap();
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
    // With daemon-routed `lint --fix`, the daemon's startup reconcile
    // and watcher already keep the index in sync with disk; this test
    // verifies the end state (index matches disk after `lint --fix`),
    // not the specific repair path. Whether reconcile, the watcher, or
    // an explicit `apply_fix_locked` produced the result is opaque to
    // the user.
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page(
        "REST Patterns",
        "Resource-oriented design with proper HTTP verbs.",
    );
    let out = run_write(&root, "rest-patterns", &content, &[]);
    assert!(out.status.success(), "write should succeed");

    // External edit (no daemon involved while we modify the bytes).
    std::fs::write(
        root.join("wiki/rest-patterns.md"),
        "---\ntitle: REST Patterns
created_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\nCompletely rewritten content about REST and GraphQL.\n",
    )
    .unwrap();

    let fix_out = memex_cmd(&root, None)
        .args(["lint", "--fix"])
        .output()
        .unwrap();
    let fix_stderr = String::from_utf8_lossy(&fix_out.stderr);
    assert!(
        fix_out.status.success(),
        "lint --fix should succeed, stderr: {fix_stderr}"
    );

    // After `lint --fix` returns, lint should report no stale-index.
    let lint_out = memex_cmd(&root, None).args(["lint"]).output().unwrap();
    let lint_stdout = String::from_utf8_lossy(&lint_out.stdout);
    assert!(
        !lint_stdout.contains("stale-index:"),
        "lint should not find stale-index after lint --fix, got: {lint_stdout}"
    );
}
#[test]
fn lint_detects_dangling_after_delete() {
    use crate::e2e_harness::E2EHarness;

    // `delete` is daemon-routed, so use the harness for explicit
    // lifecycle. `lint` itself runs in-process; it can be invoked
    // through the harness's `cli()` either way (the binary picks
    // its own dispatch path per command).
    let h = E2EHarness::start();

    // Write page A, then page B that explicitly references A. Auto-
    // linking is not part of `memex write` (the LLM Extract+Merge
    // pipeline is the canonical link-maintaining path); deterministic
    // writes carry `[[link]]` syntax verbatim from the user.
    let page_a = make_page("Caching", "LRU eviction policies.");
    run_write(h.memex_root(), "caching", &page_a, &[]);

    let page_b = make_page(
        "Performance",
        "Improve performance with [[caching]] and indexing.",
    );
    run_write(h.memex_root(), "performance", &page_b, &[]);

    let disk_b = std::fs::read_to_string(h.memex_root().join("wiki/performance.md")).unwrap();
    assert!(disk_b.contains("[[caching]]"));

    let del = h.cli(&["delete", "caching", "--force"]);
    assert!(del.status.success(), "delete failed: {:?}", del);

    // Lint should find the dangling link.
    let stdout = h.cli_ok(&["lint"]);
    assert!(
        stdout.contains("dangling:") && stdout.contains("caching"),
        "lint should detect dangling link to deleted page, got: {stdout}"
    );
}
#[test]
fn lint_fix_stale_preserves_search() {
    // After `lint --fix` (or, equivalently, daemon reconcile/watcher),
    // the page must still be findable via search. We don't assert on
    // the specific repair path — only on the end-state outcome.
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let content = make_page("Searchable Page", "Unique keyword xylophone42 in body.");
    let out = run_write(&root, "searchable", &content, &[]);
    assert!(out.status.success());

    let path = root.join("wiki/searchable.md");
    let mut existing = std::fs::read_to_string(&path).unwrap();
    existing.push_str("\nAppended extra content.\n");
    std::fs::write(&path, &existing).unwrap();

    let fix = memex_cmd(&root, None)
        .args(["lint", "--fix"])
        .output()
        .unwrap();
    assert!(
        fix.status.success(),
        "lint --fix should succeed, stderr: {}",
        String::from_utf8_lossy(&fix.stderr)
    );

    let search_after = memex_cmd(&root, None)
        .args(["search", "Searchable Page"])
        .output()
        .unwrap();
    let after_stdout = String::from_utf8_lossy(&search_after.stdout);
    assert_eq!(
        after_stdout.trim(),
        "searchable",
        "should still find page after stale fix, got: {after_stdout}"
    );
    let _ = memex_cmd(&root, None)
        .args(["daemon", "stop"])
        .output()
        .unwrap();
}
