//! In-process test harness for daemon protocol tests. Spawns the daemon's
//! request handler against a unix socket in a tempdir; optionally replaces
//! LLM workers with closures.

#![cfg(any(test, feature = "test-harness"))]
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::daemon::handler::{HandlerState, handle};
use crate::daemon::memex_cache::MemexCache;
use crate::daemon::protocol::{Event, Request};
use crate::daemon::worker::WorkerPool;

pub type MockClosure = Arc<dyn Fn(&str) -> String + Send + Sync>;

pub struct DaemonHarness {
    _tmpdir: TempDir,
    memex_root: PathBuf,
    socket_path: PathBuf,
    _accept_task: tokio::task::JoinHandle<()>,
}

impl DaemonHarness {
    pub async fn start() -> Self {
        Self::start_internal(None, None).await
    }

    pub async fn start_with_mock_extract<F>(extract: F) -> Self
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        Self::start_internal(Some(Arc::new(extract)), None).await
    }

    pub async fn start_with_mock_extract_and_merge<F, G>(extract: F, merge: G) -> Self
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
        G: Fn(&str) -> String + Send + Sync + 'static,
    {
        Self::start_internal(Some(Arc::new(extract)), Some(Arc::new(merge))).await
    }

    async fn start_internal(extract: Option<MockClosure>, merge: Option<MockClosure>) -> Self {
        // Pre-load ONNX runtime so handlers that call load_default_model
        // don't deadlock on Session::builder. The CLI binary does this in
        // main(); the in-process harness must do it explicitly.
        let _ = memex_core::embed::catch_unwind_silent(memex_core::embed::init_runtime);

        let tmp = TempDir::new().expect("tempdir");
        let memex_root = tmp.path().join("memex");
        std::fs::create_dir_all(&memex_root).expect("mkdir memex_root");
        let socket_path = memex_root.join("daemon.sock");

        let jobs = match extract {
            Some(extract_fn) => Arc::new(WorkerPool::new_with_mock(extract_fn, merge)),
            None => Arc::new(WorkerPool::new_inert_for_test()),
        };

        // Stub retrieval: ingest paths don't depend on it; query paths do.
        // This builds an mpsc channel whose receiver is dropped — any
        // request submitted to it surfaces as a closed-channel send error,
        // which the handler maps to an internal error event. Sufficient
        // for ingest-shape tests; query tests should run against a real
        // retrieval actor.
        let retrieval = retrieval_stub_for_tests();

        let state = Arc::new(HandlerState {
            pid: std::process::id(),
            started_at: chrono::Utc::now(),
            retrieval,
            jobs,
            memex_cache: MemexCache::new(),
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
            config: Arc::new(crate::daemon::config::Config::default()),
        });

        let listener = UnixListener::bind(&socket_path).expect("bind socket");
        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let state = state.clone();
                tokio::spawn(async move { handle_connection(stream, state).await });
            }
        });

        Self {
            _tmpdir: tmp,
            memex_root,
            socket_path,
            _accept_task: accept_task,
        }
    }

    pub fn memex_root(&self) -> &Path {
        &self.memex_root
    }

    pub async fn send(&self, req: &Request) -> std::io::Result<Vec<Event>> {
        let bytes = serde_json::to_vec(req)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let raw = std::str::from_utf8(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.send_raw(raw).await
    }

    pub async fn send_raw(&self, raw: &str) -> std::io::Result<Vec<Event>> {
        let mut stream = UnixStream::connect(&self.socket_path).await?;
        stream.write_all(raw.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        // Half-close the write side so the daemon knows we're done sending.
        stream.shutdown().await?;
        let mut reader = BufReader::new(stream);
        let mut events = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await? {
                0 => break,
                _ => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let event: Event = serde_json::from_str(trimmed)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    events.push(event);
                }
            }
        }
        Ok(events)
    }
}

async fn handle_connection(mut stream: UnixStream, state: Arc<HandlerState>) {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&mut stream);
        if reader.read_line(&mut line).await.is_err() {
            return;
        }
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let err = Event::Error {
                code: "bad_request".into(),
                message: format!("malformed Request JSON: {e}"),
                status: 1,
            };
            if let Ok(bytes) = serde_json::to_vec(&err) {
                let _ = stream.write_all(&bytes).await;
                let _ = stream.write_all(b"\n").await;
            }
            return;
        }
    };
    let events = handle(req, &state).await;
    for ev in events {
        let bytes = match serde_json::to_vec(&ev) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if stream.write_all(&bytes).await.is_err() {
            break;
        }
        if stream.write_all(b"\n").await.is_err() {
            break;
        }
    }
}

/// Build a stub `RetrievalSender` whose receiver is immediately dropped.
/// Any request submitted via this sender fails with a closed-channel
/// error, which the handler propagates as an internal-error event.
/// Ingest-shape tests don't issue retrieval requests, so the dead
/// receiver never matters; query-shape tests should use a real
/// retrieval actor instead.
fn retrieval_stub_for_tests() -> crate::daemon::retrieval::RetrievalSender {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    tx
}
