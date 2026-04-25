//! Integration test for the gemini cli worker's synth path.
//!
//! Symlinks `mock-gemini-cli.sh` as `gemini` on PATH and sets
//! `MEMEX__DAEMON__WORKER__BACKEND=gemini-cli` so the daemon spawns the mock.

mod common;

use tempfile::TempDir;

#[test]
fn gemini_synth_returns_answer_with_citation() {
    let (_mock, extra_path) = common::mock_on_path("mock-gemini-cli.sh", "gemini");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
        Some(&extra_path),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MEMEX__DAEMON__WORKER__BACKEND", "gemini-cli")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .args(["query", "when did production rollout begin"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

    assert!(
        out.status.success(),
        "query failed: exit={:?} stdout={stdout} stderr={stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains("Production rollout begins 2026-04-16"),
        "missing answer text: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("auth-migration-timeline"),
        "missing citation: stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn gemini_auth_fail_surfaces_backend_unavailable_with_rpc_code() {
    let (_mock, extra_path) = common::mock_on_path("mock-gemini-cli.sh", "gemini");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    common::ingest_page(
        &root,
        Some(&extra_path),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MEMEX__DAEMON__WORKER__BACKEND", "gemini-cli")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .env("MOCK_GEMINI_MODE", "auth_fail")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

    // All backend errors flow through `backend_unavailable` uniformly; the
    // CLI preserves the JSON-RPC error code as `[rpc=<n>]` so the user
    // sees the structured identifier alongside the message.
    assert!(
        stderr.contains("backend_unavailable") && stderr.contains("[rpc="),
        "expected backend_unavailable with [rpc=] code; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn gemini_crash_retries_then_surfaces_subprocess_crashed() {
    let (_mock, extra_path) = common::mock_on_path("mock-gemini-cli.sh", "gemini");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    common::ingest_page(
        &root,
        Some(&extra_path),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MEMEX__DAEMON__WORKER__BACKEND", "gemini-cli")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .env("MOCK_GEMINI_MODE", "crash")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

    assert!(
        stderr.contains("subprocess_crashed"),
        "expected subprocess_crashed; stdout={stdout} stderr={stderr}"
    );
}
