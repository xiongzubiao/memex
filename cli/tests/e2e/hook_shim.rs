//! Smoke tests for `memex hook ingest <agent>` — the Rust replacement for the
//! prior node shim scripts. Each invocation must:
//! - Parse stdin JSON
//! - Surface a clear error on bad/missing transcript_path
//! - Exit 0 without ingesting when MEMEX_INTERNAL=1
//!
//! The successful-ingest path is covered by the broader daemon ingest tests;
//! these only exercise the stdin-parsing entry point so they don't need a
//! live daemon.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn memex_bin() -> PathBuf {
    // env!("CARGO_BIN_EXE_<name>") is set by cargo for test binaries; it
    // resolves to the built `memex` binary in the same target dir.
    PathBuf::from(env!("CARGO_BIN_EXE_memex"))
}

fn run_hook(payload: &str, internal: bool) -> std::process::Output {
    let mut cmd = Command::new(memex_bin());
    cmd.args(["hook", "ingest", "claude-code"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if internal {
        cmd.env("MEMEX_INTERNAL", "1");
    }
    let mut child = cmd.spawn().expect("spawn memex hook");
    if !payload.is_empty() {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
    }
    drop(child.stdin.take());
    child.wait_with_output().expect("wait memex hook")
}

#[test]
fn hook_fails_on_missing_transcript_path() {
    let out = run_hook(r#"{"session_id":"x"}"#, false);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing transcript_path"),
        "stderr: {stderr}"
    );
}

#[test]
fn hook_fails_on_bad_json() {
    let out = run_hook("not json", false);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("parse hook payload"), "stderr: {stderr}");
}

#[test]
fn hook_skips_on_memex_internal() {
    // Empty stdin + MEMEX_INTERNAL=1 should exit 0 without reading payload.
    let out = run_hook("", true);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
