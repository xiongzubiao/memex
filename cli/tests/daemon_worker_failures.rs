//! Integration tests for Plan 4 worker failure modes.
//!
//! Each test launches the daemon with mock-claude in a specific mode and
//! asserts the CLI surfaces the expected typed error.

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
    assert!(fixture.exists(), "mock fixture missing: {fixture:?}");
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

fn ingest(root: &Path, extra_path: &Path) {
    let content = make_page(
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
    let mut cmd = memex_cmd(root, extra_path, "ok");
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

/// Shared setup: mock-claude on PATH, temp MEMEX_ROOT, ingest one page.
fn setup() -> (TempDir, PathBuf, TempDir, PathBuf) {
    let (mock_tmp, extra_path) = mock_claude_on_path();
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().join("memex");
    ingest(&root, &extra_path);
    (mock_tmp, extra_path, root_tmp, root)
}

#[test]
fn auth_fail_surfaces_auth_failed_error() {
    let (_mock, extra_path, _root_tmp, root) = setup();
    let out = memex_cmd(&root, &extra_path, "auth_fail")
        .args(["query", "when did rollout start"])
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
fn agent_error_surfaces_raw_text() {
    let (_mock, extra_path, _root_tmp, root) = setup();
    let out = memex_cmd(&root, &extra_path, "agent_error")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stop_daemon(&root, &extra_path);

    assert!(
        stderr.contains("agent_unavailable") || stderr.contains("Overloaded"),
        "expected agent_unavailable or raw text in stderr; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn non_json_reply_becomes_agent_error() {
    let (_mock, extra_path, _root_tmp, root) = setup();
    let out = memex_cmd(&root, &extra_path, "non_json")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stop_daemon(&root, &extra_path);

    // The non-JSON reply should surface as an agent error carrying the raw
    // text ("I thought about this..."), not as a successful synthesis.
    assert!(
        stderr.contains("agent_unavailable") || stderr.contains("I thought about"),
        "expected agent_unavailable or raw text; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn crash_retries_then_surfaces_subprocess_crashed() {
    let (_mock, extra_path, _root_tmp, root) = setup();
    // The crash mode exits immediately on every invocation. Worker retries
    // once (respawn → also crashes) → surfaces SubprocessCrashed.
    let out = memex_cmd(&root, &extra_path, "crash")
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

#[test]
fn timeout_retries_then_surfaces_subprocess_timeout() {
    let (_mock, extra_path, _root_tmp, root) = setup();
    // Override worker timeout to 2s via env so the test doesn't wait the
    // default 60s × 2 attempts.
    let mut cmd = memex_cmd(&root, &extra_path, "timeout");
    cmd.env("MEMEX__DAEMON__WORKER__TIMEOUT_SEC", "2");
    let out = cmd.args(["query", "any question"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    stop_daemon(&root, &extra_path);

    assert!(
        stderr.contains("subprocess_timeout"),
        "expected subprocess_timeout; stdout={stdout} stderr={stderr}"
    );
}

/// Verify that `restart_after_jobs` doesn't break the worker: after N jobs,
/// the subprocess is soft-reset (Claude: full respawn; Codex: fresh thread;
/// Gemini: fresh session). Subsequent queries must still succeed.
///
/// With a single-page wiki, each query runs ExpandJob (turn 1) + SynthJob
/// (turn 2) against the same subprocess. Setting restart_after_jobs=2 fires
/// a restart between queries, so query 2 gets a fresh subprocess.
#[test]
fn restart_cadence_respawns_and_keeps_working() {
    let (_mock, extra_path) = mock_claude_on_path();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    ingest(&root, &extra_path);

    let run_query = |q: &str| {
        let out = memex_cmd(&root, &extra_path, "ok")
            .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
            .env("MEMEX__DAEMON__WORKER__RESTART_AFTER_JOBS", "2")
            .args(["query", q])
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };

    // Three queries in sequence against the same long-lived daemon. The
    // worker accumulates ≥2 turns per query; after the first query the
    // restart-cadence branch fires. Query 2 and 3 prove the respawn works.
    for i in 0..3 {
        let (ok, stdout, stderr) = run_query("when did production rollout begin");
        assert!(ok, "query {i} failed: stdout={stdout} stderr={stderr}");
        assert!(
            stdout.contains("Production rollout begins 2026-04-16"),
            "query {i} missing answer; stdout={stdout}"
        );
    }

    stop_daemon(&root, &extra_path);
}
