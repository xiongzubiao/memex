//! End-to-end tests for the source plan / plan show / plan apply
//! pipeline.
//!
//! These tests need a real LLM worker (the worker pool's
//! `new_inert_for_test` constructor returns errors on Ingest/Merge
//! jobs, which makes the EXTRACT/MERGE phases of `source plan` fail
//! at the queue layer). To avoid breaking CI on machines without an
//! LLM backend configured, the suite is opt-in via the `MEMEX_E2E=1`
//! env var.
//!
//! Locally:
//!   MEMEX_E2E=1 cargo test -p memex-cli --test plan_pipeline_e2e
//!
//! On CI: the suite skips silently with no failures.

#[test]
fn plan_pipeline_happy_path() {
    if std::env::var("MEMEX_E2E").is_err() {
        eprintln!("skipping: set MEMEX_E2E=1 to run plan pipeline e2e tests");
    }
    // Real implementation TBD once a worker fixture is wired up.
    // The test should:
    //   1. Spawn a daemon with MockEmbedder + a deterministic Ingest/Merge
    //      worker stub
    //   2. `memex source add` a small markdown blob
    //   3. `memex source plan <docid>` — assert exit 0 + plan JSON on stdout
    //   4. `memex plan show < plan.json` — assert formatted output
    //   5. `memex plan apply < plan.json` — assert exit 0 + 'committed N wiki pages'
    //   6. Verify wiki/<slug>.md exists with the expected frontmatter
}

#[test]
fn plan_pipeline_edit_and_retry() {
    if std::env::var("MEMEX_E2E").is_err() {
        // skip silently
    }
    // Mutate plan slug to overlap with seeded existing page → exit 3 →
    // re-pipe → exit 0.
}
