//! Shared test helpers for daemon integration tests.
#![allow(dead_code)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;

pub fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_memex")
}

/// Symlink a mock agent fixture onto PATH. Returns (TempDir, bin_dir).
/// The TempDir must be held alive for the duration of the test.
pub fn mock_on_path(fixture_name: &str, binary_name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().to_path_buf();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{fixture_name}"));
    assert!(fixture.exists(), "mock fixture missing: {fixture:?}");
    std::os::unix::fs::symlink(&fixture, bin_dir.join(binary_name)).unwrap();
    (tmp, bin_dir)
}

/// Build a `memex` Command with MEMEX_ROOT set and optional extra PATH prepended.
pub fn memex_cmd(root: &Path, extra_path: Option<&Path>) -> Command {
    let mut cmd = Command::new(binary());
    cmd.env("MEMEX_ROOT", root);
    if let Some(ep) = extra_path {
        let orig = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{}", ep.display(), orig));
    }
    cmd
}

pub fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

/// Write a test wiki page via `memex write --direct`.
pub fn ingest_page(root: &Path, extra_path: Option<&Path>, slug: &str, title: &str, body: &str) {
    let content = make_page(title, body);
    let mut cmd = memex_cmd(root, extra_path);
    cmd.args(["write", "--direct", slug, "--force", "--quiet"]);
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
        "ingest failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Stop the daemon and wait for the socket to disappear.
pub fn stop_daemon(root: &Path, extra_path: Option<&Path>) {
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
