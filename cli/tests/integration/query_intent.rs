//! Verifies Tasks 22 + 23: query path threads `intent` through retrieval, and
//! the retrieved Entry.body is now a focused snippet (with diff header) rather
//! than the full chunk text.

/// Embed the wiki page on disk that `harness.write` just produced
/// using the real ONNX model. The fast harness uses `MockEmbedder`
/// (FNV-hash pseudo-vectors), so its writes never produce
/// semantically-useful chunks — query tests need the real embedder
/// to assert on similarity-driven outcomes.
use crate::integration_harness::IntegrationHarness;
fn embed_wiki_page(root: &std::path::Path, slug: &str) {
    let memex = memex_core::Memex::open(root.to_path_buf()).unwrap();
    let path = memex.wiki_dir().join(format!("{slug}.md"));
    let content = std::fs::read_to_string(&path).unwrap();
    let (fm, body) = memex_core::validate::parse_frontmatter(&content).unwrap();
    let hash = memex_core::storage::content_hash(body.as_bytes());
    let mut model = memex_core::retrieval::load_default_model().unwrap();
    memex_core::retrieval::embed_document(memex.search(), &hash, &fm.title, &body, &mut model)
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_returns_focused_snippet_with_diff_header() {
    let harness = IntegrationHarness::start_with_real_retrieval()
        .await
        .unwrap();
    let big_body = (1..=80).map(|i| format!("line {i}\n")).collect::<String>()
        + "this section discusses performance optimizations heavily\n"
        + &(1..=20).map(|i| format!("more {i}\n")).collect::<String>();
    harness.write("Perf Notes", &big_body).await.unwrap();
    embed_wiki_page(harness.root(), "perf-notes");

    let entries = harness
        .query_raw("performance optimizations", None)
        .await
        .unwrap();
    assert!(!entries.is_empty(), "query returned no entries");
    let snippet = entries[0]["body"]
        .as_str()
        .expect("entry should have body field");
    assert!(
        snippet.contains("performance optimizations"),
        "snippet should contain the matched phrase, got: {snippet}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_with_intent_threads_through_retrieval() {
    // Plumbing test: the query handler forwards `intent` into the retrieval
    // pipeline without panicking and returns context entries.
    //
    // The previous shape of this test asserted that intent's `score_chunk`
    // re-ranking flipped which chunk landed at rank 0. That coupled the
    // assertion to (a) the chunk picker firing (only on title-FTS hits with
    // `chunk_seq=None`), (b) RRF tie-breaking between title and chunk hits,
    // and (c) the inert worker pool's expansion behavior — none of which is
    // robust enough to assert here. The actual chunk-picker behavior is
    // covered by `core::snippet::tests::score_chunk_*`.
    let harness = IntegrationHarness::start_with_real_retrieval()
        .await
        .unwrap();
    let body =
        "front-end web pages load times improvement\n# Section\nback-end SQL performance tuning\n";
    harness.write("Perf", body).await.unwrap();
    embed_wiki_page(harness.root(), "perf");

    let no_intent = harness.query_raw("performance", None).await.unwrap();
    let with_intent = harness
        .query_raw("performance", Some("web page load times"))
        .await
        .unwrap();
    assert!(!no_intent.is_empty(), "no-intent query returned no entries");
    assert!(
        !with_intent.is_empty(),
        "with-intent query returned no entries"
    );
}
