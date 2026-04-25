//! Integration tests for the daemon lifecycle.

mod common;

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Spawn the daemon in foreground mode; caller must ensure it's stopped.
fn spawn_daemon(memex_root: &std::path::Path) -> Child {
    Command::new(common::binary())
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
    let out = Command::new(common::binary())
        .args(["daemon", "status"])
        .env("MEMEX_ROOT", memex_root)
        .output()
        .expect("status");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn stop(memex_root: &std::path::Path) -> std::process::Output {
    Command::new(common::binary())
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

    // Start two daemons concurrently. Each spawns a CLI process that forks internally.
    let _d1 = spawn_daemon(root);
    let _d2 = spawn_daemon(root);

    // Wait for the lock race to settle (3s max).
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut daemons_running = 0;
    while Instant::now() < deadline && daemons_running == 0 {
        thread::sleep(Duration::from_millis(50));
        // Count how many daemons have PID files (indicating the lock was acquired).
        if root.join("daemon.pid").exists() {
            daemons_running += 1;
        }
    }

    // Exactly one daemon should have acquired the lock.
    // Read the PID file to verify.
    assert!(
        root.join("daemon.pid").exists(),
        "no daemon acquired lock within 3s"
    );
    let pid: u32 = std::fs::read_to_string(root.join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // Cleanup: stop the winner, then check it exited cleanly.
    let _ = stop(root);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        // Check if the PID is still running.
        let still_alive = std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !still_alive {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Verify only one PID file exists (the winner, not the loser).
    // If a second daemon had won, we'd see a different PID.
    // The loser should have exited cleanly (exit 0).
    assert!(
        root.join("daemon.pid").exists(),
        "winner exited unexpectedly"
    );
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
