//! E2E tests for `memex misc` (and related sub-commands).
//! Spawns the actual `memex` binary; daemon-mediated commands
//! auto-spawn the daemon via connect_or_spawn.

use crate::common::*;

#[test]
fn ingest_and_backfill_help_show_collection_flag() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");

    let ingest_help = memex_cmd(&root, None)
        .args(["ingest", "--help"])
        .output()
        .unwrap();
    assert!(
        ingest_help.status.success(),
        "ingest --help should succeed, stderr: {}",
        String::from_utf8_lossy(&ingest_help.stderr)
    );
    let ingest_stdout = String::from_utf8_lossy(&ingest_help.stdout);
    assert!(
        ingest_stdout.contains("--collection"),
        "ingest help should show --collection flag, got: {ingest_stdout}"
    );

    let backfill_help = memex_cmd(&root, None)
        .args(["backfill", "--help"])
        .output()
        .unwrap();
    assert!(
        backfill_help.status.success(),
        "backfill --help should succeed, stderr: {}",
        String::from_utf8_lossy(&backfill_help.stderr)
    );
    let backfill_stdout = String::from_utf8_lossy(&backfill_help.stdout);
    assert!(
        backfill_stdout.contains("--collection"),
        "backfill help should show --collection flag, got: {backfill_stdout}"
    );
}
