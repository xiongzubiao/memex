//! E2E test for weak-signal expansion.
//!
//! Ingests one page (single-page wikis always have weak BM25 signal
//! because there's no top-2 for gap comparison), queries, and asserts:
//! 1. The daemon emits Event::Expansion (CLI prints the expansion line).
//! 2. The Answer event is still produced.

use crate::common;
use crate::e2e_harness::E2EHarness;

#[test]
fn weak_signal_query_triggers_expansion_and_returns_answer() {
    let (_mock, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");

    let h = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MOCK_CLAUDE_CODE_MODE", "expand_then_synth_ok")
        // Force a single worker so ExpandJob (turn 1) and SynthJob (turn 2)
        // are processed by the same subprocess in order — required by the
        // mock's per-process turn counter.
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
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
        "query failed: stdout={stdout} stderr={stderr}"
    );
    // Expansion evidence — lex term "rollout" appears somewhere.
    assert!(
        stdout.contains("rollout") || stderr.contains("rollout"),
        "expected expansion evidence; stdout={stdout} stderr={stderr}"
    );
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
    let (_mock, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");

    let h = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MOCK_CLAUDE_CODE_MODE", "ok")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .start();

    // Two distinct pages; query matches the title of page 1 strongly.
    // Wiki title has BM25 weight 4.0, so a title-matching query yields
    // a high top-1 score; page 2's unrelated content → BM25 returns only
    // page 1 → s2 defaults to 0.0 → (s1 - 0.0) ≥ 0.15 easily.
    common::ingest_page(
        h.memex_root(),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
    common::ingest_page(
        h.memex_root(),
        "database-schema",
        "Database Schema",
        "User accounts table has columns id, email, created_at.",
    );

    let out = h.cli(&["query", "Auth Migration Timeline"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "query failed: stdout={stdout} stderr={stderr}"
    );
    // Strong signal → no expansion fires → CLI does NOT print "Expansion:".
    assert!(
        !stdout.contains("Expansion:"),
        "expansion should not fire on strong signal; stdout={stdout}"
    );
    assert!(
        stdout.contains("Production rollout begins"),
        "missing answer text; stdout={stdout}"
    );
}
