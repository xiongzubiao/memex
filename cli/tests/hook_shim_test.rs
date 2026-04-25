//! Smoke tests for plugin/hooks/*.js shims. Each shim must:
//! - Parse stdin JSON
//! - Extract transcript_path
//! - Invoke memex with the right --agent and transcript path positional
//!
//! We run the shim with a fake `memex` on PATH that just echoes its argv,
//! and assert the argv looks right.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn workspace_hook(shim: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR points to cli/. Plugin lives at workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("plugin/hooks")
        .join(shim)
}

fn fake_memex_dir() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let memex_path = dir.path().join("memex");
    let mut f = std::fs::File::create(&memex_path).unwrap();
    writeln!(
        f,
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > {}/argv.txt",
        dir.path().display()
    )
    .unwrap();
    drop(f);
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&memex_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&memex_path, perms).unwrap();
    dir
}

fn run_shim(shim: &str, payload: &str, fake_dir: &Path) -> std::process::Output {
    let path_var = format!(
        "{}:{}",
        fake_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new("node")
        .arg(workspace_hook(shim))
        .env("PATH", path_var)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shim");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    child.wait_with_output().expect("wait shim")
}

#[test]
fn claude_code_shim_invokes_memex_with_path() {
    let dir = fake_memex_dir();
    let payload = r#"{"transcript_path":"/abs/transcript.jsonl","session_id":"x"}"#;
    let out = run_shim("claude-code-session-end.js", payload, dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    assert_eq!(
        lines,
        vec!["ingest", "--agent", "claude-code", "/abs/transcript.jsonl"]
    );
}

#[test]
fn codex_shim_invokes_memex_with_path() {
    let dir = fake_memex_dir();
    let payload = r#"{"transcript_path":"/abs/codex.jsonl"}"#;
    let out = run_shim("codex-stop.js", payload, dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    assert_eq!(
        lines,
        vec!["ingest", "--agent", "codex", "/abs/codex.jsonl"]
    );
}

#[test]
fn gemini_shim_invokes_memex_with_path() {
    let dir = fake_memex_dir();
    let payload = r#"{"transcript_path":"/abs/g.json"}"#;
    let out = run_shim("gemini-cli-session-end.js", payload, dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let argv = std::fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    assert_eq!(
        lines,
        vec!["ingest", "--agent", "gemini-cli", "/abs/g.json"]
    );
}

#[test]
fn shim_fails_on_missing_transcript_path() {
    let dir = fake_memex_dir();
    let out = run_shim("claude-code-session-end.js", r#"{"x":1}"#, dir.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("missing transcript_path"),
        "stderr: {stderr}"
    );
}

#[test]
fn shim_skips_on_memex_internal() {
    let dir = fake_memex_dir();
    let mut child = Command::new("node")
        .arg(workspace_hook("claude-code-session-end.js"))
        .env("MEMEX_INTERNAL", "1")
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Don't write anything — shim should exit immediately on MEMEX_INTERNAL.
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Fake memex should NOT have been invoked.
    assert!(
        !dir.path().join("argv.txt").exists(),
        "fake memex should not have run"
    );
}
