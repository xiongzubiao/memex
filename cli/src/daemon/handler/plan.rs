//! `Request::SourcePlan` and `Request::PlanApply` handlers.

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{
    HandlerState, acquire_content_hash_lock, error_events, get_or_open_memex, read_file_capped,
    run_worker_job,
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

    let source_doc =
        match crate::daemon::handler::source::resolve_source_ref(memex.search(), &source_id) {
            Ok(Some(d)) => d,
            Ok(None) => {
                return error_events(DaemonError::BadRequest(format!(
                    "source not found: '{source_id}'. Run `memex source list` to find docids."
                )));
            }
            Err(e) => return error_events(DaemonError::Storage(e.to_string())),
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

    // Split pages by overlap with existing wiki state. New-page proposals
    // are added directly; overlap proposals get batched into a single
    // MERGE-dry-run round-trip (matches `extract_pages_from_content`'s
    // wiki-merge phase at ingest.rs:707).
    let wiki_dir = memex.wiki_dir();
    let mut proposals: Vec<Proposal> = Vec::new();
    let mut overlap_inputs: Vec<(usize, crate::daemon::queue::ExtractedPage, String)> = Vec::new();
    for (idx, page) in pages.into_iter().enumerate() {
        let target_path = memex_core::wiki::wiki_path_for_slug(&wiki_dir, &page.slug);
        if !target_path.exists() {
            proposals.push(Proposal::new_for_page(idx, page));
            continue;
        }
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
        overlap_inputs.push((idx, page, existing_body));
    }

    if !overlap_inputs.is_empty() {
        // Per-slug fan-out: each merge dry-run is its own LLM call so a
        // slow or failing merge on one slug doesn't block or fail the
        // others. Futures share borrowed state and are awaited in scope.
        let merge_futs = overlap_inputs.iter().map(|(_, page, existing)| {
            let pair = crate::daemon::queue::MergePair {
                slug: page.slug.clone(),
                proposed: page.body.clone(),
                existing: existing.clone(),
            };
            run_worker_job(state, move |reply| {
                crate::daemon::queue::BackendJob::Merge(crate::daemon::queue::MergeJob {
                    pages: vec![pair],
                    reply,
                })
            })
        });
        let merge_results = futures::future::join_all(merge_futs).await;
        for ((idx, page, existing_body), result) in
            overlap_inputs.into_iter().zip(merge_results)
        {
            match result {
                Ok(reply) => {
                    match reply.merged_pages.into_iter().find(|p| p.slug == page.slug) {
                        Some(merged) => {
                            let target_hash =
                                memex_core::storage::content_hash(existing_body.as_bytes());
                            let merge_diff = compute_unified_diff(&existing_body, &merged.body);
                            let mut p = Proposal::new_for_page(idx, page);
                            p.title = merged.title;
                            p.body = merged.body;
                            p.merge_target_slug = Some(p.slug.clone());
                            p.merge_target_hash = Some(target_hash);
                            p.merge_diff = Some(merge_diff);
                            proposals.push(p);
                        }
                        None => proposals.push(merge_failure_proposal(
                            idx,
                            page,
                            "merge omitted slug",
                        )),
                    }
                }
                Err(e) => {
                    proposals.push(merge_failure_proposal(idx, page, &e.message()));
                }
            }
        }
        // Overlap proposals were appended out of order — restore by index.
        proposals.sort_by_key(|p| p.index);
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
    page: crate::daemon::queue::ExtractedPage,
    reason: &str,
) -> Proposal {
    // hash/diff nullified so a future apply can't take the matched-hash
    // commit path with un-merged content; slug stays populated as a
    // breadcrumb for the user; body is the un-merged EXTRACT output.
    let mut p = Proposal::new_for_page(idx, page);
    p.merge_target_slug = Some(p.slug.clone());
    p.error = Some(format!("merge-dry-run failed: {reason}"));
    p
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
    match crate::daemon::handler::source::resolve_source_ref(memex.search(), &plan.source.id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return error_events(DaemonError::BadRequest(format!(
                "source missing: '{}'",
                plan.source.id
            )));
        }
        Err(e) => return error_events(DaemonError::Storage(e.to_string())),
    }

    let wiki_dir = memex.wiki_dir();
    let mut plan = plan;
    let mut committed_slugs: Vec<String> = Vec::new();
    let mut any_failed = false;
    let mut any_rereview = false;
    // Phase 1: under each per-slug lock, decide commit / skip / needs-rereview.
    // Stale ones go to phase 2 for one batched re-MERGE.
    let mut rereview_inputs: Vec<(usize, String, String)> = Vec::new();
    for i in 0..plan.proposals.len() {
        if plan.proposals[i].dropped || plan.proposals[i].committed {
            continue;
        }
        let target = plan.proposals[i].slug.clone();
        let _slug_guard =
            crate::daemon::handler::acquire_slug_lock(&state.writer, &target).await;

        let target_path = memex_core::wiki::wiki_path_for_slug(&wiki_dir, &target);
        if !target_path.exists() {
            plan.proposals[i].merge_target_slug = None;
            plan.proposals[i].merge_target_hash = None;
            plan.proposals[i].merge_diff = None;
            let proposal_clone = plan.proposals[i].clone();
            match apply_proposal_to_wiki(&proposal_clone, &plan.source.id, PriorPage::New, state)
                .await
            {
                Ok(()) => {
                    plan.proposals[i].committed = true;
                    plan.proposals[i].error = None;
                    committed_slugs.push(target);
                }
                Err(e) => {
                    plan.proposals[i].error = Some(e.message());
                    any_failed = true;
                }
            }
            continue;
        }

        let existing_full = match tokio::fs::read_to_string(&target_path).await {
            Ok(s) => s,
            Err(e) => {
                plan.proposals[i].error = Some(format!("read existing: {e}"));
                any_failed = true;
                continue;
            }
        };
        // Parse once; reuse for hash compare AND (on match) for the
        // commit's prior values. Err-fallback uses full content as body
        // and now()/[] for prior — same shape as `source plan` so hashes
        // align in the malformed-frontmatter corner case.
        let (parsed_fm, existing_body) =
            match memex_core::validate::parse_frontmatter(&existing_full) {
                Ok((fm, body)) => (Some(fm), body),
                Err(_) => (None, existing_full.clone()),
            };
        let existing_hash = memex_core::storage::content_hash(existing_body.as_bytes());

        let hash_matches = plan.proposals[i].merge_target_hash.as_deref()
            == Some(existing_hash.as_str());
        let slug_matches = plan.proposals[i].merge_target_slug.as_deref() == Some(target.as_str());

        if hash_matches && slug_matches {
            let proposal_clone = plan.proposals[i].clone();
            let prior = match parsed_fm {
                Some(fm) => PriorPage::Existing {
                    created_at: fm.created_at,
                    sources: fm.sources,
                },
                None => PriorPage::New,
            };
            match apply_proposal_to_wiki(&proposal_clone, &plan.source.id, prior, state).await {
                Ok(()) => {
                    plan.proposals[i].committed = true;
                    plan.proposals[i].error = None;
                    committed_slugs.push(target);
                }
                Err(e) => {
                    plan.proposals[i].error = Some(e.message());
                    any_failed = true;
                }
            }
            continue;
        }

        // Stale → batched re-MERGE in phase 2 (after the slug lock drops).
        rereview_inputs.push((i, existing_body, existing_hash));
    }

    // Phase 2: per-slug MERGE-dry-run for all stale proposals. Each
    // re-merge is its own LLM call so a slow or failing slug doesn't
    // block or fail the others.
    if !rereview_inputs.is_empty() {
        let merge_futs = rereview_inputs.iter().map(|(i, existing, _)| {
            let pair = crate::daemon::queue::MergePair {
                slug: plan.proposals[*i].slug.clone(),
                proposed: plan.proposals[*i].body.clone(),
                existing: existing.clone(),
            };
            run_worker_job(state, move |reply| {
                crate::daemon::queue::BackendJob::Merge(crate::daemon::queue::MergeJob {
                    pages: vec![pair],
                    reply,
                })
            })
        });
        let merge_results = futures::future::join_all(merge_futs).await;
        for ((i, existing_body, existing_hash), result) in
            rereview_inputs.into_iter().zip(merge_results)
        {
            let target = plan.proposals[i].slug.clone();
            match result {
                Ok(reply) => {
                    match reply.merged_pages.into_iter().find(|p| p.slug == target) {
                        Some(merged) => {
                            plan.proposals[i].title = merged.title;
                            plan.proposals[i].merge_diff =
                                Some(compute_unified_diff(&existing_body, &merged.body));
                            plan.proposals[i].body = merged.body;
                            plan.proposals[i].merge_target_slug = Some(target);
                            plan.proposals[i].merge_target_hash = Some(existing_hash);
                            any_rereview = true;
                        }
                        None => {
                            plan.proposals[i].error = Some("merge omitted slug".into());
                            any_failed = true;
                        }
                    }
                }
                Err(e) => {
                    plan.proposals[i].error = Some(e.message());
                    any_failed = true;
                }
            }
        }
    }

    // Decide outcome (re-review takes precedence over partial-failure).
    if any_rereview {
        let json = match serde_json::to_string(&plan) {
            Ok(s) => s,
            Err(e) => return error_events(DaemonError::Internal(format!("serialize: {e}"))),
        };
        return vec![Event::PlanContent { json }, Event::Done { status: crate::daemon::plan::APPLY_NEEDS_REREVIEW }];
    }
    if any_failed {
        let json = match serde_json::to_string(&plan) {
            Ok(s) => s,
            Err(e) => return error_events(DaemonError::Internal(format!("serialize: {e}"))),
        };
        return vec![Event::PlanContent { json }, Event::Done { status: crate::daemon::plan::APPLY_PARTIAL_FAILURE }];
    }
    vec![
        Event::PlanApplied {
            committed: committed_slugs,
        },
        Event::Done { status: crate::daemon::plan::APPLY_OK },
    ]
}

/// Caller's pre-loaded knowledge about the wiki page that's about to be
/// written. `New` triggers parent-dir creation and a fresh
/// `created_at`/`sources` list; `Existing` reuses the caller's already-parsed
/// values to avoid a second read+parse.
pub(super) enum PriorPage {
    New,
    Existing {
        created_at: chrono::DateTime<chrono::Utc>,
        sources: Vec<String>,
    },
}

/// Write a proposal to the wiki. Caller holds the per-slug write lock.
pub(super) async fn apply_proposal_to_wiki(
    proposal: &Proposal,
    source_docid: &str,
    prior: PriorPage,
    state: &HandlerState,
) -> Result<(), DaemonError> {
    let memex = get_or_open_memex(state.writer.memex_handle(), state.writer.bound_root())?;
    let wiki_path = memex_core::wiki::wiki_path_for_slug(&memex.wiki_dir(), &proposal.slug);

    let now_dt = chrono::Utc::now();
    let new_ref = format!("#{source_docid}");

    let (created_at, sources) = match prior {
        PriorPage::New => {
            if let Some(parent) = wiki_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| DaemonError::Internal(format!("create wiki dir: {e}")))?;
            }
            (now_dt, vec![new_ref])
        }
        PriorPage::Existing {
            created_at,
            mut sources,
        } => {
            if !sources.contains(&new_ref) {
                sources.push(new_ref);
            }
            (created_at, sources)
        }
    };

    let file = memex_core::wiki::compose_wiki_markdown(
        &proposal.title,
        &proposal.body,
        created_at,
        &sources,
        &[],
        now_dt,
    )
    .map_err(|e| DaemonError::Internal(format!("frontmatter serialize: {e}")))?;
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
            "---\ntitle: Existing
