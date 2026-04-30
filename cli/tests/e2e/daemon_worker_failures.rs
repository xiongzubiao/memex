//! E2E tests for worker failure modes.
//!
//! Each test launches the daemon with mock-claude-code in a specific mode
//! and asserts the CLI surfaces the expected typed error.

use crate::common;
use crate::e2e_harness::E2EHarness;

/// Shared setup: mock-claude-code on PATH, daemon up, one page ingested.
fn harness_with_mock_mode(mode: &str) -> (tempfile::TempDir, std::path::PathBuf, E2EHarness) {
    let (mock_tmp, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");
    let h = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MOCK_CLAUDE_CODE_MODE", mode)
        .start();
    common::ingest_page(h.memex_root(),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
    (mock_tmp, extra_path, h)
}

#[test]
fn auth_fail_surfaces_backend_unavailable_with_code() {
    let (_mock, _path, h) = harness_with_mock_mode("auth_fail");
    let out = h.cli(&["query", "when did rollout start"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Auth failures flow through the unified backend-error path: the
    // CLI sees `backend_unavailable` with the Claude-native
    // `[authentication_failed]` code prefixed onto the message.
    assert!(
        stderr.contains("backend_unavailable") && stderr.contains("[authentication_failed]"),
        "expected backend_unavailable with [authentication_failed]; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn agent_error_surfaces_raw_text() {
    let (_mock, _path, h) = harness_with_mock_mode("agent_error");
    let out = h.cli(&["query", "any question"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("backend_unavailable") || stderr.contains("Overloaded"),
        "expected backend_unavailable or raw text; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn non_json_reply_becomes_agent_error() {
    let (_mock, _path, h) = harness_with_mock_mode("non_json");
    let out = h.cli(&["query", "any question"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The non-JSON reply surfaces as an agent error carrying the raw
    // text ("I thought about this..."), not as a successful synthesis.
    assert!(
        stderr.contains("backend_unavailable") || stderr.contains("I thought about"),
        "expected backend_unavailable or raw text; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn crash_retries_then_surfaces_subprocess_crashed() {
    let (_mock, _path, h) = harness_with_mock_mode("crash");
    // Crash mode exits immediately on every invocation. Worker retries
    // once (respawn → also crashes) → surfaces SubprocessCrashed.
    let out = h.cli(&["query", "any question"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("subprocess_crashed"),
        "expected subprocess_crashed; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn timeout_retries_then_surfaces_subprocess_timeout() {
    let (_mock, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");
    // 2s timeout so the test doesn't wait the default 60s × 2 attempts.
    let h = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MOCK_CLAUDE_CODE_MODE", "timeout")
        .env("MEMEX__DAEMON__WORKER__TIMEOUT_SEC", "2")
        .start();
    common::ingest_page(h.memex_root(),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    let out = h.cli(&["query", "any question"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("subprocess_timeout"),
        "expected subprocess_timeout; stdout={stdout} stderr={stderr}"
    );
}

/// Verify that `restart_after_jobs` doesn't break the worker: after N
/// jobs, the subprocess is soft-reset and subsequent queries succeed.
///
/// Single-page wiki → each query runs ExpandJob (turn 1) + SynthJob
/// (turn 2) against the same subprocess. `restart_after_jobs=2` fires
/// a restart between queries, so query 2 gets a fresh subprocess.
#[test]
fn restart_cadence_respawns_and_keeps_working() {
    let (_mock, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");
    let h = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MOCK_CLAUDE_CODE_MODE", "ok")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .env("MEMEX__DAEMON__WORKER__RESTART_AFTER_JOBS", "2")
        .start();
    common::ingest_page(h.memex_root(),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    // Three queries in sequence against the same long-lived daemon. The
    // worker accumulates ≥2 turns per query; after the first query the
    // restart-cadence branch fires. Queries 2 and 3 prove the respawn works.
    for i in 0..3 {
        let out = h.cli(&["query", "when did production rollout begin"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "query {i} failed: stdout={stdout} stderr={stderr}"
        );
        assert!(
            stdout.contains("Production rollout begins 2026-04-16"),
            "query {i} missing answer; stdout={stdout}"
        );
    }
}
