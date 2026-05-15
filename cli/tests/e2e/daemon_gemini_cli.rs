//! E2E tests for the gemini-cli worker's synth path.

use crate::common;
use crate::e2e_harness::E2EHarness;

fn gemini_harness(mock_mode: Option<&str>) -> (tempfile::TempDir, std::path::PathBuf, E2EHarness) {
    let (mock_tmp, extra_path) = common::mock_on_path("mock-gemini-cli.sh", "gemini");
    let mut b = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MEMEX__DAEMON__WORKER__BACKEND", "gemini-cli")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1");
    if let Some(mode) = mock_mode {
        b = b.env("MOCK_GEMINI_MODE", mode);
    }
    let h = b.start();
    common::ingest_page(
        h.memex_root(),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
    (mock_tmp, extra_path, h)
}

#[test]
fn gemini_synth_returns_answer_with_citation() {
    let (_mock, _path, h) = gemini_harness(None);
    let out = h.cli(&["query", "when did production rollout begin"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

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
fn gemini_auth_fail_surfaces_backend_unavailable_with_rpc_code() {
    let (_mock, _path, h) = gemini_harness(Some("auth_fail"));
    let out = h.cli(&["query", "any question"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // All backend errors flow through `backend_unavailable`; the CLI
    // preserves the JSON-RPC error code as `[rpc=<n>]` so the user
    // sees the structured identifier alongside the message.
    assert!(
        stderr.contains("backend_unavailable") && stderr.contains("[rpc="),
        "expected backend_unavailable with [rpc=] code; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn gemini_crash_retries_then_surfaces_subprocess_crashed() {
    let (_mock, _path, h) = gemini_harness(Some("crash"));
    let out = h.cli(&["query", "any question"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("subprocess_crashed"),
        "expected subprocess_crashed; stdout={stdout} stderr={stderr}"
    );
}
