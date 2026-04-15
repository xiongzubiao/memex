use async_trait::async_trait;
use tempfile::TempDir;

struct MockQueryProvider;

#[async_trait]
impl memex_core::LlmProvider for MockQueryProvider {
    async fn chat(
        &self,
        _system: Option<&str>,
        prompt: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        if prompt.contains("Which wiki pages") || prompt.contains("which wiki pages") {
            Ok("wiki/caching.md\n".to_string())
        } else if prompt.contains("Based on these wiki pages")
            || prompt.contains("based on these wiki pages")
        {
            Ok("Caching improves performance. [Caching Strategies]".to_string())
        } else {
            Ok("[mock]".to_string())
        }
    }
}

fn seed_wiki_page(root: &std::path::Path) {
    std::fs::write(
        root.join("wiki/caching.md"),
        "---\ntitle: Caching Strategies\ntags:
  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\n\nCaching stores data closer to where it's needed.\n",
    ).unwrap();
}

#[tokio::test]
async fn query_returns_answer_with_citations() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open(root.clone(), Box::new(MockQueryProvider), "test").unwrap();
    seed_wiki_page(&root);
    memex.reindex().unwrap();
    let result = memex.query("What do I know about caching?").await.unwrap();
    assert!(result.answer.contains("Caching"));
    assert!(!result.citations.is_empty());
}

#[tokio::test]
async fn query_empty_memex() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open(root, Box::new(MockQueryProvider), "test").unwrap();
    let result = memex.query("anything?").await.unwrap();
    assert!(result.answer.contains("No relevant knowledge"));
}

#[tokio::test]
async fn context_for_returns_pages() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open(root.clone(), Box::new(MockQueryProvider), "test").unwrap();
    seed_wiki_page(&root);
    memex.reindex().unwrap();
    let pages = memex.context_for("build a cache", 50000).await.unwrap();
    assert!(!pages.is_empty());
}

#[tokio::test]
async fn context_for_empty_memex() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open(root, Box::new(MockQueryProvider), "test").unwrap();
    let pages = memex.context_for("anything", 50000).await.unwrap();
    assert!(pages.is_empty());
}
