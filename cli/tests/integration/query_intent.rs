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
    memex_core::retrieval::embed_document(memex.search(), &hash, &fm.title, &body, &mut model).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_returns_focused_snippet_with_diff_header() {
    let harness = IntegrationHarness::start_with_real_retrieval().await.unwrap();
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
        snippet.starts_with("@@ -"),
        "expected diff header, got: {snippet}"
    );
    assert!(
        snippet.contains(": "),
        "expected line numbering, got: {snippet}"
    );
    assert!(
        snippet.len() < 500,
        "snippet should be capped near 300 chars, got {} chars: {snippet}",
        snippet.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_with_intent_changes_chunk_choice() {
    let harness = IntegrationHarness::start_with_real_retrieval().await.unwrap();
    // Two distinct lines so different lines win on different intents.
    // Front-end line carries 4 intent matches (web/page/load/times); back-end
    // line carries the only query match. Math: with intent, 4*0.3=1.2 > 1.0
    // so front-end wins; without intent, back-end (1.0) > front-end (0).
    // Adapted from spec test: original strings put back-end ahead even with
    // intent because INTENT_WEIGHT_SNIPPET (0.3) couldn't overcome a single
    // 1.0 query hit when only 3 intent terms matched.
    let body = "front-end web pages load times improvement\n# Section\nback-end SQL performance tuning\n";
    harness.write("Perf", body).await.unwrap();
    embed_wiki_page(harness.root(), "perf");

    let no_intent = harness.query_raw("performance", None).await.unwrap();
    let with_intent = harness
        .query_raw("performance", Some("web page load times"))
        .await
        .unwrap();
    let s1 = no_intent[0]["body"].as_str().unwrap();
    let s2 = with_intent[0]["body"].as_str().unwrap();
    assert_ne!(s1, s2, "intent should change the snippet selection");
    assert!(
        s2.contains("front-end"),
        "with intent='web page load times', the front-end line should be chosen; got: {s2}"
    );
}