created_at: 2024-01-01T00:00:00Z\nupdated_at: 2024-01-01T00:00:00Z\nsources: {yaml_sources}\n---\n\n{body}"
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
            body: "fresh body".into(),
            merge_target_slug: None,
            merge_target_hash: None,
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: "new-page".into(),
            error: None,
        };
        super::apply_proposal_to_wiki(&proposal, "src-test", super::PriorPage::New, &state)
            .await
            .unwrap();
        let body = std::fs::read_to_string(root.join("wiki/new-page.md")).unwrap();
        let (fm, parsed_body) = memex_core::validate::parse_frontmatter(&body).unwrap();
        assert_eq!(fm.title, "New Page");
        assert_eq!(fm.sources, vec!["#src-test".to_string()]);
        assert!(parsed_body.contains("fresh body"), "got: {parsed_body}");
    }

    #[tokio::test]
    async fn apply_proposal_merge_preserves_created_at_and_appends_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let _ = memex_core::Memex::open(root.clone()).unwrap();
        seed_existing_page(&root, "alpha", "old body", &["#src-old"]);
        let state = test_state(root.clone());
        let proposal = Proposal {
            index: 0,
            slug: "alpha".into(),
            title: "Alpha".into(),
            body: "merged body".into(),
            merge_target_slug: Some("alpha".into()),
            merge_target_hash: Some(memex_core::storage::content_hash("old body".as_bytes())),
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: "alpha".into(),
            error: None,
        };
        let prior = super::PriorPage::Existing {
            created_at: "2024-01-01T00:00:00Z".parse().unwrap(),
            sources: vec!["#src-old".into()],
        };
        super::apply_proposal_to_wiki(&proposal, "src-new", prior, &state)
            .await
            .unwrap();
        let body = std::fs::read_to_string(root.join("wiki/alpha.md")).unwrap();
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
            body: "body".into(),
            merge_target_slug: None,
            merge_target_hash: None,
            merge_diff: None,
            dropped: false,
            committed: false,
            original_slug: "no-injection".into(),
            error: None,
        };
        super::apply_proposal_to_wiki(&proposal, "src-x", super::PriorPage::New, &state)
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
