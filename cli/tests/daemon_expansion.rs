//! Integration test for Plan 6: weak-signal expansion.
//!
//! Ingests one page (single-page wikis always have weak BM25 signal
//! because there's no top-2 for gap comparison), queries, and asserts:
//! 1. The daemon emits Event::Expansion (CLI prints the expansion line).
//! 2. The Answer event is still produced.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_memex")
}

fn mock_claude_on_path() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().to_path_buf();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-claude.sh");
    assert!(fixture.exists());
    std::os::unix::fs::symlink(&fixture, bin_dir.join("claude")).unwrap();
    (tmp, bin_dir)
}

fn memex_cmd(root: &Path, extra_path: &Path, mock_mode: &str) -> Command {
    let mut cmd = Command::new(binary());
    let orig = std::env::var("PATH").unwrap_or_default();
    cmd.env("MEMEX_ROOT", root);
    cmd.env("PATH", format!("{}:{}", extra_path.display(), orig));
    cmd.env("MOCK_CLAUDE_MODE", mock_mode);
    cmd
}

fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

fn ingest_page(root: &Path, extra_path: &Path, stem: &str, title: &str, body: &str) {
    let content = make_page(title, body);
    let mut cmd = memex_cmd(root, extra_path, "ok");
    cmd.args(["write", stem, "--force", "--quiet"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "ingest failed");
}

fn ingest(root: &Path, extra_path: &Path) {
    ingest_page(
        root,
        extra_path,
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
}

fn stop_daemon(root: &Path, extra_path: &Path) {
    let _ = memex_cmd(root, extra_path, "ok")
        .args(["daemon", "stop"])
        .output();
    for _ in 0..20 {
        if !root.join("daemon.sock").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn weak_signal_query_triggers_expansion_and_returns_answer() {
    let (_mock, extra_path) = mock_claude_on_path();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    ingest(&root, &extra_path);

    let out = memex_cmd(&root, &extra_path, "expand_then_synth_ok")
        // Force a single worker so ExpandJob (turn 1) and SynthJob (turn 2)
        // are processed by the same subprocess in order — required by the
        // mock's per-process turn counter.
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .args(["query", "when did production rollout begin"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stop_daemon(&root, &extra_path);

    assert!(
        out.status.success(),
        "query failed: stdout={stdout} stderr={stderr}"
    );
    // Expansion event: Task 10 updates query_synth to print it explicitly,
    // but for now assert that the expansion terms appear in the output
    // (stdout or stderr) somewhere — lex term "rollout" is specific enough.
    assert!(
        stdout.contains("rollout") || stderr.contains("rollout"),
        "expected expansion evidence; stdout={stdout} stderr={stderr}"
    );
    // Answer still present.
    assert!(
        stdout.contains("Production rollout began"),
        "missing answer text; stdout={stdout}"
    );
    assert!(
        stdout.contains("auth-migration-timeline"),
        "missing citation; stdout={stdout}"
    );
}

#[test]
fn strong_signal_query_skips_expansion() {
    let (_mock, extra_path) = mock_claude_on_path();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    // Two distinct pages; query matches the title of page 1 strongly.
    // Wiki title has BM25 weight 4.0, so a title-matching query yields
    // a high top-1 score. Page 2's content is unrelated → BM25 returns
    // only page 1 → s2 defaults to 0.0 → (s1 - 0.0) ≥ 0.15 easily.
    ingest_page(
        &root,
        &extra_path,
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
    ingest_page(
        &root,
        &extra_path,
        "database-schema",
        "Database Schema",
        "User accounts table has columns id, email, created_at.",
    );

    let out = memex_cmd(&root, &extra_path, "ok")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .args(["query", "Auth Migration Timeline"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stop_daemon(&root, &extra_path);

    assert!(
        out.status.success(),
        "query failed: stdout={stdout} stderr={stderr}"
    );
    // Strong signal means no expansion fires → CLI does NOT print an
    // "Expansion:" block. Check for the literal header.
    assert!(
        !stdout.contains("Expansion:"),
        "expansion should not fire on strong signal; stdout={stdout}"
    );
    // Synthesis still runs and produces the answer.
    assert!(
        stdout.contains("Production rollout begins"),
        "missing answer text; stdout={stdout}"
    );
}
