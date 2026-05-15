mod common;

use memex_core::embed::MockEmbedder;
use memex_core::{FixOutcome, Memex};
use std::time::Duration;

/// Induce a stale-index condition by writing directly to the disk file
/// (bypassing memex), so the on-disk hash no longer matches the DB row.
fn induce_stale_index(root: &std::path::Path, stem: &str, new_content: &str) {
    let path = root.join("wiki").join(format!("{stem}.md"));
    std::fs::write(&path, new_content).unwrap();
}

#[test]
fn lint_fix_reverify_skips_already_fixed() {
    let (_dir, root) = common::setup_temp_memex();

    // Create a page via writer, reindex so DB knows about it.
    let w = Memex::open_writer(root.clone()).unwrap();
    let content_v1 = "---\ntitle: Foo
created_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nV1.\n";
    let page_path = root.join("wiki/foo.md");
    std::fs::write(&page_path, content_v1).unwrap();
    w.reindex().unwrap();
    drop(w);

    // Now induce stale-index (edit the file on disk).
    let content_v2 = "---\ntitle: Foo
created_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nV2.\n";
    induce_stale_index(&root, "foo", content_v2);

    // Reader opens — its connection is created here.
    let reader = Memex::open(root.clone()).unwrap();
    let report = reader.lint().unwrap();
    let stale: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.kind == memex_core::types::LintIssueKind::StaleIndex)
        .collect();
    assert_eq!(stale.len(), 1);

    // Force the reader's SQLite connection to pin a WAL snapshot by opening
    // a DEFERRED read transaction. Without this pin, the reader's connection
    // sees auto-committed reads per statement and the regression (apply_fix_locked
    // using self.search instead of a fresh connection) would NOT manifest — the
    // test would false-pass.
    reader
        .search()
        .with_connection(|conn| {
            conn.execute_batch("BEGIN DEFERRED").unwrap();
            let _: i64 = conn
                .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                .unwrap();
            Ok(())
        })
        .unwrap();

    // Concurrent writer fixes the issue. Use the `_with` variant +
    // MockEmbedder so the test exercises the apply-fix code path without
    // requiring ONNX runtime on the CI runner.
    {
        let r2 = root.clone();
        let issue = (*stale[0]).clone();
        std::thread::spawn(move || {
            let w = Memex::open_writer(r2).unwrap();
            w.apply_fix_with(&issue, &mut MockEmbedder).unwrap();
        })
        .join()
        .unwrap();
    }

    // Reader's apply_fix_locked_with MUST use a FRESH connection to see the
    // fixed state. If it reused reader.search (the regression), it would see
    // the pinned snapshot where disk=v2, DB=v1 (still stale), and attempt to
    // re-apply the fix.
    let outcome = reader
        .apply_fix_locked_with(stale[0], &mut MockEmbedder)
        .unwrap();
    assert!(
        matches!(outcome, FixOutcome::Stale),
        "reader must see committed state via fresh connection, got {outcome:?}"
    );

    // Release the reader's pinned transaction so TempDir can clean up.
    reader
        .search()
        .with_connection(|conn| {
            conn.execute_batch("ROLLBACK").ok(); // ok if already rolled back
            Ok(())
        })
        .unwrap();
}

#[test]
fn apply_fix_locked_releases_lock_on_return() {
    // Direct oracle: if apply_fix_locked's WriterLock RAII guard is leaked
    // (regression: holding the guard past the return), this test blocks
    // or times out on the follow-up open_writer_with_timeout.

    let (_dir, root) = common::setup_temp_memex();

    // Create a stale-index issue.
    let w = Memex::open_writer(root.clone()).unwrap();
    let content_v1 = "---\ntitle: Foo
created_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nV1.\n";
    std::fs::write(root.join("wiki/foo.md"), content_v1).unwrap();
    w.reindex().unwrap();
    drop(w);

    // Induce staleness.
    let content_v2 = "---\ntitle: Foo
created_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nV2.\n";
    std::fs::write(root.join("wiki/foo.md"), content_v2).unwrap();

    let reader = Memex::open(root.clone()).unwrap();
    let report = reader.lint().unwrap();
    let stale: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.kind == memex_core::types::LintIssueKind::StaleIndex)
        .collect();
    assert_eq!(stale.len(), 1);

    // Apply the fix via the reader path (acquires writer lock, runs, releases).
    // Use the `_with` variant + MockEmbedder so we don't need ONNX in CI.
    let outcome = reader
        .apply_fix_locked_with(stale[0], &mut MockEmbedder)
        .unwrap();
    assert!(matches!(outcome, memex_core::FixOutcome::Applied));

    // Immediately try to acquire the writer lock with a tight timeout.
    // Success = lock was properly released inside apply_fix_locked.
    // Timeout = lock was leaked past the return.
    let start = std::time::Instant::now();
    let second_writer = Memex::open_writer_with_timeout(root.clone(), Duration::from_millis(500));
    let elapsed = start.elapsed();
    assert!(
        second_writer.is_ok(),
        "expected immediate lock acquire after apply_fix_locked returned, got {second_writer:?} (elapsed {elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "lock was released but acquire took {elapsed:?} — slow?"
    );
}
