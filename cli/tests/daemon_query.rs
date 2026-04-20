//! Integration test for `memex query --raw` end-to-end.
//!
//! Ingests a wiki page, auto-spawns the daemon via `memex query`, asserts the
//! expected page body fragment appears in stdout, then stops the daemon.

mod common;

use std::time::{Duration, Instant};
use tempfile::TempDir;

fn wait_socket(root: &std::path::Path, dur: Duration) -> bool {
    let socket = root.join("daemon.sock");
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if socket.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn query_raw_returns_indexed_page() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
        None,
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    // memex query --raw auto-spawns the daemon (Task 9).
    // Use content-rich terms that BM25 will index (avoiding stopwords like
    // "when"/"did" that FTS5's Porter tokenizer does not index).
    let out = common::memex_cmd(&root, None)
        .args(["query", "--raw", "production rollout begins 2026"])
        .output()
        .expect("memex query");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Wait up to 5s for socket to exist (daemon may still be running).
    wait_socket(&root, Duration::from_secs(5));

    common::stop_daemon(&root, None);

    assert!(
        out.status.success(),
        "query exit={:?} stdout={stdout} stderr={stderr}",
        out.status.code(),
    );
    assert!(
        stdout.contains("auth-migration-timeline") || stdout.contains("Production rollout"),
        "expected result to include the page; got:\nstdout={stdout}\nstderr={stderr}"
    );
}
