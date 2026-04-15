//! End-to-end CLI tests for the `memex` binary.
//! Exercises the real binary with MEMEX_ROOT pointed at a temp directory.
//! Uses the DryRunProvider (no API keys needed).

use std::path::Path;
use std::process::Command;

fn memex_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_memex"))
}

fn run_memex(root: &Path, args: &[&str]) -> (String, String, bool) {
    let output = memex_bin()
        .args(args)
        .env("MEMEX_ROOT", root.to_str().unwrap())
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .output()
        .expect("failed to run memex binary");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (stdout, stderr, output.status.success())
}

#[test]
fn e2e_init_creates_structure() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let (stdout, stderr, ok) = run_memex(&root, &["init"]);
    assert!(ok, "init failed: stdout={stdout}\nstderr={stderr}");
    assert!(root.join("schema.md").exists(), "schema.md missing");
    assert!(root.join("index.md").exists(), "index.md missing");
    assert!(root.join("log.md").exists(), "log.md missing");
    assert!(root.join("wiki").is_dir(), "wiki/ missing");
    assert!(root.join("config.toml").exists(), "config.toml missing");
}

#[test]
fn e2e_init_is_idempotent() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let (_, _, ok1) = run_memex(&root, &["init"]);
    assert!(ok1, "first init failed");

    let (_, _, ok2) = run_memex(&root, &["init"]);
    assert!(ok2, "second init failed");
}

#[test]
fn e2e_ingest_creates_wiki_pages() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Init first
    let (_, _, ok) = run_memex(&root, &["init"]);
    assert!(ok, "init failed");

    // Create source file
    let source = dir.path().join("notes.md");
    std::fs::write(&source, "# My Notes\nSome content about caching.\n").unwrap();

    // Ingest (no tty, so confirmation is skipped automatically)
    let (stdout, stderr, ok) = run_memex(&root, &["ingest", source.to_str().unwrap()]);
    assert!(ok, "ingest failed: stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout.contains("pages created"),
        "should report pages created: stdout={stdout}"
    );

    // Wiki pages should exist
    let wiki_entries: Vec<_> = std::fs::read_dir(root.join("wiki"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();
    assert!(
        !wiki_entries.is_empty(),
        "wiki/ should have pages after ingest"
    );

    // Index should be updated
    let index = std::fs::read_to_string(root.join("index.md")).unwrap();
    assert!(
        !index.contains("No wiki pages yet"),
        "index should list pages: {index}"
    );
}

#[test]
fn e2e_query_after_ingest() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Init + ingest
    run_memex(&root, &["init"]);
    let source = dir.path().join("notes.md");
    std::fs::write(&source, "# Notes\nContent.\n").unwrap();
    run_memex(&root, &["ingest", source.to_str().unwrap()]);

    // Query
    let (stdout, stderr, ok) = run_memex(&root, &["query", "what do I know?"]);
    assert!(ok, "query failed: stdout={stdout}\nstderr={stderr}");
    // DryRunProvider returns a canned answer
    assert!(
        !stdout.is_empty(),
        "query should produce output: stdout={stdout}"
    );
}

#[test]
fn e2e_lint_after_ingest() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // Init + ingest
    run_memex(&root, &["init"]);
    let source = dir.path().join("notes.md");
    std::fs::write(&source, "# Notes\nContent.\n").unwrap();
    run_memex(&root, &["ingest", source.to_str().unwrap()]);

    // Lint
    let (stdout, stderr, ok) = run_memex(&root, &["lint"]);
    assert!(ok, "lint failed: stdout={stdout}\nstderr={stderr}");
}

#[test]
fn e2e_wiki_show_and_stats() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    run_memex(&root, &["init"]);

    // Wiki show
    let (stdout, _, ok) = run_memex(&root, &["wiki", "show"]);
    assert!(ok);
    assert!(stdout.contains("Index"), "wiki show should display index");

    // Wiki stats
    let (stdout, _, ok) = run_memex(&root, &["wiki", "stats"]);
    assert!(ok);
    assert!(stdout.contains("Pages:"), "stats should show page count");
}

#[test]
fn e2e_wiki_reindex() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    run_memex(&root, &["init"]);

    // Manually create a wiki page
    std::fs::write(
        root.join("wiki/manual-page.md"),
        "---\ntitle: Manual Page\ntags:\n  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\n\nManual content.\n",
    ).unwrap();

    // Reindex
    let (stdout, _, ok) = run_memex(&root, &["wiki", "reindex"]);
    assert!(ok, "reindex failed");
    assert!(
        stdout.contains("rebuilt") || stdout.contains("Rebuilt") || stdout.contains("reindex"),
        "should confirm reindex: {stdout}"
    );

    // Wiki show should now include the manual page
    let (stdout, _, _) = run_memex(&root, &["wiki", "show"]);
    assert!(
        stdout.contains("Manual Page"),
        "index should include manual page after reindex: {stdout}"
    );
}

#[test]
fn e2e_full_workflow() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("memex");

    // 1. Init
    let (_, _, ok) = run_memex(&root, &["init"]);
    assert!(ok, "init failed");

    // 2. Ingest
    let source = dir.path().join("design-doc.md");
    std::fs::write(&source, "# API Design\nREST with proper HTTP methods.\n").unwrap();
    let (stdout, _, ok) = run_memex(&root, &["ingest", source.to_str().unwrap()]);
    assert!(ok, "ingest failed");
    assert!(stdout.contains("pages created"));

    // 3. Query
    let (stdout, _, ok) = run_memex(&root, &["query", "what do I know about APIs?"]);
    assert!(ok, "query failed");
    assert!(!stdout.is_empty());

    // 4. Lint
    let (_, _, ok) = run_memex(&root, &["lint"]);
    assert!(ok, "lint failed");

    // 5. Wiki stats
    let (stdout, _, ok) = run_memex(&root, &["wiki", "stats"]);
    assert!(ok, "stats failed");
    assert!(stdout.contains("Pages:"));

    // 6. Wiki log
    let (stdout, _, ok) = run_memex(&root, &["wiki", "log"]);
    assert!(ok, "log failed");
    // Log should contain entries from ingest, query, lint
    assert!(
        stdout.contains("ingest") || stdout.contains("query") || stdout.contains("lint"),
        "log should have operation entries: {stdout}"
    );

    // 7. Config show
    let (stdout, _, ok) = run_memex(&root, &["config", "show"]);
    assert!(ok, "config show failed");
    assert!(
        stdout.contains("dry-run"),
        "config should show provider: {stdout}"
    );

    // 8. Doctor
    let (stdout, _, ok) = run_memex(&root, &["doctor"]);
    assert!(ok, "doctor failed");
    // With no API keys, should show "No providers detected"
    assert!(
        stdout.contains("No providers") || stdout.contains("provider"),
        "doctor output: {stdout}"
    );
}
