use crate::integration_harness::IntegrationHarness;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_with_from_line_and_max_lines_emits_slice_with_header_suffix() {
    let harness = IntegrationHarness::start().await;
    let body: String = (1..=20).map(|i| format!("L{i}\n")).collect();
    harness.write("p", &body).await.unwrap();
    let out = harness
        .cli(&["read", "p", "--from-line", "5", "--max-lines", "3"])
        .await;
    assert!(
        out.contains("[lines 5..7 of 20]"),
        "header missing slice info: {out}"
    );
    assert!(
        out.contains("L5\nL6\nL7"),
        "expected sliced body, got: {out}"
    );
    assert!(!out.contains("L4"), "L4 leaked in: {out}");
    assert!(!out.contains("L8"), "L8 leaked in: {out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_multi_ref_with_from_line_errors() {
    let harness = IntegrationHarness::start().await;
    harness.write("a", "x").await.unwrap();
    harness.write("b", "y").await.unwrap();
    let out = harness
        .cli_expect_err(&["read", "a", "b", "--from-line", "1"])
        .await;
    assert!(out.contains("require a single ref"), "got: {out}");
}

#[test]
fn read_from_line_beyond_eof_returns_empty_body() {
    let body = "L1\nL2\nL3\n";
    let (sliced, (start, end)) = memex_cli::slice_body(body, Some(99), None);
    assert_eq!(sliced, "");
    assert_eq!((start, end), (0, 0));
}

#[test]
fn slice_body_default_returns_whole_body() {
    let body = "L1\nL2\nL3\n";
    let (sliced, (start, end)) = memex_cli::slice_body(body, None, None);
    assert_eq!(sliced, body);
    assert_eq!((start, end), (1, 3));
}

#[test]
fn slice_body_clamps_max_lines_at_eof() {
    let body = "L1\nL2\nL3\n";
    let (sliced, (start, end)) = memex_cli::slice_body(body, Some(2), Some(99));
    assert!(sliced.contains("L2"));
    assert!(sliced.contains("L3"));
    assert_eq!((start, end), (2, 3));
}
