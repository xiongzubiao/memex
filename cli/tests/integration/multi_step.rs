use crate::integration_harness::IntegrationHarness;

/// Asserts the daemon's write path populates `chunks` + `chunks_vec`
/// synchronously. Embedding now runs on the request path, so the rows
/// are present immediately after `write` returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_via_daemon_populates_chunks_vec() {
    let h = IntegrationHarness::start_with_real_retrieval().await.unwrap();
    let body = "# Auth Tokens\n\nbearer tokens authenticate api requests using the Authorization header";
    h.write("Auth", body).await.unwrap();

    // The daemon strips any incoming frontmatter and re-serializes; the
    // body that gets hashed is what `parse_frontmatter` returns from the
    // on-disk file.
    let on_disk = std::fs::read_to_string(h.root().join("wiki/auth.md")).unwrap();
    let (_fm, persisted_body) = memex_core::validate::parse_frontmatter(&on_disk).unwrap();
    let body_hash = memex_core::storage::content_hash(persisted_body.as_bytes());

    // After write returns, embeddings are already populated (synchronous).
    let conn = rusqlite::Connection::open(h.root().join("index.db")).unwrap();
    let chunk_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks WHERE hash=?1",
            rusqlite::params![&body_hash],
            |r| r.get(0),
        )
        .unwrap();
    assert!(chunk_count >= 1, "expected ≥1 chunk for new doc, got {chunk_count}");
    let vec_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM chunks_vec WHERE hash_seq LIKE ?1 || '_%'",
            rusqlite::params![&body_hash],
            |r| r.get(0),
        )
        .unwrap();
    assert!(vec_count >= 1, "expected ≥1 chunks_vec row, got {vec_count}");
}
