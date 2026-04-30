//! E2E test for the autoscaling worker pool.
//!
//! Starts a daemon with `max_count=4` and fires 3 concurrent `memex query`
//! processes. The pool begins with 1 min worker; under pressure it scales
//! up (submit() sees a backlog and spawns extras). All three queries must
//! succeed with the expected answer.

use crate::common;
use crate::e2e_harness::E2EHarness;
use std::thread;

#[test]
fn autoscale_serves_concurrent_queries() {
    let (_mock, extra_path) = common::mock_on_path("mock-claude-code.sh", "claude");

    let h = E2EHarness::builder()
        .extra_path(&extra_path)
        .env("MOCK_CLAUDE_CODE_MODE", "ok")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "4")
        .start();

    common::ingest_page(h.memex_root(),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    // Warm up the daemon's worker (also primes the retrieval actor).
    let _ = h.cli(&["query", "--raw", "warmup"]);

    let results = thread::scope(|s| {
        let mut handles = Vec::new();
        for i in 0..3 {
            let h = &h;
            handles.push(s.spawn(move || {
                let out = h.cli(&["query", "when did production rollout begin"]);
                (
                    i,
                    out.status.success(),
                    String::from_utf8_lossy(&out.stdout).to_string(),
                    String::from_utf8_lossy(&out.stderr).to_string(),
                )
            }));
        }
        handles
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
    assert!(all_ok, "one or more autoscale queries failed");
}
