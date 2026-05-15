//! E2E test for `memex query "<q>"` (no --raw): synth path.
//!
//! Uses a mock `claude` binary (cli/tests/e2e/fixtures/mock-claude-code.sh)
//! placed first on PATH. The mock emits a canned stream-json reply
//! regardless of input, so the test is deterministic.

use crate::common;
use crate::e2e_harness::E2EHarness;

#[test]
fn synth_returns_answer_with_citation() {
    let (_mock_tmp, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");

    let h = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MOCK_CLAUDE_CODE_MODE", "ok")
        .start();

    common::ingest_page(
        h.memex_root(),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    let out = h.cli(&["query", "when did production rollout begin"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

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
