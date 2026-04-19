//! Integration test for `memex query "<q>"` (no --raw): synth path.
//!
//! Uses a mock `claude` binary (cli/tests/fixtures/mock-claude.sh) placed
//! first on PATH. The mock emits a canned stream-json reply regardless of
//! input, so the test is deterministic.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_memex")
}

/// Set up a temp dir containing a symlink `claude -> mock-claude.sh`.
/// Returns (tempdir, path-prepend) so the caller can set `PATH=<dir>:$PATH`.
fn mock_claude_on_path() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().to_path_buf();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-claude.sh");
    assert!(fixture.exists(), "mock fixture missing: {fixture:?}");
    let link = bin_dir.join("claude");
    std::os::unix::fs::symlink(&fixture, &link).unwrap();
    (tmp, bin_dir)
}

fn memex_cmd(root: &Path, extra_path: &Path) -> Command {
    let mut cmd = Command::new(binary());
    let orig = std::env::var("PATH").unwrap_or_default();
    cmd.env("MEMEX_ROOT", root);
    cmd.env("PATH", format!("{}:{}", extra_path.display(), orig));
    cmd
}

fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

fn ingest(root: &Path, extra_path: &Path, name: &str, title: &str, body: &str) {
    let content = make_page(title, body);
    let mut cmd = memex_cmd(root, extra_path);
    cmd.args(["write", name, "--force", "--quiet"]);
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
    assert!(
        out.status.success(),
        "ingest failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
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
fn synth_returns_answer_with_citation() {
    let (_mock_tmp, extra_path) = mock_claude_on_path();

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    ingest(
        &root,
        &extra_path,
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

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
        out.status.code(),
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
