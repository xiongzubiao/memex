//! Integration tests for worker failure modes.
//!
//! Each test launches the daemon with mock-claude-code in a specific mode and
//! asserts the CLI surfaces the expected typed error.

mod common;

use std::path::PathBuf;
use tempfile::TempDir;

/// Shared setup: mock-claude-code on PATH, temp MEMEX_ROOT, ingest one page.
fn setup() -> (TempDir, PathBuf, TempDir, PathBuf) {
    let (mock_tmp, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");
    let root_tmp = TempDir::new().unwrap();
    let root = root_tmp.path().join("memex");
    common::ingest_page(
        &root,
        Some(&extra_path),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
    (mock_tmp, extra_path, root_tmp, root)
}

#[test]
fn auth_fail_surfaces_auth_failed_error() {
    let (_mock, extra_path, _root_tmp, root) = setup();
    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MOCK_CLAUDE_CODE_MODE", "auth_fail")
        .args(["query", "when did rollout start"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

    assert!(
        stderr.contains("auth_failed") || stderr.contains("auth failed"),
        "expected auth_failed in stderr; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn agent_error_surfaces_raw_text() {
    let (_mock, extra_path, _root_tmp, root) = setup();
    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MOCK_CLAUDE_CODE_MODE", "agent_error")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

    assert!(
        stderr.contains("agent_unavailable") || stderr.contains("Overloaded"),
        "expected agent_unavailable or raw text in stderr; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn non_json_reply_becomes_agent_error() {
    let (_mock, extra_path, _root_tmp, root) = setup();
    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MOCK_CLAUDE_CODE_MODE", "non_json")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

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
    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MOCK_CLAUDE_CODE_MODE", "crash")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

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
    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MOCK_CLAUDE_CODE_MODE", "timeout")
        .env("MEMEX__DAEMON__WORKER__TIMEOUT_SEC", "2")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

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
    let (_mock, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    common::ingest_page(
        &root,
        Some(&extra_path),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    let run_query = |q: &str| {
        let out = common::memex_cmd(&root, Some(&extra_path))
            .env("MOCK_CLAUDE_CODE_MODE", "ok")
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

    common::stop_daemon(&root, Some(&extra_path));
}
