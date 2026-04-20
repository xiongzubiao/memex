//! Integration test for the autoscaling worker pool.
//!
//! Starts a daemon with `max_count=4` and fires 3 concurrent `memex query`
//! processes. The pool begins with 1 min worker; under pressure it scales
//! up (submit() sees a backlog and spawns extras). All three queries must
//! succeed with the expected answer.

mod common;

use std::thread;
use tempfile::TempDir;

#[test]
fn autoscale_serves_concurrent_queries() {
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

    // Pre-spawn the daemon so the concurrent queries don't race on the
    // single-daemon flock. With max_count=4 the pool starts at 1 worker
    // and scales up as the queue backlogs under the 3 concurrent queries.
    let _ = common::memex_cmd(&root, Some(&extra_path))
        .env("MOCK_CLAUDE_CODE_MODE", "ok")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "4")
        .args(["query", "--raw", "warmup"])
        .output()
        .unwrap();

    let mut handles = Vec::new();
    for i in 0..3 {
        let root = root.clone();
        let extra_path = extra_path.clone();
        let h = thread::spawn(move || {
            let out = common::memex_cmd(&root, Some(&extra_path))
                .env("MOCK_CLAUDE_CODE_MODE", "ok")
                .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "4")
                .args(["query", "when did production rollout begin"])
                .output()
                .unwrap();
            (
                i,
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).to_string(),
                String::from_utf8_lossy(&out.stderr).to_string(),
            )
        });
        handles.push(h);
    }

    let mut all_ok = true;
    for h in handles {
        let (i, ok, stdout, stderr) = h.join().unwrap();
        if !ok || !stdout.contains("Production rollout begins 2026-04-16") {
            all_ok = false;
            eprintln!("query {i}: ok={ok} stdout={stdout} stderr={stderr}");
        }
    }

    common::stop_daemon(&root, Some(&extra_path));
    assert!(all_ok, "one or more autoscale queries failed");
}
