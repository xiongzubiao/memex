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

/// Write a proposal to the wiki using the merge-aware path. New pages
/// get fresh frontmatter; merges preserve `created_at` and accumulate
/// `sources:`. Caller holds the per-slug write lock around this call
/// (see spec §1.3 step 4).
pub(super) async fn apply_proposal_to_wiki(
    proposal: &Proposal,
    source_docid: &str,
    state: &HandlerState,
) -> Result<(), DaemonError> {
    let memex = get_or_open_memex(state.writer.memex_handle(), state.writer.bound_root())?;
    let wiki_path = memex_core::wiki::wiki_path_for_slug(&memex.wiki_dir(), &proposal.slug);

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let yaml_tags = if proposal.tags.is_empty() {
        "[]".to_string()
    } else {
        format!("\n  - {}", proposal.tags.join("\n  - "))
    };

    let (created_at, sources) = if wiki_path.exists() {
        // Merge case: parse existing frontmatter for created_at +
        // sources accumulation.
        let existing = std::fs::read_to_string(&wiki_path)
            .map_err(|e| DaemonError::Internal(format!("read existing wiki: {e}")))?;
        let fm_str = existing
            .strip_prefix("---\n")
            .and_then(|s| s.split_once("\n---\n"))
            .map(|(fm, _)| fm)
            .unwrap_or("");
        let created_at = parse_created_at(fm_str).unwrap_or_else(|| now.clone());
        let mut sources = parse_sources_array(fm_str);
        let new_ref = format!("#{source_docid}");
        if !sources.contains(&new_ref) {
            sources.push(new_ref);
        }
        (created_at, sources)
    } else {
        if let Some(parent) = wiki_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| DaemonError::Internal(format!("create wiki dir: {e}")))?;
        }
        (now.clone(), vec![format!("#{source_docid}")])
    };

    let yaml_sources = if sources.is_empty() {
        "[]".to_string()
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
        "title: {}\ntags: {}\ncreated_at: {}\nupdated_at: {}\nsources: {}\n",
        proposal.title, yaml_tags, created_at, now, yaml_sources
    );
    let file = format!("---\n{frontmatter}---\n\n{}", proposal.body);
    crate::daemon::handler::async_atomic_write(wiki_path.clone(), file.into_bytes()).await?;

    // Index after write so chunks/embeddings stay consistent.
    {
        let mut guard = state.writer.embed_model().lock().await;
        memex_core::index_wiki::index_wiki_file(&memex, &wiki_path, Some(guard.as_mut()))
            .map_err(|e| DaemonError::Internal(format!("index_wiki_file: {e}")))?;
    }
    Ok(())
}

/// Extract `created_at: <RFC3339>` from a YAML frontmatter slice.
/// Lenient — returns None if missing/malformed.
fn parse_created_at(fm: &str) -> Option<String> {
    for line in fm.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("created_at:") {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Extract the `sources:` YAML array as a Vec<String>. Tolerant of
/// inline ([]) and block (\n  - "x") forms.
fn parse_sources_array(fm: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_sources = false;
    for line in fm.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("sources:") {
            let rest = rest.trim();
            if rest == "[]" {
                return Vec::new();
            }
            if rest.starts_with('[') && rest.ends_with(']') {
                let inner = &rest[1..rest.len() - 1];
                for token in inner.split(',') {
                    let s = token.trim().trim_matches('"');
                    if !s.is_empty() {
                        out.push(s.to_string());
                    }
                }
                return out;
            }
            in_sources = true;
            continue;
        }
        if in_sources {
            if let Some(rest) = trimmed.strip_prefix("- ") {
                let s = rest.trim().trim_matches('"');
                out.push(s.to_string());
            } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                break;
            }
        }
    }
    out
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
                sources.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join("\n  - ")
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
        assert!(body.contains("title: New Page"));
        assert!(body.contains("\"#src-test\""));
        assert!(body.contains("fresh body"));
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
        assert!(body.contains("created_at: 2024-01-01"), "got: {body}");
        assert!(body.contains("\"#src-old\""), "old source dropped: {body}");
        assert!(body.contains("\"#src-new\""), "new source missing: {body}");
        assert!(body.contains("merged body"));
    }
}
