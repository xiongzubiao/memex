//! `Request::SourcePlan` and `Request::PlanApply` handlers.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{
    HandlerState, acquire_content_hash_lock, error_events, get_or_open_memex, read_file_capped,
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

    // Read source content from raw store and strip frontmatter. Async +
    // size-capped: source files can be up to INGEST_MAX_BYTES (100 MB);
    // a sync read would stall the tokio worker for the duration.
    let raw_path = memex.root().join(&source_doc.path);
    let raw_body =
        match read_file_capped(&raw_path, crate::daemon::config::INGEST_MAX_BYTES as u64).await {
            Ok(b) => b,
            Err(e) => return error_events(e),
        };
    let (fm, body) = match memex_core::raw::parse_raw_frontmatter(&raw_body) {
        Ok(p) => p,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("raw frontmatter: {e}")));
        }
    };
    let source_path = fm.source.clone().unwrap_or_default();

    let pages =
        match crate::daemon::handler::ingest::extract_pages_from_content(body, &source_path, state)
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
        let existing_full = match tokio::fs::read_to_string(&target_path).await {
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
                    proposals.push(merge_failure_proposal(
                        idx,
                        &page,
                        "merge returned no pages",
                    ));
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

    // The lock guarded the LLM phase (EXTRACT + MERGE-dry-run) only;
    // streaming the plan back is read-only on shared state.
    drop(_hash_guard);

    let plan = Plan {
        version: 1,
        source: PlanSource {
            id: source_id.clone(),
            identifier: source_path.clone(),
            content_hash: content_hash.clone(),
            size_bytes: body.len() as u64,
        },
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        proposals,
    };
    let json = match serde_json::to_string(&plan) {
        Ok(s) => s,
        Err(e) => return error_events(DaemonError::Internal(format!("plan serialize: {e}"))),
    };
    vec![Event::PlanContent { json }, Event::Done { status: 0 }]
}

fn merge_failure_proposal(
    idx: usize,
    page: &crate::daemon::queue::ExtractedPage,
    reason: &str,
) -> Proposal {
    // hash/diff nullified so a future apply can't take the matched-hash
    // commit path with un-merged content; slug stays populated as a
    // breadcrumb for the user; body is the un-merged EXTRACT output.
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
    diff.unified_diff().header("existing", "merged").to_string()
}

