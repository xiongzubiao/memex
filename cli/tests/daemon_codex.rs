//! Integration test for the codex worker's synth path.
//!
//! Symlinks `mock-codex.sh` as `codex` on PATH and sets
//! `MEMEX__DAEMON__WORKER__AGENT=codex` so the daemon spawns the mock.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_memex")
}

fn mock_codex_on_path() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().to_path_buf();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-codex.sh");
    assert!(fixture.exists(), "mock fixture missing: {fixture:?}");
    std::os::unix::fs::symlink(&fixture, bin_dir.join("codex")).unwrap();
    (tmp, bin_dir)
}

fn memex_cmd(root: &Path, extra_path: &Path) -> Command {
    let mut cmd = Command::new(binary());
    let orig = std::env::var("PATH").unwrap_or_default();
    cmd.env("MEMEX_ROOT", root);
    cmd.env("PATH", format!("{}:{}", extra_path.display(), orig));
    // Select codex as the worker agent
    cmd.env("MEMEX__DAEMON__WORKER__AGENT", "codex");
    // Single worker so expand + synth hit the same subprocess in sequence
    cmd.env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1");
    cmd
}

fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

fn ingest(root: &Path, extra_path: &Path) {
    let content = make_page(
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
    let mut cmd = memex_cmd(root, extra_path);
    cmd.args(["write", "auth-migration-timeline", "--force", "--quiet"]);
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

fn stop_daemon(root: &Path, extra_path: &Path) {
    let _ = memex_cmd(root, extra_path)
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
fn codex_synth_returns_answer_with_citation() {
    let (_mock, extra_path) = mock_codex_on_path();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    ingest(&root, &extra_path);

    // Mock defaults to ok mode. Single-page wiki → weak signal → expand job
    // fires first (mock returns the synth JSON, which parse_expand rejects,
    // so the handler falls back to un-expanded retrieval). Then the synth
    // job runs against the mock, returning a valid synth reply.
    let out = memex_cmd(&root, &extra_path)
        .args(["query", "when did production rollout begin"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stop_daemon(&root, &extra_path);

    assert!(
        out.status.success(),
        "query failed: exit={:?} stdout={stdout} stderr={stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains("Production rollout begins 2026-04-16"),
        "missing answer text: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("auth-migration-timeline"),
        "missing citation: stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn codex_auth_fail_surfaces_auth_failed() {
    let (_mock, extra_path) = mock_codex_on_path();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    ingest(&root, &extra_path);

    let out = memex_cmd(&root, &extra_path)
        .env("MOCK_CODEX_MODE", "auth_fail")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stop_daemon(&root, &extra_path);

    assert!(
        stderr.contains("auth_failed") || stderr.contains("auth failed"),
        "expected auth_failed in stderr; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn codex_crash_retries_then_surfaces_subprocess_crashed() {
    let (_mock, extra_path) = mock_codex_on_path();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    ingest(&root, &extra_path);

    // Mock exits immediately on every invocation. Worker retries once
    // (respawn → also crashes) → surfaces SubprocessCrashed.
    let out = memex_cmd(&root, &extra_path)
        .env("MOCK_CODEX_MODE", "crash")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stop_daemon(&root, &extra_path);

    assert!(
        stderr.contains("subprocess_crashed"),
        "expected subprocess_crashed; stdout={stdout} stderr={stderr}"
    );
}
