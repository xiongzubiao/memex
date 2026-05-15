use crate::integration_harness::IntegrationHarness;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_creates_wiki_file_and_indexes() {
    let harness = IntegrationHarness::start().await;
    let resp = harness
        .write("Auth Tokens", "# Auth Tokens\n\nbearer body")
        .await
        .unwrap();
    let docid = resp.docid;
    assert_eq!(docid.len(), 7);

    let path = harness.root().join("wiki/auth-tokens.md");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("title: Auth Tokens"));
    assert!(content.contains("bearer body"));

    let body_hash = memex_core::storage::content_hash(b"# Auth Tokens\n\nbearer body");
    assert_eq!(docid, &body_hash[..7]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_idempotent_when_body_unchanged() {
    let harness = IntegrationHarness::start().await;
    let r1 = harness.write("Auth", "same body").await.unwrap();
    let r2 = harness.write("Auth", "same body").await.unwrap();
    assert_eq!(r1.docid, r2.docid);
}
