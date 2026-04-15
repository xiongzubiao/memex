use async_trait::async_trait;
use tempfile::TempDir;

struct MockLintProvider;

#[async_trait]
impl memex_core::LlmProvider for MockLintProvider {
    async fn chat(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("ISSUES:\n- [stale] wiki/old-page.md: references deleted source\n\nSUGGESTED_QUESTIONS:\n- What caching strategies work best for mobile apps?\n\nSUGGESTED_SOURCES:\n- Martin Fowler's caching patterns article\n".to_string())
    }
}

#[tokio::test]
async fn lint_empty_wiki() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open(root, Box::new(MockLintProvider), "test").unwrap();
    let report = memex.lint().await.unwrap();
    assert!(report.issues.is_empty());
}

#[tokio::test]
async fn lint_detects_dangling_links() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open(root.clone(), Box::new(MockLintProvider), "test").unwrap();
    std::fs::write(
        root.join("wiki/page-a.md"),
        "---\ntitle: Page A\ntags:
  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\n\nSee [[nonexistent-page]].\n",
    ).unwrap();
    memex.reindex().unwrap();
    let report = memex.lint().await.unwrap();
    let dangling: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.kind == memex_core::types::LintIssueKind::MissingLink)
        .collect();
    assert!(!dangling.is_empty(), "Should detect dangling link");
    assert!(dangling[0].proposed_fix.is_some(), "Should propose fix");
}

#[tokio::test]
async fn lint_returns_suggestions() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("memex");
    let memex = memex_core::Memex::open(root.clone(), Box::new(MockLintProvider), "test").unwrap();
    std::fs::write(
        root.join("wiki/caching.md"),
        "---\ntitle: Caching\ntags:
  - entity\ncreated: 2026-04-06T00:00:00Z\nlast_updated: 2026-04-06T00:00:00Z\nsources: []\n---\n\nCaching content.\n",
    ).unwrap();
    memex.reindex().unwrap();
    let report = memex.lint().await.unwrap();
    assert!(!report.suggested_questions.is_empty());
    assert!(!report.suggested_sources.is_empty());
}
