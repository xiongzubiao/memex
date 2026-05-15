//! Integration tests for `Request::Search` (the daemon-routed
//! title→slug lookup that backs `memex search`).
//!
//! Verifies:
//! - Match returns SearchResult with the slug.
//! - No match returns SearchResult with `slug: None`.
//! - The strong-BM25 short-circuit path produces the expected slug
//!   (verified end-to-end; the function's internal short-circuit is
//!   exercised because the seeded pages give an unambiguous BM25 winner,
//!   so the embedder is never invoked).

use crate::common;
use memex_cli::daemon::handler::{HandlerState, ReaderSession, WriterSession, handle};
use memex_cli::daemon::memex_handle::MemexHandle;
use memex_cli::daemon::protocol::{Event, Request};
use memex_cli::daemon::retrieval::RetrievalReq;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

/// Spawn a stub retrieval actor — the search handler doesn't use it,
/// but `HandlerState` needs a live channel to be a valid struct.
fn stub_retrieval() -> (
    tokio::sync::mpsc::Sender<RetrievalReq>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<RetrievalReq>(1);
    let handle = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    (tx, handle)
}

fn test_state(
    retrieval: tokio::sync::mpsc::Sender<RetrievalReq>,
    bound_root: PathBuf,
) -> HandlerState {
    let reader_session = ReaderSession {
        bound_root,
        memex_handle: MemexHandle::new(),
        // Strong-BM25 short-circuit fires for an unambiguous title hit,
        // so MockEmbedder is never invoked here — keeps the test ONNX-free.
        embed_model: memex_cli::daemon::handler::shared_embedder(memex_core::embed::MockEmbedder),
    };
    let writer_session = WriterSession {
        reader: reader_session,
        slug_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        content_hash_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };
    HandlerState {
        pid: 1234,
        started_at: chrono::Utc::now(),
        retrieval,
        jobs: Arc::new(memex_cli::daemon::worker::WorkerPool::new(
            memex_cli::daemon::config::WorkerConfig::default(),
            MemexHandle::new(),
        )),
        config: Arc::new(memex_cli::daemon::config::Config::default()),
        writer: writer_session,
    }
}

fn slug_from_events(events: &[Event]) -> Option<Option<String>> {
    events.iter().find_map(|ev| match ev {
        Event::SearchResult { slug } => Some(slug.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn search_returns_top_match_via_strong_bm25_short_circuit() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
        "auth-tokens",
        "Auth Tokens",
        "Bearer tokens authenticate API requests.",
    );
    common::ingest_page(
        &root,
        "caching-strategies",
        "Caching Strategies",
        "Cache layers reduce latency.",
    );

    let (retrieval, _join) = stub_retrieval();
    let state = test_state(retrieval, root.clone());

    let events = handle(
        Request::Search {
            title: "Auth Tokens".into(),
        },
        &state,
    )
    .await;

    let slug = slug_from_events(&events).expect("expected SearchResult event");
    assert_eq!(
        slug,
        Some("auth-tokens".into()),
        "strong-BM25 short-circuit should return the unambiguous match"
    );
}

#[tokio::test]
async fn search_returns_none_for_missing_title() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
        "auth-tokens",
        "Auth Tokens",
        "Bearer tokens authenticate API requests.",
    );

    let (retrieval, _join) = stub_retrieval();
    let state = test_state(retrieval, root.clone());

    let events = handle(
        Request::Search {
            title: "Nonexistent Topic Xyzzy".into(),
        },
        &state,
    )
    .await;

    let slug = slug_from_events(&events).expect("expected SearchResult event");
    assert_eq!(slug, None, "missing title must produce slug: None");
}
