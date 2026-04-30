//! Subprocess-daemon test fixture. Spawns a real `memex daemon` against
//! a per-test tempdir, exposes `cli()` for invoking the binary, and
//! runs `daemon stop` on `Drop` so panicked tests don't leak processes.
//!
//! The daemon already self-reaps when its socket file disappears (see
//! `cli/src/daemon/server.rs`), so the leak risk is bounded to a few
//! seconds — but Drop-on-stop closes the window faster, and provides
//! a single API for explicit lifecycle when a test wants it.
//!
//! Use this when a test wants:
//!   - Explicit start / stop bracketing (no implicit auto-spawn)
//!   - The `cli()` / `cli_ok()` shorthand for invoking the binary
//!   - Drop-time cleanup that runs even on panic
//!
//! For tests that just want to invoke the binary against a tempdir and
//! let auto-spawn handle the daemon, the lower-level pattern in
//! `daemon_lifecycle.rs` (`Command::new(common::binary())`) is fine.

#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

pub struct E2EHarness {
    _tmpdir: TempDir,
    memex_root: PathBuf,
    // Env vars to apply to every `cli()` invocation so worker
    // subprocesses spawned from those calls inherit them. Mirrors
    // what was passed to `start_with_*` so PATH (mock binaries) and
    // MOCK_*_MODE env vars stay consistent across daemon + cli calls.
    inherited_env: Vec<(OsString, OsString)>,
}

/// Builder for `E2EHarness`. Lets tests prepend a directory to PATH
/// (for mock subprocesses like fixtures/mock-claude-code.sh) and set
/// arbitrary env vars (e.g. MOCK_CLAUDE_CODE_MODE) before the daemon
/// spawns — both the daemon and any subsequent `cli()` calls inherit
/// the same environment.
pub struct E2EHarnessBuilder {
    extra_path: Option<PathBuf>,
    extra_env: Vec<(OsString, OsString)>,
    timeout: Duration,
}

impl E2EHarnessBuilder {
    pub fn new() -> Self {
        Self {
            extra_path: None,
            extra_env: Vec::new(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Prepend `path` to PATH for the daemon and every `cli()` call.
    /// Used by tests that mock external CLIs (`claude`, `codex`,
    /// `gemini`) — the daemon's worker subprocess looks up the
    /// agent binary via PATH, so the extra dir has to be visible
    /// when the daemon (and its workers) are spawned.
    pub fn extra_path(mut self, path: &Path) -> Self {
        self.extra_path = Some(path.to_path_buf());
        self
    }

    /// Set an env var inherited by the daemon and every `cli()` call.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.extra_env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn start(self) -> E2EHarness {
        let tmp = TempDir::new().expect("tempdir");
        let memex_root = tmp.path().join("memex");
        std::fs::create_dir_all(&memex_root).expect("mkdir memex_root");

        // Materialize the env we'll pass to both the daemon spawn AND
        // every cli() call. PATH gets the extra prefix prepended.
        let mut inherited_env: Vec<(OsString, OsString)> = self.extra_env.clone();
        if let Some(extra) = self.extra_path.as_ref() {
            let orig = std::env::var_os("PATH").unwrap_or_default();
            let mut combined = OsString::from(extra);
            combined.push(":");
            combined.push(&orig);
            inherited_env.push((OsString::from("PATH"), combined));
        }

        let bin = memex_bin();
        let mut cmd = Command::new(&bin);
        cmd.args(["daemon", "start"]).env("MEMEX_ROOT", &memex_root);
        for (k, v) in &inherited_env {
            cmd.env(k, v);
        }
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("failed to start daemon at {}: {e}", bin.display()));
        assert!(
            out.status.success(),
            "daemon start failed (exit {:?}): stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr),
        );

        // Poll the socket until the daemon is accepting connections.
        let socket = memex_root.join("daemon.sock");
        let deadline = Instant::now() + self.timeout;
        loop {
            if socket.exists()
                && std::os::unix::net::UnixStream::connect(&socket).is_ok()
            {
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "daemon did not become reachable at {:?} within {:?}",
                    socket, self.timeout,
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        E2EHarness {
            _tmpdir: tmp,
            memex_root,
            inherited_env,
        }
    }
}

impl Default for E2EHarnessBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl E2EHarness {
    /// Spawn a fresh daemon under a tempdir and wait for it to be
    /// reachable. Panics if the daemon doesn't accept a socket
    /// connection within 30 seconds.
    pub fn start() -> Self {
        E2EHarnessBuilder::new().start()
    }

    pub fn builder() -> E2EHarnessBuilder {
        E2EHarnessBuilder::new()
    }

    pub fn memex_root(&self) -> &Path {
        &self.memex_root
    }

    /// Run `memex <args>` with `MEMEX_ROOT` pointed at this harness's
    /// tempdir, inheriting any env vars the harness was built with
    /// (including PATH for mock subprocesses).
    pub fn cli(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(memex_bin());
        cmd.args(args).env("MEMEX_ROOT", &self.memex_root);
        for (k, v) in &self.inherited_env {
            cmd.env(k, v);
        }
        cmd.output()
            .unwrap_or_else(|e| panic!("failed to run memex {args:?}: {e}"))
    }

    /// `cli()` plus a one-off env override for this single call —
    /// useful when a single test query wants to flip a mock's mode
    /// (e.g. `MOCK_CLAUDE_CODE_MODE=fail`) without rebuilding the harness.
    pub fn cli_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(memex_bin());
        cmd.args(args).env("MEMEX_ROOT", &self.memex_root);
        for (k, v) in &self.inherited_env {
            cmd.env(k, v);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output()
            .unwrap_or_else(|e| panic!("failed to run memex {args:?}: {e}"))
    }

    /// Convenience: assert success and return stdout as a String.
    pub fn cli_ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "memex {:?} failed (exit {:?})\nstdout: {}\nstderr: {}",
            args,
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for E2EHarness {
    fn drop(&mut self) {
        // Best-effort: stop the daemon proactively. The daemon's
        // self-reap-on-socket-loss bounds the leak anyway, but Drop
        // closes the window faster and runs even on test panic.
        let _ = Command::new(memex_bin())
            .args(["daemon", "stop"])
            .env("MEMEX_ROOT", &self.memex_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Resolve the `memex` binary path from the test runner's exe path.
/// `target/{debug,release}/deps/<test>` → `target/{debug,release}/memex`.
fn memex_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let parent = exe.parent().unwrap();          // deps/
    let profile_dir = parent.parent().unwrap_or(parent); // debug or release
    profile_dir.join("memex")
}
