use crate::integration_harness::IntegrationHarness;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiki_delete_blocks_when_backlinks_exist() {
    let harness = IntegrationHarness::start().await;
    harness.write("Auth", "auth body").await.unwrap();
    harness
        .write("Login", "see [[auth]] for context")
        .await
        .unwrap();
    let err = harness.delete("auth", false).await.unwrap_err();
    let s = err.to_string();
    assert!(s.contains("link"), "expected backlink message; got: {s}");

    // File still exists.
    assert!(harness.root().join("wiki/auth.md").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiki_delete_force_proceeds_despite_backlinks() {
    let harness = IntegrationHarness::start().await;
    harness.write("Auth", "auth body").await.unwrap();
    harness
        .write("Login", "see [[auth]] for context")
        .await
        .unwrap();
    harness.delete("auth", true).await.unwrap();
    assert!(!harness.root().join("wiki/auth.md").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiki_delete_no_backlinks_proceeds_without_force() {
    let harness = IntegrationHarness::start().await;
    harness.write("Auth", "no backlinks").await.unwrap();
    harness.delete("auth", false).await.unwrap();
    assert!(!harness.root().join("wiki/auth.md").exists());
}
