use memex_core::Memex;
use std::path::PathBuf;
use tempfile::TempDir;

#[allow(dead_code)]
pub const TEST_CONTENT: &str = "---\ntitle: Test\ntags: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z\nsources: []\n---\n\nTest body.\n";

pub fn setup_temp_memex() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    std::fs::create_dir_all(root.join("wiki")).unwrap();
    (dir, root)
}

/// Run post-stress invariants on a clean memex. Panics on violation.
#[allow(dead_code)]
pub fn assert_invariants(memex: &Memex) {
    let search = memex.search();
    // 1. Every documents.hash exists in content table.
    search
        .with_connection(|conn| {
            let orphans: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM documents d \
             LEFT JOIN content c ON d.hash = c.hash \
             WHERE c.hash IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(orphans, 0, "found {orphans} documents rows without content");
            // 2. No orphan chunks (content row for each chunks.hash)
            let orphan_chunks: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM chunks ch \
             LEFT JOIN content c ON ch.hash = c.hash \
             WHERE c.hash IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(orphan_chunks, 0, "found {orphan_chunks} orphan chunks");
            // 3. No duplicate docids
            let dup_docids: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (SELECT docid FROM documents GROUP BY docid HAVING COUNT(*) > 1)",
            [], |r| r.get(0)).unwrap();
            assert_eq!(dup_docids, 0, "found {dup_docids} duplicate docids");
            Ok(())
        })
        .unwrap();

    // 4. No tmp files in wiki/
    let leaks: Vec<_> = walkdir::WalkDir::new(memex.wiki_dir())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leaks.is_empty(), "tmp files leaked: {leaks:?}");
}
