use memex_core::Memex;
use memex_core::embed::EmbeddingModel;
use std::path::PathBuf;
use tempfile::TempDir;

/// Try to load the default ONNX embedding model. Returns `None` if the
/// model file or runtime dylib isn't available — callers should `return`
/// early so the test skips cleanly without asserting anything. Works on
/// fresh clones and CI without `~/.memex/models/` populated.
#[allow(dead_code)]
pub fn try_load_embedding_model() -> Option<EmbeddingModel> {
    if memex_core::embed::init_runtime().is_err() {
        eprintln!("SKIP: ONNX runtime unavailable");
        return None;
    }
    match memex_core::retrieval::load_default_model() {
        Ok(m) => Some(m),
        Err(e) => {
            eprintln!("SKIP: embedding model unavailable: {e}");
            None
        }
    }
}

#[allow(dead_code)]
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
    search
        .with_connection(|conn| {
            // 1. No orphan chunks — every chunks.hash has a documents row
            //    with the same hash (filesystem-canonical: bodies live on
            //    disk; chunks just point at offsets).
            let orphan_chunks: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM chunks ch \
                     LEFT JOIN documents d ON ch.hash = d.hash \
                     WHERE d.hash IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(orphan_chunks, 0, "found {orphan_chunks} orphan chunks");
            // 2. No duplicate (doc_type, path) — UNIQUE constraint should
            //    enforce this; double-check post-stress.
            let dup_paths: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ( \
                       SELECT doc_type, path FROM documents \
                       GROUP BY doc_type, path HAVING COUNT(*) > 1 \
                     )",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(dup_paths, 0, "found {dup_paths} duplicate (doc_type, path) rows");
            Ok(())
        })
        .unwrap();

    // 3. No tmp files in wiki/
    let leaks: Vec<_> = walkdir::WalkDir::new(memex.wiki_dir())
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leaks.is_empty(), "tmp files leaked: {leaks:?}");
}
