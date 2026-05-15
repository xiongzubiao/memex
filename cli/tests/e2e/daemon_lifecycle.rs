//! Integration tests for the daemon lifecycle.

#[allow(unused_imports)]
use crate::common;
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
    let mut d1 = spawn_daemon(root);
    let mut d2 = spawn_daemon(root);

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
    // Read the PID file to verify exactly one daemon won.
    assert!(
        root.join("daemon.pid").exists(),
        "no daemon acquired lock within 3s"
    );
    let pid_before: u32 = std::fs::read_to_string(root.join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // Hold for ~250ms and re-read; if a second daemon had taken over
    // (lock race went wrong), the pid file would point at a different
    // process by now. Same PID = exactly one winner held the lock for
    // this whole window.
    thread::sleep(Duration::from_millis(250));
    let pid_after: u32 = std::fs::read_to_string(root.join("daemon.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        pid_before, pid_after,
        "pid file changed: a second daemon took over after the first"
    );

    // Cleanup: stop and reap. The daemon removes its pid file on
    // successful shutdown (server.rs:604), so we don't assert on the
    // pid file existing post-stop.
    let _ = stop(root);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let still_alive = std::process::Command::new("ps")
            .args(["-p", &pid_before.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !still_alive {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = d1.wait();
    let _ = d2.wait();
}

/// Regression: when the daemon's socket file disappears (e.g. its
/// memex root was deleted out from under it — this is the dominant
/// failure mode for tests that drop their TempDir while the daemon is
/// still running), the daemon must self-reap within a few seconds.
/// Without this check, daemons survived until the 15-minute idle
/// timeout and accumulated under concurrent test runs, causing
/// pidfile / socket-path collisions and flaky e2e failures.
#[test]
fn daemon_self_reaps_when_socket_is_deleted() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    let mut daemon = spawn_daemon(root);
    assert!(
        wait_until(
            || status(root).starts_with("daemon: running"),
            Duration::from_secs(10),
        ),
        "daemon did not come up"
    );

    // Pull the socket out from under the daemon.
    std::fs::remove_file(root.join("daemon.sock")).expect("remove socket");

    // Daemon stats the socket every 2s; allow up to 8s for it to notice
    // and exit.
    let exited = wait_until(
        || matches!(daemon.try_wait(), Ok(Some(_))),
        Duration::from_secs(8),
    );
    if !exited {
        let _ = daemon.kill();
        panic!("daemon did not self-reap within 8s of socket deletion");
    }
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
