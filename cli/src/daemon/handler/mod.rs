//! Per-connection request handler.
//!
//! Called by `server.rs` for each accepted connection. Handles exactly
//! one request per connection and emits a stream of `Event`s terminated
//! by a `done` event.
//!
//! Per-request implementations live in submodules; this module owns
//! session types, shared utilities, and the dispatch entry point.

use crate::daemon::error::DaemonError;
use crate::daemon::memex_handle::MemexHandle;
use crate::daemon::protocol::{Event, Request};
use chrono::Utc;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};

mod delete;
mod ingest;
mod lint_fix;
mod plan;
mod query;
mod search;
mod source;
mod write;

/// Read-only session: long-lived state any reader needs. `bound_root`
/// is the single memex root the daemon is serving; `memex_handle`
/// is the shared lazy-opened Memex; `embed_model` is the warm
/// embedder used for vector probes — a real ONNX-backed
/// `EmbeddingModel` in production, a `MockEmbedder` in tests.
/// Cloning this is cheap (`Arc::clone`).
#[derive(Clone)]
pub struct ReaderSession {
    pub bound_root: PathBuf,
    pub memex_handle: Arc<MemexHandle>,
    pub embed_model: SharedEmbedder,
}

/// Type alias for the daemon-wide shared embedder. Wraps an
/// `Arc<TokioMutex<Box<dyn Embedder>>>` so callsites don't repeat the
/// triple-indirection. Use [`shared_embedder`] to construct one.
pub type SharedEmbedder = Arc<TokioMutex<Box<dyn memex_core::embed::Embedder>>>;

/// Wrap a concrete `Embedder` (real `EmbeddingModel` in production,
/// `MockEmbedder` in tests) into the daemon's shared form. Centralizes
/// the `Arc::new(TokioMutex::new(Box::new(_) as Box<dyn Embedder>))`
/// boilerplate.
pub fn shared_embedder<E: memex_core::embed::Embedder + 'static>(embedder: E) -> SharedEmbedder {
    Arc::new(TokioMutex::new(
        Box::new(embedder) as Box<dyn memex_core::embed::Embedder>
    ))
}

/// Writer session: ReaderSession's collaborators plus the slug-lock
/// map. The map keys on **slug alone** because the daemon is bound to
/// one root for its lifetime — the old `(root, slug)` key was a
/// vestigial multi-root design hook.
///
/// Holding a `WriterSession` is the type-system signal that a code
/// path may mutate. Reader handlers borrow only `ReaderSession`, so
/// they can't accidentally reach for slug locks or hold the writer's
/// lifecycle assumptions.
#[derive(Clone)]
pub struct WriterSession {
    pub reader: ReaderSession,
    /// Per-slug write locks. Acquired before `dedup_against_existing`
    /// reads the existing wiki file and released after `write_wiki_files`,
    /// so parallel ingests targeting the same slug don't race on the
    /// read-modify-write sequence. The outer `StdMutex` only guards
    /// the map (brief); the inner `TokioMutex` is held across the
    /// long-running MERGE LLM call via `.await`.
    pub slug_locks: Arc<StdMutex<HashMap<String, Arc<TokioMutex<()>>>>>,
    /// Per-content-hash advisory locks. Held by `source plan` for the
    /// duration of EXTRACT + MERGE-dry-run only; does NOT serialize
    /// against `plan apply` (no shared state). Same map shape as
    /// `slug_locks` so cleanup heuristics match.
    pub content_hash_locks: Arc<StdMutex<HashMap<String, Arc<TokioMutex<()>>>>>,
}

impl WriterSession {
    pub fn memex_handle(&self) -> &Arc<MemexHandle> {
        &self.reader.memex_handle
    }
    pub fn embed_model(&self) -> &SharedEmbedder {
        &self.reader.embed_model
    }
    pub fn bound_root(&self) -> &Path {
        &self.reader.bound_root
    }
}

/// Daemon-shared state accessible to handlers. Composes a
/// `WriterSession` (which contains the `ReaderSession`) plus
/// process-level singletons that don't fit either session shape.
pub struct HandlerState {
    pub pid: u32,
    pub started_at: chrono::DateTime<Utc>,
    pub retrieval: crate::daemon::retrieval::RetrievalSender,
    pub jobs: Arc<crate::daemon::worker::WorkerPool>,
    pub config: Arc<crate::daemon::config::Config>,
    pub writer: WriterSession,
}

impl HandlerState {
    pub fn reader(&self) -> &ReaderSession {
        &self.writer.reader
    }
}

