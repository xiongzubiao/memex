mod common;

use common::DaemonHarness;
use memex_cli::daemon::protocol::{Event, Request};

#[tokio::test(flavor = "multi_thread")]
async fn harness_starts_and_handles_status() {
    let h = DaemonHarness::start().await;
    let req = Request::Ping {};
    let events = h.send(&req).await.expect("send");
    assert!(
        events.iter().any(|e| matches!(e, Event::Pong { .. })),
        "expected Pong event, got: {events:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn harness_send_raw_returns_bad_request_on_garbage() {
    let h = DaemonHarness::start().await;
    let events = h.send_raw("not json").await.expect("send_raw");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Error { code, .. } if code == "bad_request")),
        "expected bad_request error, got: {events:?}",
    );
}
