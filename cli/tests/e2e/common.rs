//! Shared utilities for the e2e test crate.
//! Tests reach for these via `crate::common::*` from main.rs.

#![allow(dead_code)]

pub use std::path::{Path, PathBuf};
pub use std::process::Command;
pub use std::time::Duration;
pub use tempfile::TempDir;

pub use memex_cli::test_utils::{ingest_page, make_page, seed_wiki_page};

/// Subprocess-output-shaped result for the `run_write` shim: `status`
/// has the same `success()` accessor, `stderr` is a Vec<u8> of the
/// error message. Lives here, not in test_utils, because it is a
/// compat layer for e2e tests that grew up around `std::process::Output`
/// before the seeder went in-process.
#[derive(Debug)]
pub struct WriteResult {
    pub status: WriteStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct WriteStatus(bool);

impl WriteStatus {
    pub fn success(&self) -> bool {
        self.0
    }
}

/// Thin shim for tests that pipe `--force` through a flag-list arg
/// (mirroring how the old `memex write --direct` subprocess wrapper
/// was used). Calls `seed_wiki_page` and packages the result into a
/// `WriteResult` so the test assertion patterns don't churn.
pub fn run_write(
    root: &Path,
    name: &str,
    content: &str,
    extra_args: &[&str],
) -> WriteResult {
    let force = extra_args.contains(&"--force");
    match seed_wiki_page(root, name, content, force) {
        Ok(()) => WriteResult {
            status: WriteStatus(true),
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
        Err(e) => WriteResult {
            status: WriteStatus(false),
            stdout: Vec::new(),
            stderr: e.to_string().into_bytes(),
        },
    }
}

pub fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_memex")
}

/// Symlink a mock agent fixture onto PATH. Returns (TempDir, bin_dir).
/// The TempDir must be held alive for the duration of the test.
pub fn mock_on_path(fixture_name: &str, binary_name: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().to_path_buf();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/e2e/fixtures/{fixture_name}"));
    assert!(fixture.exists(), "mock fixture missing: {fixture:?}");
    std::os::unix::fs::symlink(&fixture, bin_dir.join(binary_name)).unwrap();
    (tmp, bin_dir)
}

/// Build a `memex` Command with `MEMEX_ROOT` set and optional extra
/// `PATH` entries prepended (used by tests that mock external CLIs
/// like `claude` or `codex`).
pub fn memex_cmd(root: &Path, extra_path: Option<&Path>) -> Command {
    let mut cmd = Command::new(binary());
    cmd.env("MEMEX_ROOT", root);
    if let Some(ep) = extra_path {
        let orig = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}:{}", ep.display(), orig));
    }
    cmd
}

/// Drop a wiki file directly to disk (no DB row). Used by tests that
/// want to set up a stale-index or untracked-file scenario before
/// invoking `memex lint`.
pub fn write_wiki_page(root: &Path, filename: &str, title: &str, body: &str) {
    std::fs::create_dir_all(root.join("wiki")).unwrap();
    std::fs::write(root.join("wiki").join(filename), make_page(title, body)).unwrap();
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
