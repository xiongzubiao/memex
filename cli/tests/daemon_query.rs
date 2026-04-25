//! Integration test for daemon query handling.
//!
//! Exercises the query request path directly against the daemon handler so we
//! can verify collection filtering without relying on Unix socket startup in
//! the sandboxed test environment.

mod common;

use memex_cli::daemon::handler::{HandlerState, handle};
use memex_cli::daemon::memex_cache::MemexCache;
use memex_cli::daemon::protocol::{Event, Request};
use memex_cli::daemon::retrieval::{Entry, RetrievalReq, RetrievalResp};
use memex_core::Memex;
use memex_core::retrieval::Signal;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

fn spawn_retrieval_actor(
    root: PathBuf,
) -> (
    tokio::sync::mpsc::Sender<RetrievalReq>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<RetrievalReq>(1);
    let handle = tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let memex = Memex::open(root.clone()).unwrap();
            let results = memex
                .search()
                .search_by_doc_type_in_collections(
                    &req.question,
                    "wiki",
                    req.top_k,
                    &req.collections,
                )
                .unwrap();

            let entries = results
                .into_iter()
                .enumerate()
                .map(|(idx, r)| {
                    let title = std::path::Path::new(&r.path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| r.title.clone());
                    Entry {
                        id: r.docid.clone(),
                        title,
                        doc_type: r.doc_type.clone(),
                        rank: (idx + 1) as u32,
                        signal: Signal::Strong,
                        body: std::fs::read_to_string(root.join(&r.path)).unwrap_or_default(),
                    }
                })
                .collect();

            let _ = req.reply.send(Ok(RetrievalResp {
                entries,
                signal: Signal::Strong,
            }));
        }
    });
    (tx, handle)
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

fn test_state(
    retrieval: tokio::sync::mpsc::Sender<RetrievalReq>,
) -> HandlerState {
    HandlerState {
        pid: 1234,
        started_at: chrono::Utc::now(),
        retrieval,
        jobs: Arc::new(memex_cli::daemon::worker::WorkerPool::new(
            memex_cli::daemon::config::WorkerConfig::default(),
        )),
        memex_cache: MemexCache::new(),
        slug_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        config: Arc::new(memex_cli::daemon::config::Config::default()),
    }
}

#[tokio::test]
async fn query_raw_returns_indexed_entry() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("memex");

    common::ingest_page(
        &root,
        None,
        "auth-migration-timeline",
        "Auth Migration Timeline",
        "- 2026-04-16: Production rollout begins",
    );

    let (retrieval, _join) = spawn_retrieval_actor(root.clone());
    let state = test_state(retrieval);

    let events = handle(
        Request::Query {
            question: "production rollout begins 2026".into(),
            raw: true,
            top_k: 5,
            collections: vec![],
            memex_root: root.to_string_lossy().to_string(),
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
        None,
        "default-article",
        "Default Article",
        "shared query target in the default collection",
    );
    common::ingest_page(
        &root,
        None,
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

    let (retrieval, _join) = spawn_retrieval_actor(root.clone());
    let state = test_state(retrieval);

    let events = handle(
        Request::Query {
            question: "shared query target".into(),
            raw: true,
            top_k: 5,
            collections: vec![],
            memex_root: root.to_string_lossy().to_string(),
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
        None,
        "default-article",
        "Default Article",
        "shared query target in the default collection",
    );
    common::ingest_page(
        &root,
        None,
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

    let (retrieval, _join) = spawn_retrieval_actor(root.clone());
    let state = test_state(retrieval);

    let events = handle(
        Request::Query {
            question: "shared query target".into(),
            raw: true,
            top_k: 5,
            collections: vec!["team-b".into()],
            memex_root: root.to_string_lossy().to_string(),
        },
        &state,
    )
    .await;

    let titles = context_entry_titles(&events);
    assert!(titles.iter().any(|s| s == "team-b-article"));
    assert!(!titles.iter().any(|s| s == "default-article"));
}