/// Acquire per-slug locks for a batch, in sorted order to avoid deadlock
/// between concurrent ingests that touch overlapping slug sets.
///
/// Keys on slug alone — the daemon is bound to one memex root for its
/// lifetime. The map grows by one `Arc<Mutex<()>>` per distinct slug
/// ever seen and is never compacted; a long-running daemon over a
/// million-page corpus would accumulate ~80 MB of map entries, which
/// is the practical upper bound rather than a leak with no ceiling.
/// Single-slug convenience over `acquire_slug_locks`. Most ingest /
/// write / delete paths touch exactly one slug; the original
/// `vec![slug.clone()]` wrapping at every call site was noise.
pub(super) async fn acquire_slug_lock(
    writer: &WriterSession,
    slug: &str,
) -> OwnedMutexGuard<()> {
    let mut guards = acquire_slug_locks(writer, vec![slug.to_string()]).await;
    guards.pop().expect("acquire_slug_locks returns one guard for one slug")
}

pub(super) async fn acquire_slug_locks(
    writer: &WriterSession,
    mut slugs: Vec<String>,
) -> Vec<OwnedMutexGuard<()>> {
    slugs.sort();
    slugs.dedup();
    let locks: Vec<Arc<TokioMutex<()>>> = {
        let mut map = writer.slug_locks.lock().expect("slug_locks map poisoned");
        slugs
            .into_iter()
            .map(|s| {
                map.entry(s)
                    .or_insert_with(|| Arc::new(TokioMutex::new(())))
                    .clone()
            })
            .collect()
    };
    let mut guards = Vec::with_capacity(locks.len());
    for lock in locks {
        guards.push(lock.lock_owned().await);
    }
    guards
}

/// Acquire the per-content-hash lock. Held during EXTRACT/MERGE-dry-run
/// for `source plan`; prevents two simultaneous LLM call chains on the
/// same source content. Released before stdout streaming.
pub(super) async fn acquire_content_hash_lock(
    writer: &WriterSession,
    content_hash: &str,
) -> OwnedMutexGuard<()> {
    let lock: Arc<TokioMutex<()>> = {
        let mut map = writer
            .content_hash_locks
            .lock()
            .expect("content_hash_locks map poisoned");
        map.entry(content_hash.to_string())
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    };
    lock.lock_owned().await
}

/// Look up the shared Memex handle for `root`, opening on first use.
/// Thin wrapper that maps cache-level errors into `DaemonError::Internal`.
pub(super) fn get_or_open_memex(
    cache: &MemexHandle,
    root: &Path,
) -> Result<Arc<memex_core::Memex>, DaemonError> {
    cache
        .get_or_open(root)
        .map_err(|e| DaemonError::Internal(format!("cannot open memex: {e}")))
}

/// `atomic_write` on the blocking thread pool, so the fsync + rename don't
/// stall the tokio runtime.
pub(super) async fn async_atomic_write(
    path: std::path::PathBuf,
    bytes: Vec<u8>,
) -> Result<(), DaemonError> {
    tokio::task::spawn_blocking(move || memex_core::storage::atomic_write(&path, &bytes))
        .await
        .map_err(|e| DaemonError::Internal(format!("write task panicked: {e}")))?
        .map_err(|e| DaemonError::Internal(format!("atomic_write failed: {e}")))
}

/// Read a file after gating on size via `metadata()` — avoids allocating
/// multi-GB before the cap check would have rejected the input.
pub(super) async fn read_file_capped(path: &Path, max_bytes: u64) -> Result<String, DaemonError> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() > max_bytes => Err(DaemonError::BadRequest(format!(
            "transcript too large: {} bytes (max {})",
            meta.len(),
            max_bytes
        ))),
        Ok(_) => tokio::fs::read_to_string(path)
            .await
            .map_err(|e| DaemonError::BadRequest(format!("cannot read transcript: {e}"))),
        Err(e) => Err(DaemonError::BadRequest(format!(
            "cannot stat transcript: {e}"
        ))),
    }
}

/// Submit a worker job and await its reply, mapping every error path into a
/// `DaemonError`. Collapses the five-arm match (submit failure, crash,
/// timeout, auth, backend) repeated across the handler.
pub(super) async fn run_worker_job<R, F>(
    state: &HandlerState,
    build_job: F,
) -> Result<R, DaemonError>
where
    R: Send + 'static,
    F: FnOnce(tokio::sync::oneshot::Sender<Result<R, crate::daemon::queue::WorkerError>>)
        -> crate::daemon::queue::BackendJob,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let job = build_job(tx);
    state
        .jobs
        .submit(job)
        .await
        .map_err(|_| DaemonError::Internal("worker queue closed".into()))?;
    match rx.await {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(DaemonError::Internal("worker dropped reply".into())),
    }
}

