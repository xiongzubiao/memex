//! Integration tests for the daemon lifecycle.

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_memex")
}

/// Spawn the daemon in foreground mode; caller must ensure it's stopped.
fn spawn_daemon(memex_root: &std::path::Path) -> Child {
    Command::new(binary())
        .args(["daemon", "start"])
        .env("MEMEX_ROOT", memex_root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning daemon")
}

/// Block until `predicate(...)` returns true or deadline elapses.
fn wait_until<F: FnMut() -> bool>(mut f: F, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    f()
}

fn status(memex_root: &std::path::Path) -> String {
    let out = Command::new(binary())
        .args(["daemon", "status"])
        .env("MEMEX_ROOT", memex_root)
        .output()
        .expect("status");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn stop(memex_root: &std::path::Path) -> std::process::Output {
    Command::new(binary())
        .args(["daemon", "stop"])
        .env("MEMEX_ROOT", memex_root)
        .output()
        .expect("stop")
}

#[test]
fn daemon_starts_answers_status_stops_cleanly() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let mut daemon = spawn_daemon(root);

    // Wait for daemon to bind socket.
    let socket = root.join("daemon.sock");
    assert!(
        wait_until(|| socket.exists(), Duration::from_secs(10)),
        "daemon did not create socket"
    );

    // status should report running.
    let s = status(root);
    assert!(s.starts_with("daemon: running"), "unexpected status: {s}");

    // stop should request shutdown (ignore exit code; wait for actual daemon exit).
    let _ = stop(root);

    // Daemon process should exit cleanly after stop.
    let exited = wait_until(
        || daemon.try_wait().map(|o| o.is_some()).unwrap_or(false),
        Duration::from_secs(30),
    );
    assert!(exited, "daemon did not exit after stop");
}

#[test]
fn concurrent_start_exactly_one_wins() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Start two daemons concurrently.
    let mut d1 = spawn_daemon(root);
    let mut d2 = spawn_daemon(root);

    // Exactly one should remain running; the other should exit 0 within 3s.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut d1_exited = false;
    let mut d2_exited = false;
    while Instant::now() < deadline && !(d1_exited ^ d2_exited) {
        thread::sleep(Duration::from_millis(50));
        d1_exited = d1.try_wait().map(|o| o.is_some()).unwrap_or(false);
        d2_exited = d2.try_wait().map(|o| o.is_some()).unwrap_or(false);
    }
    assert!(
        d1_exited ^ d2_exited,
        "expected exactly one daemon to exit quickly (d1={d1_exited}, d2={d2_exited})"
    );

    // The loser should have exit code 0 (clean "already running" exit).
    if d1_exited {
        let out = d1.wait().unwrap();
        assert_eq!(out.code(), Some(0), "loser d1 should exit 0");
    } else {
        let out = d2.wait().unwrap();
        assert_eq!(out.code(), Some(0), "loser d2 should exit 0");
    }

    // Cleanup: stop the winner.
    let _ = stop(root);
    let _ = d1.wait();
    let _ = d2.wait();
}

#[test]
fn stale_socket_is_cleaned_up() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    // Create a fake stale socket file.
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join("daemon.sock"), b"").unwrap();
    assert!(root.join("daemon.sock").exists());

    // Start the daemon. It should unlink the stale socket and create a real one.
    let mut daemon = spawn_daemon(root);

    assert!(
        wait_until(
            || {
                // Wait until the daemon can answer status.
                let s = status(root);
                s.starts_with("daemon: running")
            },
            Duration::from_secs(10),
        ),
        "daemon did not come up after stale socket"
    );

    let _ = stop(root);
    let _ = daemon.wait();
}
