//! `Request::SourcePlan` and `Request::PlanApply` handlers.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{
    HandlerState, acquire_content_hash_lock, error_events, get_or_open_memex,
};
use crate::daemon::protocol::Event;

/// Handle `Request::SourcePlan`. Resolves the source by docid, runs
/// EXTRACT (chunked) + MERGE-dry-run for overlaps, emits PlanContent.
pub(super) async fn handle_source_plan(source_id: String, state: &HandlerState) -> Vec<Event> {
    let memex = match get_or_open_memex(state.writer.memex_handle(), state.writer.bound_root()) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };

    // Resolve docid prefix to a raw source row.
    let docs = match memex.search().resolve_ref_documents(&source_id) {
        Ok(d) => d,
        Err(e) => return error_events(DaemonError::Storage(e.to_string())),
    };
    let source_doc = match docs.into_iter().find(|d| d.doc_type == "raw") {
        Some(d) => d,
        None => {
            return error_events(DaemonError::BadRequest(format!(
                "source not found: '{source_id}'. Run `memex source list` to find docids."
            )));
        }
    };

    let content_hash = source_doc.hash.clone();

    // Acquire the per-content-hash lock for the EXTRACT/MERGE phase.
    let _hash_guard = acquire_content_hash_lock(&state.writer, &content_hash).await;

    // (Tasks 7-8: EXTRACT + MERGE-dry-run)
    let _ = (memex, source_doc, content_hash);
    error_events(DaemonError::Internal(
        "source_plan: EXTRACT/MERGE not yet implemented".into(),
    ))
}

/// Handle `Request::PlanApply`. Stubbed until Tasks 12-13 implement the
/// per-proposal commit logic.
pub(super) async fn handle_plan_apply(_plan_json: String, _state: &HandlerState) -> Vec<Event> {
    error_events(DaemonError::Internal(
        "plan_apply: not yet implemented".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::handler::{ReaderSession, WriterSession};
    use crate::daemon::memex_handle::MemexHandle;
    use chrono::Utc;
    use memex_core::embed::MockEmbedder;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};

    fn test_state(root: PathBuf) -> HandlerState {
        let (r_tx, _r_rx) = tokio::sync::mpsc::channel(1);
        let reader = ReaderSession {
            bound_root: root,
            memex_handle: MemexHandle::new(),
            embed_model: crate::daemon::handler::shared_embedder(MockEmbedder),
        };
        let writer = WriterSession {
            reader,
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
            content_hash_locks: Arc::new(StdMutex::new(HashMap::new())),
        };
        HandlerState {
            pid: 1234,
            started_at: Utc::now(),
            retrieval: r_tx,
            jobs: Arc::new(crate::daemon::worker::WorkerPool::new_inert_for_test()),
            config: Arc::new(crate::daemon::config::Config::default()),
            writer,
        }
    }

    #[tokio::test]
    async fn source_plan_unknown_docid_returns_bad_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        // Initialize an empty memex root by opening it once.
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        let state = test_state(root);
        let events = handle_source_plan("src-doesnotexist".into(), &state).await;
        assert!(matches!(&events[0], Event::Error { code, .. } if code == "bad_request"));
    }
}