/// Given a parsed request, return the stream of events to emit, in order.
/// The final event is always `Event::Done { status }`.
pub async fn handle(req: Request, state: &HandlerState) -> Vec<Event> {
    match req {
        Request::Ping {} => {
            vec![
                Event::Pong {
                    pid: state.pid,
                    started_at: state.started_at.to_rfc3339(),
                },
                Event::Done { status: 0 },
            ]
        }
        Request::Query {
            question,
            raw,
            top_k,
            collections,
            intent,
        } => query::handle_query(question, raw, top_k, collections, intent, state).await,

        Request::Ingest {
            source,
            collections,
        } => match source {
            crate::daemon::protocol::IngestSource::Transcript { path, agent } => {
                ingest::handle_ingest_transcript(path, agent, collections, state).await
            }
            crate::daemon::protocol::IngestSource::TranscriptInline {
                content,
                agent,
                source_label,
            } => {
                ingest::handle_ingest_transcript_content(
                    content,
                    source_label,
                    agent,
                    collections,
                    state,
                )
                .await
            }
            crate::daemon::protocol::IngestSource::Document {
                source_path,
                content,
            } => ingest::handle_ingest_document(source_path, content, collections, state).await,
        },

        Request::Write {
            title,
            content,
            source,
            force,
        } => write::handle_write(title, content, source, force, state).await,

        Request::SourceAdd {
            source_path,
            content,
            collections,
        } => source::handle_source_add(source_path, content, collections, state).await,

        Request::SourceDelete { ref_, force } => {
            source::handle_source_delete(ref_, force, state).await
        }

        Request::Delete { slug, force } => delete::handle_delete(slug, force, state).await,

        Request::Search { title } => search::handle_search(title, state).await,

        Request::LintFix {} => lint_fix::handle_lint_fix(state).await,

        Request::SourcePlan { source_id } => plan::handle_source_plan(source_id, state).await,

        Request::PlanApply { plan_json } => plan::handle_plan_apply(plan_json, state).await,
    }
}

/// Drop an LLM expansion field that shares zero words with the user
/// query — a strong signal of hallucination. Mirrors QMD `llm.ts:1183
/// hasQueryTerm`, plus a length-2 minimum (consistent with QMD's
/// `extractIntentTerms` at store.ts:3845): single-letter tokens like
/// "s" from "what's" substring-match half the English language and
/// would degenerate the filter on contraction-heavy queries.
/// Empty input returns empty (no-op for un-set fields). When the query
/// has no useful tokens (only stopwords ≤1 char), passes text through.
pub(super) fn filter_hallucinated(text: &str, query: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let terms: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| s.len() >= 2)
        .map(String::from)
        .collect();
    if terms.is_empty() {
        return text.to_string();
    }
    let lower = text.to_lowercase();
    if terms.iter().any(|t| lower.contains(t)) {
        text.to_string()
    } else {
        String::new()
    }
}

pub(super) fn error_events(err: DaemonError) -> Vec<Event> {
    let status = err.exit_code();
    tracing::error!(code = err.code_str(), status, message = %err.message(), "handler error");
    vec![
        Event::Error {
            code: err.code_str().to_string(),
            message: err.message(),
            status,
        },
        Event::Done { status },
    ]
}

/// Reject source identifiers that are too long or contain control chars
/// that would corrupt frontmatter / logs / IPC line framing.
pub(super) fn validate_source_path(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("source path is empty".into());
    }
    if s.len() > 2048 {
        return Err(format!("source path too long: {} chars (max 2048)", s.len()));
    }
    for c in s.chars() {
        let cu = c as u32;
        if cu == 0 || (cu < 0x20 && c != '\t') {
            return Err(format!("source path contains control char (U+{:04X})", cu));
        }
    }
    Ok(())
}

