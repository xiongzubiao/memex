//! Integration test for the autoscaling worker pool.
//!
//! Starts a daemon with `max_count=4` and fires 3 concurrent `memex query`
//! processes. The pool begins with 1 min worker; under pressure it scales
//! up (submit() sees a backlog and spawns extras). All three queries must
//! succeed with the expected answer.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_memex")
}

fn mock_claude_on_path() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let bin_dir = tmp.path().to_path_buf();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-claude.sh");
    assert!(fixture.exists(), "mock fixture missing: {fixture:?}");
    std::os::unix::fs::symlink(&fixture, bin_dir.join("claude")).unwrap();
    (tmp, bin_dir)
}

fn memex_cmd(root: &Path, extra_path: &Path) -> Command {
    let mut cmd = Command::new(binary());
    let orig = std::env::var("PATH").unwrap_or_default();
    cmd.env("MEMEX_ROOT", root);
    cmd.env("PATH", format!("{}:{}", extra_path.display(), orig));
    cmd.env("MOCK_CLAUDE_MODE", "ok");
    cmd
}

fn make_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {title}\ntags:\n  - entity\ncreated_at: 2026-04-10T00:00:00Z\nupdated_at: 2026-04-10T00:00:00Z\nsources: []\n---\n\n{body}\n"
    )
}

fn ingest(root: &Path, extra_path: &Path) {
    let content = make_page(
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );
    let mut cmd = memex_cmd(root, extra_path);
    cmd.args(["write", "auth-migration-timeline", "--force", "--quiet"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "ingest failed");
}

fn stop_daemon(root: &Path, extra_path: &Path) {
    let _ = memex_cmd(root, extra_path)
        .args(["daemon", "stop"])
        .output();
    for _ in 0..20 {
        if !root.join("daemon.sock").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn autoscale_serves_concurrent_queries() {
    let (_mock, extra_path) = mock_claude_on_path();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    ingest(&root, &extra_path);

    // Pre-spawn the daemon so the concurrent queries don't race on the
    // single-daemon flock. With max_count=4 the pool starts at 1 worker
    // and scales up as the queue backlogs under the 3 concurrent queries.
    let _ = memex_cmd(&root, &extra_path)
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "4")
        .args(["query", "--raw", "warmup"])
        .output()
        .unwrap();

    let mut handles = Vec::new();
    for i in 0..3 {
        let root = root.clone();
        let extra_path = extra_path.clone();
        let h = thread::spawn(move || {
            let out = memex_cmd(&root, &extra_path)
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

    stop_daemon(&root, &extra_path);
    assert!(all_ok, "one or more autoscale queries failed");
}
