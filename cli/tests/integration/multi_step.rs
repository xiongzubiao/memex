//! Task 32: end-to-end write → query (focused snippet) → read --from-line.


/// Embed the wiki page on disk with the real ONNX model so vector
/// search can return a focused snippet. The harness write already
/// embeds via the shared embedder, but if the harness fell back to
/// `MockEmbedder` (no ONNX bundle) those chunks are FNV-hash garbage —
/// re-embedding here guarantees real vectors regardless. Callers that
/// need this guarantee must ensure `load_default_model()` succeeds.
use crate::integration_harness::IntegrationHarness;
fn embed_wiki_page(root: &std::path::Path, slug: &str) {
    let memex = memex_core::Memex::open(root.to_path_buf()).unwrap();
    let path = memex.wiki_dir().join(format!("{slug}.md"));
    let content = std::fs::read_to_string(&path).unwrap();
    let (fm, body) = memex_core::validate::parse_frontmatter(&content).unwrap();
    let hash = memex_core::storage::content_hash(body.as_bytes());
    let mut model = memex_core::retrieval::load_default_model().unwrap();
    memex_core::retrieval::embed_document(memex.search(), &hash, &fm.title, &body, &mut model).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_write_query_read() {
    let h = IntegrationHarness::start_with_real_retrieval().await.unwrap();
    let body = (1..=80).map(|i| format!("line {i}\n")).collect::<String>()
        + "this is a focused snippet about authentication tokens and bearer auth\n"
        + &(1..=20).map(|i| format!("trail {i}\n")).collect::<String>();
    h.write("Auth Tokens", &body).await.unwrap();
    embed_wiki_page(h.root(), "auth-tokens");

    let entries = h
        .query_raw("authentication tokens", Some("bearer auth"))
        .await
        .unwrap();
    assert!(!entries.is_empty(), "query returned no entries");
    let snippet = entries[0]["body"].as_str().expect("entry body missing");
    assert!(snippet.starts_with("@@ -"), "expected diff header, got: {snippet}");

    let header = snippet.lines().next().unwrap();
    let (n, m) = parse_diff_header(header);

    let read_out = h
        .cli(&[
            "read", "auth-tokens",
            "--from-line", &n.to_string(),
            "--max-lines", &m.to_string(),
        ])
        .await;
    assert!(
        read_out.contains("focused snippet about authentication tokens"),
        "read output should include the keyworded line; got: {read_out}"
    );
}

fn parse_diff_header(h: &str) -> (usize, usize) {
    // "@@ -47,4 @@ ..."  → (47, 4)
    let nm = h.strip_prefix("@@ -").unwrap().split_whitespace().next().unwrap();
    let mut it = nm.splitn(2, ',');
    let n: usize = it.next().unwrap().parse().unwrap();
    let m: usize = it.next().unwrap().parse().unwrap();
    (n, m)
}

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
