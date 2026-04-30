//! Integration-test harness: runs the daemon's request handler
//! **in-process** against a Unix socket in a tempdir. Optional mock
//! workers replace the LLM extract/merge subprocesses.
//!
//! Use this for protocol-shape tests, handler-logic tests, and anything
//! where you want to assert on daemon state without paying the ~5s
//! subprocess-spawn cost. Does NOT exercise:
//!   - `daemonize_child` (fork, fd close, /dev/null redirection)
//!   - `setsid` / process-group detachment
//!   - Pidfile lifecycle and stale detection
//!   - Real signal handling on `daemon stop`
//!   - The `connect_or_spawn` race when N CLI processes hit a cold socket
//!   - ONNX init in a fresh process
//!
//! For those, use `e2e_harness::E2EHarness` instead.


#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use memex_cli::daemon::handler::{HandlerState, handle};
use memex_cli::daemon::memex_handle::MemexHandle;
use memex_cli::daemon::protocol::{Event, Request};
use memex_cli::daemon::worker::WorkerPool;

pub type MockClosure = Arc<dyn Fn(&str) -> String + Send + Sync>;

#[derive(Debug)]
pub struct WriteResp {
    pub docid: String,
}

#[derive(Debug)]
pub struct SourceAddResp {
    pub docid: String,
}

#[derive(Debug)]
pub struct IngestDocumentResp {
    pub source_docid: String,
    pub wiki_pages: Vec<String>,
}

pub struct IntegrationHarness {
    _tmpdir: TempDir,
    memex_root: PathBuf,
    socket_path: PathBuf,
    _accept_task: tokio::task::JoinHandle<()>,
}

impl IntegrationHarness {
    pub async fn start() -> Self {
        Self::start_internal(None, None).await
    }

