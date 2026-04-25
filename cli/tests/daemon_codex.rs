//! Integration test for the codex worker's synth path.
//!
//! Symlinks `mock-codex.sh` as `codex` on PATH and sets
//! `MEMEX__DAEMON__WORKER__BACKEND=codex` so the daemon spawns the mock.

mod common;

use tempfile::TempDir;

#[test]
fn codex_synth_returns_answer_with_citation() {
    let (_mock, extra_path) = common::mock_on_path("mock-codex.sh", "codex");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
        Some(&extra_path),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    // Mock defaults to ok mode. Single-page wiki → weak signal → expand job
    // fires first (mock returns the synth JSON, which parse_expand rejects,
    // so the handler falls back to un-expanded retrieval). Then the synth
    // job runs against the mock, returning a valid synth reply.
    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MEMEX__DAEMON__WORKER__BACKEND", "codex")
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
fn codex_auth_fail_surfaces_backend_unavailable_with_code() {
    let (_mock, extra_path) = common::mock_on_path("mock-codex.sh", "codex");
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
        .env("MEMEX__DAEMON__WORKER__BACKEND", "codex")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .env("MOCK_CODEX_MODE", "auth_fail")
        .args(["query", "any question"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    common::stop_daemon(&root, Some(&extra_path));

    // Auth failures are no longer special-cased: they surface as a
    // generic `backend_unavailable` with the backend's code prefixed.
    // The mock emits kind="auth" in the turn error, so the CLI sees
    // `[auth] <message>` via the structured code propagation path.
    assert!(
        stderr.contains("backend_unavailable") && stderr.contains("[auth]"),
        "expected backend_unavailable with [auth] code; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn codex_crash_retries_then_surfaces_subprocess_crashed() {
    let (_mock, extra_path) = common::mock_on_path("mock-codex.sh", "codex");
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    common::ingest_page(
        &root,
        Some(&extra_path),
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    // Mock exits immediately on every invocation. Worker retries once
    // (respawn → also crashes) → surfaces SubprocessCrashed.
    let out = common::memex_cmd(&root, Some(&extra_path))
        .env("MEMEX__DAEMON__WORKER__BACKEND", "codex")
        .env("MEMEX__DAEMON__WORKER__MAX_COUNT", "1")
        .env("MOCK_CODEX_MODE", "crash")
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