/// Handle `Request::PlanApply`. Validates the plan, then per non-dropped
/// non-committed proposal: under per-slug lock, check existence + hash,
/// commit via apply_proposal_to_wiki or mark needs-rereview.
pub(super) async fn handle_plan_apply(plan_json: String, state: &HandlerState) -> Vec<Event> {
    let plan: Plan = match serde_json::from_str(&plan_json) {
        Ok(p) => p,
        Err(e) => {
            return error_events(DaemonError::BadRequest(format!("plan JSON parse: {e}")));
        }
    };
    if let Err(e) = plan.validate() {
        return error_events(DaemonError::BadRequest(format!("plan invalid: {e}")));
    }

    // Verify source still exists.
    let memex = match get_or_open_memex(state.writer.memex_handle(), state.writer.bound_root()) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let docs = match memex.search().resolve_ref_documents(&plan.source.id) {
        Ok(d) => d,
        Err(e) => return error_events(DaemonError::Storage(e.to_string())),
    };
    if !docs.iter().any(|d| d.doc_type == "raw") {
        return error_events(DaemonError::BadRequest(format!(
            "source missing: '{}'",
            plan.source.id
        )));
    }

    let wiki_dir = memex.wiki_dir();
    // Take ownership of proposals so we can mutate per-proposal state.
    let mut plan = plan;
    let mut committed_slugs: Vec<String> = Vec::new();
    let mut any_failed = false;
    let mut any_rereview = false;

    for i in 0..plan.proposals.len() {
        if plan.proposals[i].dropped || plan.proposals[i].committed {
            continue;
        }
        let target = plan.proposals[i].slug.clone();

        // Acquire per-slug writer lock.
        let _slug_guard =
            crate::daemon::handler::acquire_slug_locks(&state.writer, vec![target.clone()]).await;

        let target_path = memex_core::wiki::wiki_path_for_slug(&wiki_dir, &target);
        let exists = target_path.exists();
        let saved_hash = plan.proposals[i].merge_target_hash.clone();
        let saved_target_slug = plan.proposals[i].merge_target_slug.clone();

        if !exists {
            // New page — clear any stale merge fields.
            plan.proposals[i].merge_target_slug = None;
            plan.proposals[i].merge_target_hash = None;
            plan.proposals[i].merge_diff = None;
            // Commit.
            let proposal_clone = plan.proposals[i].clone();
            match apply_proposal_to_wiki(&proposal_clone, &plan.source.id, state).await {
                Ok(()) => {
                    plan.proposals[i].committed = true;
                    plan.proposals[i].error = None;
                    committed_slugs.push(target.clone());
                }
                Err(e) => {
                    plan.proposals[i].error = Some(e.message());
                    any_failed = true;
                }
            }
            drop(_slug_guard);
            continue;
        }

        // Slug exists — staleness check under lock.
        let existing_full = match tokio::fs::read_to_string(&target_path).await {
            Ok(s) => s,
            Err(e) => {
                plan.proposals[i].error = Some(format!("read existing: {e}"));
                any_failed = true;
                drop(_slug_guard);
                continue;
            }
        };
        let existing_body = match memex_core::validate::parse_frontmatter(&existing_full) {
            Ok((_, body)) => body.to_string(),
            Err(_) => existing_full.clone(),
        };
        let existing_hash = memex_core::storage::content_hash(existing_body.as_bytes());

        let hash_matches = saved_hash.as_deref() == Some(existing_hash.as_str());
        let slug_matches = saved_target_slug.as_deref() == Some(target.as_str());

        if hash_matches && slug_matches {
            // Commit the plan body as-is.
            let proposal_clone = plan.proposals[i].clone();
            match apply_proposal_to_wiki(&proposal_clone, &plan.source.id, state).await {
                Ok(()) => {
                    plan.proposals[i].committed = true;
                    plan.proposals[i].error = None;
                    committed_slugs.push(target.clone());
                }
                Err(e) => {
                    plan.proposals[i].error = Some(e.message());
                    any_failed = true;
                }
            }
            drop(_slug_guard);
            continue;
        }

        // Otherwise: stale or new overlap. Drop the slug lock before LLM call.
        drop(_slug_guard);

        // Re-MERGE.
        let merge_pair = crate::daemon::queue::MergePair {
            slug: target.clone(),
            proposed: plan.proposals[i].body.clone(),
            existing: existing_body.clone(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job = crate::daemon::queue::BackendJob::Merge(crate::daemon::queue::MergeJob {
            pages: vec![merge_pair],
            reply: tx,
        });
        if state.jobs.submit(job).await.is_err() {
            plan.proposals[i].error = Some("merge queue closed".into());
            any_failed = true;
            continue;
        }
        match rx.await {
            Ok(Ok(reply)) => {
                if let Some(merged) = reply.merged_pages.into_iter().next() {
                    plan.proposals[i].title = merged.title;
                    plan.proposals[i].tags = merged.tags;
                    plan.proposals[i].body = merged.body.clone();
                    plan.proposals[i].merge_diff =
                        Some(compute_unified_diff(&existing_body, &merged.body));
                    plan.proposals[i].merge_target_slug = Some(target.clone());
                    plan.proposals[i].merge_target_hash = Some(existing_hash.clone());
                    any_rereview = true;
                } else {
                    plan.proposals[i].error = Some("merge returned no pages".into());
                    any_failed = true;
                }
            }
            Ok(Err(e)) => {
                plan.proposals[i].error = Some(format!("merge worker: {e:?}"));
                any_failed = true;
            }
            Err(_) => {
                plan.proposals[i].error = Some("merge worker dropped reply".into());
                any_failed = true;
            }
        }
    }

    // Decide outcome (re-review takes precedence over partial-failure).
    if any_rereview {
        let json = match serde_json::to_string(&plan) {
            Ok(s) => s,
            Err(e) => return error_events(DaemonError::Internal(format!("serialize: {e}"))),
        };
        return vec![Event::PlanContent { json }, Event::Done { status: 3 }];
    }
    if any_failed {
        let json = match serde_json::to_string(&plan) {
            Ok(s) => s,
            Err(e) => return error_events(DaemonError::Internal(format!("serialize: {e}"))),
        };
        return vec![Event::PlanContent { json }, Event::Done { status: 4 }];
    }
    vec![
        Event::PlanApplied {
            committed: committed_slugs,
        },
        Event::Done { status: 0 },
    ]
}

/// Write a proposal to the wiki using the merge-aware path. New pages
/// get fresh frontmatter; merges preserve `created_at` and accumulate
/// `sources:`. Caller holds the per-slug write lock around this call.
pub(super) async fn apply_proposal_to_wiki(
    proposal: &Proposal,
    source_docid: &str,
    state: &HandlerState,
) -> Result<(), DaemonError> {
    let memex = get_or_open_memex(state.writer.memex_handle(), state.writer.bound_root())?;
    let wiki_path = memex_core::wiki::wiki_path_for_slug(&memex.wiki_dir(), &proposal.slug);

    let now_dt = chrono::Utc::now();
    let new_ref = format!("#{source_docid}");

    let (created_at, sources) = if wiki_path.exists() {
        // Reuse the canonical parser. If parsing fails (malformed page
        // on disk), fall back to fresh values rather than aborting.
        let existing = tokio::fs::read_to_string(&wiki_path)
            .await
            .map_err(|e| DaemonError::Internal(format!("read existing wiki: {e}")))?;
        match memex_core::validate::parse_frontmatter(&existing) {
            Ok((fm, _)) => {
                let mut sources = fm.sources;
                if !sources.contains(&new_ref) {
                    sources.push(new_ref);
                }
                (fm.created_at, sources)
            }
            Err(_) => (now_dt, vec![new_ref]),
        }
    } else {
        if let Some(parent) = wiki_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| DaemonError::Internal(format!("create wiki dir: {e}")))?;
        }
        (now_dt, vec![new_ref])
    };

    // Title comes from the LLM (or a user-edited plan); collapse newlines
    // to spaces so the YAML stays a single-line scalar — matches the
    // sanitization in `build_wiki_records`. serde_yaml escapes any
    // remaining special chars, so YAML injection via title/tags isn't
    // possible regardless of source content.
    let safe_title = proposal.title.replace(['\n', '\r'], " ");
    let yaml = serde_yaml::to_string(&memex_core::types::PageFrontmatterRef {
        title: &safe_title,
        summary: None,
        tags: &proposal.tags,
        collections: &[],
        created_at,
        updated_at: now_dt,
        sources: &sources,
    })
    .map_err(|e| DaemonError::Internal(format!("frontmatter serialize: {e}")))?;
    let file = format!("---\n{yaml}---\n\n{}", proposal.body);
    crate::daemon::handler::async_atomic_write(wiki_path.clone(), file.into_bytes()).await?;

    // Index after write so chunks/embeddings stay consistent.
    {
        let mut guard = state.writer.embed_model().lock().await;
        memex_core::index_wiki::index_wiki_file(&memex, &wiki_path, Some(guard.as_mut()))
            .map_err(|e| DaemonError::Internal(format!("index_wiki_file: {e}")))?;
    }
    Ok(())
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

    fn seed_existing_page(root: &std::path::Path, slug: &str, body: &str, sources: &[&str]) {
        let wiki_dir = root.join("wiki");
        std::fs::create_dir_all(&wiki_dir).unwrap();
        let yaml_sources = if sources.is_empty() {
            "[]".into()
        } else {
            format!(
                "\n  - {}",
                sources
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join("\n  - ")
            )
        };
        let frontmatter = format!(
            "---\ntitle: Existing\ntags: []\ncreated_at: 2024-01-01T00:00:00Z\nupdated_at: 2024-01-01T00:00:00Z\nsources: {yaml_sources}\n---\n\n{body}"
        );
        let path = wiki_dir.join(format!("{slug}.md"));
        std::fs::write(path, frontmatter).unwrap();
    }

    #[tokio::test]
    async fn apply_proposal_new_page_synthesizes_fresh_frontmatter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        let state = test_state(root.clone());
        let proposal = Proposal {
            index: 0,
            slug: "new-page".into(),
            title: "New Page".into(),
            tags: vec!["t1".into()],
            body: "fresh body".into(),
            merge_target_slug: None,
            merge_target_hash: None,
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: "new-page".into(),
            error: None,
        };
        super::apply_proposal_to_wiki(&proposal, "src-test", &state)
            .await
            .unwrap();
        let body = std::fs::read_to_string(root.join("wiki/new-page.md")).unwrap();
        let (fm, parsed_body) = memex_core::validate::parse_frontmatter(&body).unwrap();
        assert_eq!(fm.title, "New Page");
        assert_eq!(fm.tags, vec!["t1".to_string()]);
        assert_eq!(fm.sources, vec!["#src-test".to_string()]);
        assert!(parsed_body.contains("fresh body"), "got: {parsed_body}");
    }

    #[tokio::test]
    async fn apply_proposal_merge_preserves_created_at_and_appends_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        seed_existing_page(&root, "mmai", "old body", &["#src-old"]);
        let state = test_state(root.clone());
        let proposal = Proposal {
            index: 0,
            slug: "mmai".into(),
            title: "MMAI".into(),
            tags: vec![],
            body: "merged body".into(),
            merge_target_slug: Some("mmai".into()),
            merge_target_hash: Some(memex_core::storage::content_hash("old body".as_bytes())),
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: "mmai".into(),
            error: None,
        };
        super::apply_proposal_to_wiki(&proposal, "src-new", &state)
            .await
            .unwrap();
        let body = std::fs::read_to_string(root.join("wiki/mmai.md")).unwrap();
        let (fm, parsed_body) = memex_core::validate::parse_frontmatter(&body).unwrap();
        assert_eq!(
            fm.created_at.format("%Y-%m-%d").to_string(),
            "2024-01-01",
            "created_at was not preserved"
        );
        assert_eq!(
            fm.sources,
            vec!["#src-old".to_string(), "#src-new".to_string()]
        );
        assert!(parsed_body.contains("merged body"));
    }

    #[tokio::test]
    async fn apply_proposal_sanitizes_newline_title_no_yaml_injection() {
        // A title with a newline must NOT inject extra YAML keys: it's
        // collapsed to a space (matching `build_wiki_records`) and then
        // serialized through serde_yaml, which escapes anything else.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        let state = test_state(root.clone());
        let proposal = Proposal {
            index: 0,
            slug: "no-injection".into(),
            title: "Hello\nsources:\n  - \"#fake\"\nbogus: x".into(),
            tags: vec![],
            body: "body".into(),
            merge_target_slug: None,
            merge_target_hash: None,
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: "no-injection".into(),
            error: None,
        };
        super::apply_proposal_to_wiki(&proposal, "src-x", &state)
            .await
            .unwrap();
        let body = std::fs::read_to_string(root.join("wiki/no-injection.md")).unwrap();
        let (fm, _) = memex_core::validate::parse_frontmatter(&body).unwrap();
        // Sources must NOT include the injected ref — it's safely
        // captured as part of the title's quoted scalar instead.
        assert_eq!(fm.sources, vec!["#src-x".to_string()]);
        assert!(
            !fm.title.contains('\n'),
            "title still has newline: {:?}",
            fm.title
        );
        // The malicious payload should be IN the title (escaped), proving
        // the round-trip is structurally safe.
        assert!(
            fm.title.contains("#fake") && fm.title.contains("bogus"),
            "title should contain the literal injection attempt: {:?}",
            fm.title
        );
    }

    #[tokio::test]
    async fn apply_proposal_serializes_tag_with_special_chars() {
        // A tag with embedded YAML-special chars must serialize back to a
        // single tag (escaped), not inject new fields.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        let state = test_state(root.clone());
        let evil_tag = "evil\nbogus: x";
        let proposal = Proposal {
            index: 0,
            slug: "tag-test".into(),
            title: "OK".into(),
            tags: vec!["clean".into(), evil_tag.into()],
            body: "body".into(),
            merge_target_slug: None,
            merge_target_hash: None,
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: "tag-test".into(),
            error: None,
        };
        super::apply_proposal_to_wiki(&proposal, "src-x", &state)
            .await
            .unwrap();
        let body = std::fs::read_to_string(root.join("wiki/tag-test.md")).unwrap();
        let (fm, _) = memex_core::validate::parse_frontmatter(&body).unwrap();
        assert_eq!(fm.tags.len(), 2, "tags expanded from injection: {:?}", fm.tags);
        assert_eq!(fm.tags[0], "clean");
        assert_eq!(fm.tags[1], evil_tag);
    }

    #[tokio::test]
    async fn plan_apply_rejects_invalid_version() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        let state = test_state(root);
        let bad = r#"{"version":99,"source":{"id":"s","identifier":"i","content_hash":"a","size_bytes":0},"created_at":"x","proposals":[]}"#;
        let events = handle_plan_apply(bad.into(), &state).await;
        assert!(matches!(&events[0], Event::Error { code, .. } if code == "bad_request"));
    }

    #[tokio::test]
    async fn plan_apply_rejects_malformed_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        let state = test_state(root);
        let events = handle_plan_apply("not json".into(), &state).await;
        assert!(matches!(&events[0], Event::Error { code, .. } if code == "bad_request"));
    }

    #[tokio::test]
    async fn plan_apply_rejects_missing_source() {
        // Plan validates structurally but references a docid not in the
        // raw store → bad_request "source missing".
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        let state = test_state(root);
        let plan = Plan {
            version: 1,
            source: PlanSource {
                id: "src-doesnotexist".into(),
                identifier: "x".into(),
                content_hash: "a".repeat(64),
                size_bytes: 0,
            },
            created_at: "2026-04-30T00:00:00Z".into(),
            proposals: vec![],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let events = handle_plan_apply(json, &state).await;
        let last = events.last().unwrap();
        assert!(
            matches!(last, Event::Done { status: 1 }),
            "expected Done{{1}}, got: {events:?}"
        );
    }
}
