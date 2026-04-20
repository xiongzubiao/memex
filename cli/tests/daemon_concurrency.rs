//! Integration test for concurrent queries against a single daemon.
//!
//! Launches multiple `memex query` processes in parallel and asserts all
//! succeed. The daemon's MPMC job queue is the unit under test — with a
//! two-worker pool, the N queries fan out across both workers.

mod common;

use std::thread;
use tempfile::TempDir;

#[test]
fn concurrent_queries_all_succeed() {
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

    // Pre-spawn the daemon so the three concurrent queries don't race
    // on the single-daemon flock (one would win, others would retry).
    // The subsequent `memex query` calls will reuse the running daemon.
    let _ = common::memex_cmd(&root, Some(&extra_path))
        .env("MOCK_CLAUDE_CODE_MODE", "ok")
        .args(["query", "--raw", "warmup"])
        .output()
        .unwrap();

    // Launch 3 queries in parallel threads. With worker.max_count=2 the
    // pool scales up to 2 under pressure and the queue fans them out. The
    // 3rd waits briefly then picks up when a worker frees.
    let mut handles = Vec::new();
    for i in 0..3 {
        let root = root.clone();
        let extra_path = extra_path.clone();
        let h = thread::spawn(move || {
            let out = common::memex_cmd(&root, Some(&extra_path))
                .env("MOCK_CLAUDE_CODE_MODE", "ok")
                .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "2")
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
    assert!(all_ok, "one or more concurrent queries failed");
}