    /// Start the harness with a real retrieval actor backed by the default
    /// ONNX embedding model. Required for query-shape tests; ingest tests
    /// keep using the cheaper `start()`.
    ///
    /// Errors propagate from the model loader (file missing, ONNX dylib
    /// unavailable). In test environments where the model isn't installed,
    /// the caller should `expect(...)` and let the test fail loudly.
    pub async fn start_with_real_retrieval()
    -> std::result::Result<Self, memex_core::error::MemexError> {
        Self::start_internal_real_retrieval(None, None).await
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

    /// Like `start_with_mock_extract_and_merge` but also loads the real
    /// ONNX embedder so dedup-by-title can run. Required by tests that
    /// exercise the merge path: without an embed model, every proposed
    /// page is treated as new, the MERGE worker is never invoked.
    pub async fn start_with_mock_extract_and_merge_with_embed<F, G>(
        extract: F,
        merge: G,
    ) -> std::result::Result<Self, memex_core::error::MemexError>
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
        G: Fn(&str) -> String + Send + Sync + 'static,
    {
        Self::start_internal_real_retrieval(Some(Arc::new(extract)), Some(Arc::new(merge))).await
    }

    async fn start_internal_real_retrieval(
        extract: Option<MockClosure>,
        merge: Option<MockClosure>,
    ) -> std::result::Result<Self, memex_core::error::MemexError> {
        let _ = memex_core::embed::catch_unwind_silent(memex_core::embed::init_runtime);

        let tmp = TempDir::new().expect("tempdir");
        let memex_root = tmp.path().join("memex");
        std::fs::create_dir_all(&memex_root).expect("mkdir memex_root");
        let socket_path = memex_root.join("daemon.sock");

        let jobs = match extract {
            Some(extract_fn) => Arc::new(WorkerPool::new_with_mock(extract_fn, merge)),
            None => Arc::new(WorkerPool::new_inert_for_test()),
        };

        let memex_handle = MemexHandle::new();
        // Use the real model when available (vector-search tests need it),
        // otherwise fall back to a deterministic mock so dispatch-only
        // tests still run on CI without the ONNX bundle. Same instance
        // is shared with the retrieval actor — mirrors production.
        let embed_model = match memex_core::retrieval::load_default_model() {
            Ok(m) => memex_cli::daemon::handler::shared_embedder(m),
            Err(e) => {
                eprintln!("test harness: embed model load failed ({e}); using MockEmbedder");
                memex_cli::daemon::handler::shared_embedder(memex_core::embed::MockEmbedder)
            }
        };
        let retrieval =
            memex_cli::daemon::retrieval::spawn(memex_handle.clone(), embed_model.clone());

        let reader_session = memex_cli::daemon::handler::ReaderSession {
            bound_root: memex_root.clone(),
            memex_handle,
            embed_model,
        };
        let writer_session = memex_cli::daemon::handler::WriterSession {
            reader: reader_session,
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
        };
        let state = Arc::new(HandlerState {
            pid: std::process::id(),
            started_at: chrono::Utc::now(),
            retrieval,
            jobs,
            config: Arc::new(memex_cli::daemon::config::Config::default()),
            writer: writer_session,
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

        Ok(Self {
            _tmpdir: tmp,
            memex_root,
            socket_path,
            _accept_task: accept_task,
        })
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

        let reader_session = memex_cli::daemon::handler::ReaderSession {
            bound_root: memex_root.clone(),
            memex_handle: MemexHandle::new(),
            // Fast harness: MockEmbedder satisfies the type without
            // touching ONNX, but `dedup_against_existing` still gets a
            // working `Embedder` so its title-search path is exercised.
            embed_model: memex_cli::daemon::handler::shared_embedder(
                memex_core::embed::MockEmbedder,
            ),
        };
        let writer_session = memex_cli::daemon::handler::WriterSession {
            reader: reader_session,
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
        };
        let state = Arc::new(HandlerState {
            pid: std::process::id(),
            started_at: chrono::Utc::now(),
            retrieval,
            jobs,
            config: Arc::new(memex_cli::daemon::config::Config::default()),
            writer: writer_session,
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

    pub fn root(&self) -> &Path {
        &self.memex_root
    }

    pub fn raw_dir(&self) -> std::path::PathBuf {
        self.memex_root.join("raw")
    }

    pub async fn source_add(
        &self,
        source_path: &str,
        body: &str,
    ) -> Result<SourceAddResp, Box<dyn std::error::Error>> {
        let req = Request::SourceAdd {
            source_path: source_path.to_string(),
            content: body.to_string(),
            collections: vec![],
        };
        let events = self.send(&req).await?;
        for ev in &events {
            if let Event::SourceAdded { docid } = ev {
                return Ok(SourceAddResp { docid: docid.clone() });
            }
        }
        Err(format!("source_add returned no SourceAdded event: {events:?}").into())
    }

    pub async fn ingest_document(
        &self,
        source_path: &str,
        body: &str,
    ) -> Result<IngestDocumentResp, Box<dyn std::error::Error>> {
        let req = Request::Ingest {
            source: memex_cli::daemon::protocol::IngestSource::Document {
                source_path: source_path.to_string(),
                content: body.to_string(),
            },
            collections: vec![],
        };
        let events = self.send(&req).await?;
        for ev in &events {
            if let Event::Stored {
                source_docid,
                wiki_pages,
                ..
            } = ev
            {
                return Ok(IngestDocumentResp {
                    source_docid: source_docid.clone(),
                    wiki_pages: wiki_pages.clone(),
                });
            }
        }
        Err(format!("ingest_document returned no Stored event: {events:?}").into())
    }

    pub async fn delete(&self, slug: &str, force: bool) -> Result<(), Box<dyn std::error::Error>> {
        let req = Request::Delete {
            slug: slug.to_string(),
            force,
        };
        let events = self.send(&req).await?;
        for ev in &events {
            if let Event::Deleted { .. } = ev {
                return Ok(());
            }
            if let Event::Error { message, .. } = ev {
                return Err(message.clone().into());
            }
        }
        Err(format!("delete returned no Deleted event: {events:?}").into())
    }

    pub async fn write(&self, name: &str, body: &str) -> std::io::Result<WriteResp> {
        let req = Request::Write {
            title: name.to_string(),
            content: body.to_string(),
            tags: vec![],
            source: None,
            force: true,
        };
        let events = self.send(&req).await?;
        for ev in &events {
            if let Event::Written { docid, .. } = ev {
                return Ok(WriteResp { docid: docid.clone() });
            }
        }
        Err(std::io::Error::other(format!(
            "write returned no Written event: {events:?}"
        )))
    }

    /// Send `Request::Query { raw: true, top_k: 10, intent }` and return
    /// the entries from the resulting `Event::Context`.
    pub async fn query_raw(
        &self,
        question: &str,
        intent: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        let req = Request::Query {
            question: question.to_string(),
            raw: true,
            top_k: 10,
            collections: vec![],
            intent: intent.map(String::from),
        };
        let events = self.send(&req).await?;
        for ev in &events {
            if let Event::Context { entries } = ev {
                return Ok(entries.clone());
            }
            if let Event::Error { message, .. } = ev {
                return Err(format!("query error: {message}").into());
            }
        }
        Err(format!("query returned no Context event: {events:?}").into())
    }

    /// Resolve the `memex` binary path. During `cargo test`, Cargo sets
    /// `CARGO_BIN_EXE_memex` on the test runner environment; fall back to
    /// the binary next to the current test executable for other invocations.
    fn memex_bin() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("CARGO_BIN_EXE_memex") {
            return std::path::PathBuf::from(p);
        }
        // Derive from the current executable (e.g. target/debug/deps/read_ranged_test-…)
        // → target/debug/memex
        let exe = std::env::current_exe().expect("current_exe");
        // Go up: deps → debug/release → target
        let parent = exe.parent().unwrap(); // deps/
        let profile_dir = parent.parent().unwrap_or(parent); // debug or release
        profile_dir.join("memex")
    }

    /// Run the `memex` binary with the given args against this harness's root.
    /// Returns stdout on success (exit 0).
    ///
    /// Panics if the binary exits non-zero — use `cli_expect_err` for that.
    pub async fn cli(&self, args: &[&str]) -> String {
        let bin = Self::memex_bin();
        let out = std::process::Command::new(&bin)
            .args(args)
            .env("MEMEX_ROOT", &self.memex_root)
            .output()
            .unwrap_or_else(|e| panic!("failed to run memex binary at {}: {e}", bin.display()));
        assert!(
            out.status.success(),
            "memex {:?} failed (exit {:?}):\nstdout: {}\nstderr: {}",
            args,
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run the `memex` binary and expect a non-zero exit. Returns stderr (falls
    /// back to stdout if stderr is empty) for the caller to assert on.
    ///
    /// Panics if the binary exits zero.
    pub async fn cli_expect_err(&self, args: &[&str]) -> String {
        let bin = Self::memex_bin();
        let out = std::process::Command::new(&bin)
            .args(args)
            .env("MEMEX_ROOT", &self.memex_root)
            .output()
            .unwrap_or_else(|e| panic!("failed to run memex binary at {}: {e}", bin.display()));
        assert!(
            !out.status.success(),
            "memex {:?} unexpectedly succeeded",
            args,
        );
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !stderr.trim().is_empty() {
            stderr
        } else {
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
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
fn retrieval_stub_for_tests() -> memex_cli::daemon::retrieval::RetrievalSender {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    tx
}
