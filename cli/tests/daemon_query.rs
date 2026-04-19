//! Integration test for `memex query --raw` end-to-end.
//!
//! Ingests a wiki page, auto-spawns the daemon via `memex query`, asserts the
//! expected page body fragment appears in stdout, then stops the daemon.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_memex")
}

fn memex_cmd(root: &Path) -> Command {
    let mut cmd = Command::new(binary());
    cmd.env("MEMEX_ROOT", root);
    cmd
}

/// Frontmatter + body format accepted by `memex write`.
fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

fn ingest_wiki_page(root: &Path, name: &str, title: &str, body: &str) {
    let content = make_page(title, body);
    let mut cmd = memex_cmd(root);
    cmd.args(["write", name, "--force", "--quiet"]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn memex write");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait memex write");
    assert!(
        out.status.success(),
        "memex write failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn wait_socket(root: &Path, dur: Duration) -> bool {
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

fn stop_daemon(root: &Path) {
    let _ = memex_cmd(root).args(["daemon", "stop"]).output();
    // Give it a moment for cleanup.
    for _ in 0..20 {
        if !root.join("daemon.sock").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn query_raw_returns_indexed_page() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    ingest_wiki_page(
        &root,
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    // memex query --raw auto-spawns the daemon (Task 9).
    // Use content-rich terms that BM25 will index (avoiding stopwords like
    // "when"/"did" that FTS5's Porter tokenizer does not index).
    let out = memex_cmd(&root)
        .args(["query", "--raw", "production rollout begins 2026"])
        .output()
        .expect("memex query");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Wait up to 5s for socket to exist (daemon may still be running).
    wait_socket(&root, Duration::from_secs(5));

    stop_daemon(&root);

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
