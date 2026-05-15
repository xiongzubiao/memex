//! E2E test for concurrent queries against a single daemon.
//!
//! Launches multiple `memex query` processes in parallel and asserts all
//! succeed. The daemon's MPMC job queue is the unit under test — with a
//! two-worker pool, the N queries fan out across both workers.

use crate::common;
use crate::e2e_harness::E2EHarness;
use std::thread;

#[test]
fn concurrent_queries_all_succeed() {
    let (_mock, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");

    let h = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MOCK_CLAUDE_CODE_MODE", "ok")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "2")
        .start();

    common::ingest_page(
        h.memex_root(),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    // Warm up: prime retrieval actor + spawn first worker.
    let _ = h.cli(&["query", "--raw", "warmup"]);

    // Launch 3 queries in parallel. With worker.max_count=2 the pool
    // scales to 2 under pressure; the 3rd waits briefly then picks
    // up when a worker frees.
    let results = thread::scope(|s| {
        (0..3)
            .map(|i| {
                let h = &h;
                s.spawn(move || {
                    let out = h.cli(&["query", "when did production rollout begin"]);
                    (
                        i,
                        out.status.success(),
                        String::from_utf8_lossy(&out.stdout).to_string(),
                        String::from_utf8_lossy(&out.stderr).to_string(),
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });

    let mut all_ok = true;
    for (i, ok, stdout, stderr) in results {
        if !ok || !stdout.contains("Production rollout begins 2026-04-16") {
            all_ok = false;
            eprintln!("query {i}: ok={ok} stdout={stdout} stderr={stderr}");
        }
    }
    assert!(all_ok, "one or more concurrent queries failed");
}
