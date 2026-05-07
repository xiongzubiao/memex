//! Integration test for daemon query handling.
//!
//! Exercises the query request path directly against the daemon handler so we
//! can verify collection filtering without relying on Unix socket startup in
//! the sandboxed test environment.


use crate::common;
use memex_cli::daemon::handler::{
    HandlerState, ReaderSession, SharedEmbedder, WriterSession, handle, shared_embedder,
};
use memex_cli::daemon::memex_handle::MemexHandle;
use memex_cli::daemon::protocol::{Event, Request};
use memex_cli::daemon::retrieval;
use memex_core::Memex;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

/// Build a HandlerState wired to the **production** retrieval actor
/// (title-FTS + chunk-FTS + vector + RRF + signal classification).
/// Sharing the MemexHandle and SharedEmbedder between the actor and
/// the handler keeps test and production query paths in lockstep —
/// avoids the test-vs-prod divergence the previous stub permitted.
fn test_state(root: PathBuf) -> HandlerState {
    let memex_handle = MemexHandle::new();
    let _ = memex_handle.get_or_open(&root);
    let embed_model: SharedEmbedder = shared_embedder(memex_core::embed::MockEmbedder);
    let retrieval_tx = retrieval::spawn(memex_handle.clone(), embed_model.clone());
    let reader = ReaderSession {
        bound_root: root,
        memex_handle,
        embed_model,
    };
    let writer = WriterSession {
        reader,
        slug_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        content_hash_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };
    HandlerState {
        pid: 1234,
        started_at: chrono::Utc::now(),
        retrieval: retrieval_tx,
        jobs: Arc::new(memex_cli::daemon::worker::WorkerPool::new_inert_for_test()),
        config: Arc::new(memex_cli::daemon::config::Config::default()),
        writer,
    }
}

fn context_entry_titles(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .find_map(|ev| match ev {
            Event::Context { entries } => Some(
                entries
                    .iter()
                    .map(|entry| {
                        entry
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn query_raw_returns_indexed_entry() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
                "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    let state = test_state(root.clone());

    let events = handle(
        Request::Query {
            question: "production rollout begins 2026".into(),
            raw: true,
            top_k: 5,
            collections: vec![],
            intent: None,
        },
        &state,
    )
    .await;

    let titles = context_entry_titles(&events);
    assert!(titles.iter().any(|s| s == "auth-migration-timeline"));
}

#[tokio::test]
async fn query_raw_with_default_collection_excludes_non_default_docs() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
                "default-article",
        "Default Article",
        "shared query target in the default collection",
    );
    common::ingest_page(
        &root,
                "project-article",
        "Project Article",
        "shared query target in the project collection",
    );

    let memex = Memex::open(root.clone()).unwrap();
    memex
        .search()
        .set_document_collections_by_path(
            "wiki",
            "wiki/project-article.md",
            &[String::from("project-a")],
        )
        .unwrap();

    let state = test_state(root.clone());

    let events = handle(
        Request::Query {
            question: "shared query target".into(),
            raw: true,
            top_k: 5,
            collections: vec![],
            intent: None,
        },
        &state,
    )
    .await;

    let titles = context_entry_titles(&events);
    assert!(titles.iter().any(|s| s == "default-article"));
    assert!(!titles.iter().any(|s| s == "project-article"));
}

#[tokio::test]
async fn query_raw_with_explicit_collection_includes_only_matching_docs() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
                "default-article",
        "Default Article",
        "shared query target in the default collection",
    );
    common::ingest_page(
        &root,
                "team-b-article",
        "Team B Article",
        "shared query target in team b",
    );

    let memex = Memex::open(root.clone()).unwrap();
    memex
        .search()
        .set_document_collections_by_path(
            "wiki",
            "wiki/team-b-article.md",
            &[String::from("team-b")],
        )
        .unwrap();

    let state = test_state(root.clone());

    let events = handle(
        Request::Query {
            question: "shared query target".into(),
            raw: true,
            top_k: 5,
            collections: vec!["team-b".into()],
            intent: None,
        },
        &state,
    )
    .await;

    let titles = context_entry_titles(&events);
    assert!(titles.iter().any(|s| s == "team-b-article"));
    assert!(!titles.iter().any(|s| s == "default-article"));
}

/// Empty/whitespace-only questions must be rejected with a BadRequest
/// error. Found via strict e2e — without the guard, the retrieval
/// pipeline runs with a zero-vector embedding and returns ALL chunks
/// (every vector is equally close to zero), so a typo'd `memex query
/// ""` dumps the entire wiki.
#[tokio::test]
async fn query_rejects_empty_question() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");
    common::ingest_page(&root, "any-page", "Any", "any body");

    let state = test_state(root.clone());

    for blank in &["", "   ", "\n\t  \n"] {
        let events = handle(
            Request::Query {
                question: blank.to_string(),
                raw: true,
                top_k: 5,
                collections: vec![],
                intent: None,
            },
            &state,
        )
        .await;
        let has_bad_request = events.iter().any(|ev| {
            matches!(
                ev,
                Event::Error { code, .. } if code == "bad_request"
            )
        });
        assert!(
            has_bad_request,
            "blank question {blank:?} must produce a bad_request error; got events: {events:?}"
        );
        // No Context event — the pipeline must short-circuit, not run.
        let titles = context_entry_titles(&events);
        assert!(titles.is_empty(), "blank question must not return results: {titles:?}");
    }
}
