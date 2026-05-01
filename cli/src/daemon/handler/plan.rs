//! `Request::SourcePlan` and `Request::PlanApply` handlers.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{
    HandlerState, acquire_content_hash_lock, error_events, get_or_open_memex,
};
use crate::daemon::plan::{Plan, PlanSource, Proposal};
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

    // Read source content from raw store and strip frontmatter.
    let raw_path = memex.root().join(&source_doc.path);
    let raw_body = match std::fs::read_to_string(&raw_path) {
        Ok(b) => b,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("read source: {e}")));
        }
    };
    let (fm, body) = match memex_core::raw::parse_raw_frontmatter(&raw_body) {
        Ok(p) => p,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("raw frontmatter: {e}")));
        }
    };
    let source_path = fm.source.clone().unwrap_or_default();

    let pages = match crate::daemon::handler::ingest::extract_pages_from_content(
        body,
        &source_path,
        state,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return error_events(e),
    };

    if pages.is_empty() {
        return vec![
            Event::EmptyExtract {
                reason: "no extractable subjects".into(),
            },
            Event::Done { status: 0 },
        ];
    }

    // For each proposal, check if its slug exists in the wiki. If so,
    // run MERGE-dry-run; otherwise it's a new page.
    let wiki_dir = memex.wiki_dir();
    let mut proposals: Vec<Proposal> = Vec::new();
    for (idx, page) in pages.into_iter().enumerate() {
        let target_path = memex_core::wiki::wiki_path_for_slug(&wiki_dir, &page.slug);
        if !target_path.exists() {
            proposals.push(Proposal {
                index: idx,
                slug: page.slug.clone(),
                title: page.title,
                tags: page.tags,
                body: page.body,
                merge_target_slug: None,
                merge_target_hash: None,
                merge_diff: None,
                dropped: false,
                committed: false,
                original_slug: page.slug,
                error: None,
            });
            continue;
        }

        // Existing wiki page → MERGE-dry-run.
        let existing_full = match std::fs::read_to_string(&target_path) {
            Ok(s) => s,
            Err(e) => {
                return error_events(DaemonError::Internal(format!(
                    "read existing wiki page {}: {e}",
                    target_path.display()
                )));
            }
        };
        let existing_body = match memex_core::validate::parse_frontmatter(&existing_full) {
            Ok((_, body)) => body.to_string(),
            Err(_) => existing_full.clone(),
        };

        let merge_pair = crate::daemon::queue::MergePair {
            slug: page.slug.clone(),
            proposed: page.body.clone(),
            existing: existing_body.clone(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job = crate::daemon::queue::BackendJob::Merge(crate::daemon::queue::MergeJob {
            pages: vec![merge_pair],
            reply: tx,
        });
        if state.jobs.submit(job).await.is_err() {
            return error_events(DaemonError::Internal("merge queue closed".into()));
        }
        match rx.await {
            Ok(Ok(reply)) => {
                if let Some(merged) = reply.merged_pages.into_iter().next() {
                    let merged_body = merged.body;
                    let merge_diff = compute_unified_diff(&existing_body, &merged_body);
                    let target_hash = memex_core::storage::content_hash(existing_body.as_bytes());
                    proposals.push(Proposal {
                        index: idx,
                        slug: page.slug.clone(),
                        title: merged.title,
                        tags: merged.tags,
                        body: merged_body,
                        merge_target_slug: Some(page.slug.clone()),
                        merge_target_hash: Some(target_hash),
                        merge_diff: Some(merge_diff),
                        dropped: false,
                        committed: false,
                        original_slug: page.slug,
                        error: None,
                    });
                } else {
                    proposals.push(merge_failure_proposal(idx, &page, "merge returned no pages"));
                }
            }
            Ok(Err(e)) => {
                proposals.push(merge_failure_proposal(
                    idx,
                    &page,
                    &format!("merge worker: {e:?}"),
                ));
            }
            Err(_) => {
                proposals.push(merge_failure_proposal(
                    idx,
                    &page,
                    "merge worker dropped reply",
                ));
            }
        }
    }

    // Drop the lock before stdout streaming (per spec §1.1).
    drop(_hash_guard);

    let plan = Plan {
        version: 1,
        source: PlanSource {
            id: source_id.clone(),
            identifier: source_path.clone(),
            content_hash: content_hash.clone(),
            size_bytes: body.len() as u64,
        },
        created_at: chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        proposals,
    };
    let json = match serde_json::to_string(&plan) {
        Ok(s) => s,
        Err(e) => return error_events(DaemonError::Internal(format!("plan serialize: {e}"))),
    };
    vec![
        Event::PlanContent { json },
        Event::Done { status: 0 },
    ]
}

fn merge_failure_proposal(
    idx: usize,
    page: &crate::daemon::queue::ExtractedPage,
    reason: &str,
) -> Proposal {
    // Per spec §4.5: error populated, hash/diff nullified, slug stays
    // populated as informational, body stays as un-merged EXTRACT output.
    Proposal {
        index: idx,
        slug: page.slug.clone(),
        title: page.title.clone(),
        tags: page.tags.clone(),
        body: page.body.clone(),
        merge_target_slug: Some(page.slug.clone()),
        merge_target_hash: None,
        merge_diff: None,
        dropped: false,
        committed: false,
        original_slug: page.slug.clone(),
        error: Some(format!("merge-dry-run failed: {reason}")),
    }
}

/// Compute a unified diff between two bodies. Pure local op; no LLM.
pub(super) fn compute_unified_diff(old: &str, new: &str) -> String {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff()
        .header("existing", "merged")
        .to_string()
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

    #[test]
    fn compute_unified_diff_shows_added_line() {
        let old = "line1\nline2\n";
        let new = "line1\nline2\nline3\n";
        let d = super::compute_unified_diff(old, new);
        assert!(d.contains("+line3"), "diff: {d}");
    }
}