/// Validate inbound document content: size cap, non-empty after redaction,
/// then apply secret redaction. Returns the redacted bytes or a typed error.
///
/// **Empty-check is post-redaction by design** — a body that's entirely a
/// redacted secret would otherwise pass the empty-check and reach Extract
/// with an empty payload. This catches that edge case.
pub(super) fn validate_and_redact_inbound_content(
    content: &str,
    max_bytes: usize,
) -> Result<String, DaemonError> {
    if content.len() > max_bytes {
        return Err(DaemonError::BadRequest(format!(
            "content too large: {} bytes (max {})",
            content.len(),
            max_bytes
        )));
    }
    let redacted = memex_core::transcript::redact_secrets(content);
    if redacted.trim().is_empty() {
        return Err(DaemonError::BadRequest(
            "empty content on stdin. The upstream converter probably exited with no \
             output. Try running it standalone first (e.g. `markitdown <url>`), or use \
             `set -o pipefail` so converter failures propagate."
                .into(),
        ));
    }
    Ok(redacted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use memex_core::embed::MockEmbedder;

    fn mock_model() -> SharedEmbedder {
        shared_embedder(MockEmbedder)
    }

    fn test_state() -> HandlerState {
        let (r_tx, _r_rx) = tokio::sync::mpsc::channel(1);
        let reader_session = ReaderSession {
            bound_root: PathBuf::from("/tmp/memex-handler-test"),
            memex_handle: MemexHandle::new(),
            embed_model: mock_model(),
        };
        let writer_session = WriterSession {
            reader: reader_session,
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
            content_hash_locks: Arc::new(StdMutex::new(HashMap::new())),
        };
        HandlerState {
            pid: 1234,
            started_at: Utc::now(),
            retrieval: r_tx,
            jobs: Arc::new(crate::daemon::worker::WorkerPool::new_inert_for_test()),
            config: Arc::new(crate::daemon::config::Config::default()),
            writer: writer_session,
        }
    }

    #[tokio::test]
    async fn ping_returns_pong_then_done() {
        let state = test_state();
        let events = handle(Request::Ping {}, &state).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::Pong { pid, .. } if *pid == 1234));
        assert!(matches!(&events[1], Event::Done { status: 0 }));
    }

    #[tokio::test]
    async fn query_raw_error_when_actor_down() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx); // actor has exited
        let reader_session = ReaderSession {
            bound_root: PathBuf::from("/tmp/memex-handler-test"),
            memex_handle: MemexHandle::new(),
            embed_model: mock_model(),
        };
        let writer_session = WriterSession {
            reader: reader_session,
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
            content_hash_locks: Arc::new(StdMutex::new(HashMap::new())),
        };
        let state = HandlerState {
            pid: 0,
            started_at: Utc::now(),
            retrieval: tx,
            jobs: Arc::new(crate::daemon::worker::WorkerPool::new_inert_for_test()),
            config: Arc::new(crate::daemon::config::Config::default()),
            writer: writer_session,
        };
        let events = handle(
            Request::Query {
                question: "q".into(),
                raw: true,
                top_k: 5,
                collections: vec![],
                intent: None,
            },
            &state,
        )
        .await;
        assert!(matches!(&events[0], Event::Error { code, .. } if code == "internal"));
        assert!(matches!(&events[1], Event::Done { status: 1 }));
    }

    #[test]
    fn filter_hallucinated_passes_when_word_overlaps() {
        let kept = filter_hallucinated(
            "Bearer tokens are short-lived credentials.",
            "how do bearer tokens work",
        );
        assert_eq!(kept, "Bearer tokens are short-lived credentials.");
    }

    #[test]
    fn filter_hallucinated_drops_when_no_word_overlaps() {
        // Note: substring matching means short query words can spuriously
        // pass (e.g. "to" ⊂ "into"). Pick query tokens that don't appear
        // anywhere in the text as substrings.
        let dropped = filter_hallucinated(
            "Photosynthesis converts sunlight via chlorophyll.",
            "rebase squash fixup",
        );
        assert_eq!(dropped, "");
    }

    #[test]
    fn filter_hallucinated_is_case_insensitive() {
        let kept = filter_hallucinated("REBASE squash fixup", "interactive rebase");
        assert_eq!(kept, "REBASE squash fixup");
    }

    #[test]
    fn filter_hallucinated_empty_text_stays_empty() {
        assert_eq!(filter_hallucinated("", "anything"), "");
    }

    #[test]
    fn filter_hallucinated_empty_query_passes_text() {
        // No tokens to compare → don't filter (matches QMD semantics).
        assert_eq!(filter_hallucinated("anything", "   "), "anything");
    }

    #[test]
    fn filter_hallucinated_drops_single_letter_tokens_from_contractions() {
        // Query "what's up" tokenizes to ["what", "s", "up"]. With the
        // length-2 minimum, the lone "s" is dropped — otherwise it would
        // substring-match into virtually any English text, degrading the
        // filter to a no-op on contraction-heavy queries.
        let dropped = filter_hallucinated(
            "Photosynthesis converts sunlight via chlorophyll.",
            "what's up",
        );
        assert_eq!(
            dropped, "",
            "single-letter token 's' from 'what's' must not save an off-topic expansion"
        );
    }

    #[test]
    fn filter_hallucinated_uses_substring_not_whole_word_match() {
        // QMD uses includes(); "fork" is a substring of "forking" so the
        // expansion stays. This is intentional — narrower than whole-word.
        let kept = filter_hallucinated("forking the repository", "fork");
        assert_eq!(kept, "forking the repository");
    }

    #[test]
    fn source_path_validator_accepts_url() {
        validate_source_path("https://example.com/post").unwrap();
    }

    #[test]
    fn source_path_validator_accepts_filesystem_path() {
        validate_source_path("/abs/path/to/file.md").unwrap();
    }

    #[test]
    fn source_path_validator_rejects_empty() {
        assert!(validate_source_path("").is_err());
    }

    #[test]
    fn source_path_validator_rejects_overlong() {
        let s = "a".repeat(2049);
        assert!(validate_source_path(&s).is_err());
    }

    #[test]
    fn source_path_validator_rejects_newline() {
        assert!(validate_source_path("https://x/p\ninjected").is_err());
    }

    #[test]
    fn source_path_validator_rejects_null() {
        assert!(validate_source_path("path\0null").is_err());
    }

    #[test]
    fn source_path_validator_accepts_tab() {
        validate_source_path("a\tb").unwrap();
    }

    #[test]
    fn validate_and_redact_passes_normal_content() {
        let r = validate_and_redact_inbound_content("# Title\n\nbody", 10_000).unwrap();
        assert!(r.contains("# Title"));
        assert!(r.contains("body"));
    }

    #[test]
    fn validate_and_redact_rejects_oversize() {
        let big = "a".repeat(1001);
        let err = validate_and_redact_inbound_content(&big, 1000).unwrap_err();
        assert!(matches!(err, DaemonError::BadRequest(_)));
    }

    #[test]
    fn validate_and_redact_rejects_post_redaction_empty() {
        // A 50-char string of all whitespace — empty after trim post-redaction.
        let r = validate_and_redact_inbound_content("   \n\t  \n  ", 1000);
        assert!(r.is_err(), "expected empty-content error, got: {r:?}");
    }

    #[test]
    fn validate_and_redact_strips_secrets_in_output() {
        let content = "see token sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA in body";
        let r = validate_and_redact_inbound_content(content, 10_000).unwrap();
        assert!(!r.contains("sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"), "got: {r}");
    }

    #[tokio::test]
    async fn content_hash_lock_serializes_concurrent_acquisitions() {
        let state = test_state();
        let hash = "deadbeef".to_string();
        let g1 = acquire_content_hash_lock(&state.writer, &hash).await;
        let writer = state.writer.clone();
        let hash2 = hash.clone();
        let racer = tokio::spawn(async move {
            let _g2 = acquire_content_hash_lock(&writer, &hash2).await;
            "second"
        });
        // Racer must NOT complete while g1 is held.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!racer.is_finished(), "racer acquired while first lock held");
        drop(g1);
        let v = racer.await.unwrap();
        assert_eq!(v, "second");
    }

    #[tokio::test]
    async fn slug_lock_same_slug_serializes() {
        let state = test_state();
        let g1 = acquire_slug_locks(&state.writer, vec!["alice".into()]).await;
        let writer = state.writer.clone();
        let racer = tokio::spawn(async move {
            let _g2 = acquire_slug_locks(&writer, vec!["alice".into()]).await;
            "second"
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !racer.is_finished(),
            "second acquisition on same slug completed while first held"
        );
        drop(g1);
        assert_eq!(racer.await.unwrap(), "second");
    }

    #[tokio::test]
    async fn slug_lock_disjoint_slugs_run_in_parallel() {
        let state = test_state();
        let g1 = acquire_slug_locks(&state.writer, vec!["alice".into()]).await;
        let writer = state.writer.clone();
        let racer = tokio::spawn(async move {
            let _g2 = acquire_slug_locks(&writer, vec!["bob".into()]).await;
            "second"
        });
        let v = tokio::time::timeout(std::time::Duration::from_millis(500), racer)
            .await
            .expect("disjoint-slug acquisition blocked behind unrelated slug")
            .unwrap();
        assert_eq!(v, "second");
        drop(g1);
    }

    #[tokio::test]
    async fn slug_lock_multi_slug_serializes_on_any_overlap() {
        let state = test_state();
        let g1 = acquire_slug_locks(&state.writer, vec!["alice".into(), "bob".into()]).await;
        let writer = state.writer.clone();
        let racer = tokio::spawn(async move {
            let _g2 = acquire_slug_locks(&writer, vec!["alice".into(), "carol".into()]).await;
            "second"
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !racer.is_finished(),
            "multi-slug acquisition completed while overlapping slug was held"
        );
        drop(g1);
        assert_eq!(racer.await.unwrap(), "second");
    }
}
