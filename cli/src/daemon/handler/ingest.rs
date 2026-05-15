//! `Request::Ingest` handlers (transcript + document) plus the shared
//! post-Extract pipeline (`store_extracted_pages`) and its support
//! functions: dedup, fragment-merge, embed-and-mark, atomic file writes.

use std::path::Path;

use crate::daemon::error::DaemonError;
use crate::daemon::handler::source::derive_source_title;
use crate::daemon::handler::{
    HandlerState, acquire_slug_lock, async_atomic_write, error_events, get_or_open_memex,
    read_file_capped, run_worker_job, validate_and_redact_inbound_content, validate_source_path,
};
use crate::daemon::protocol::Event;

/// Path-based transcript ingest: read the file, dispatch to the shared body.
pub(super) async fn handle_ingest_transcript(
    transcript_path: String,
    agent: crate::daemon::protocol::TranscriptAgent,
    collections: Vec<String>,
    state: &HandlerState,
) -> Vec<Event> {
    use std::path::PathBuf;

    let path = PathBuf::from(&transcript_path);
    if !path.is_absolute() {
        return error_events(DaemonError::BadRequest(
            "transcript_path must be absolute".into(),
        ));
    }
    let raw_content =
        match read_file_capped(&path, crate::daemon::config::INGEST_MAX_BYTES as u64).await {
            Ok(c) => c,
            Err(e) => return error_events(e),
        };
    handle_ingest_transcript_content(raw_content, transcript_path, agent, collections, state).await
}

/// Shared body: parse content for `agent`, run dedup → Extract → Merge → store.
/// `source_label` is used purely as a label (source field, logs, events) —
/// no file IO happens inside.
pub(super) async fn handle_ingest_transcript_content(
    raw_content: String,
    source_label: String,
    agent: crate::daemon::protocol::TranscriptAgent,
    collections: Vec<String>,
    state: &HandlerState,
) -> Vec<Event> {
    use crate::daemon::protocol::TranscriptAgent;
    use crate::daemon::queue::{BackendJob, IngestJob};
    use memex_core::transcript::SessionFilter;

    let t0 = std::time::Instant::now();

    if let Err(e) = memex_core::search::validate_collection_names(&collections) {
        return error_events(DaemonError::BadRequest(e));
    }

    // Re-opening memex per-request races on schema-init DDL under concurrent ingest.
    let root_path = state.writer.bound_root().to_path_buf();
    let memex = match get_or_open_memex(state.writer.memex_handle(), &root_path) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();
    let effective_collections = memex_core::search::normalize_collections(&collections);

    let transcript = match agent {
        TranscriptAgent::ClaudeCode => memex_core::transcript::parse_claude_code_session(
            std::io::BufReader::new(raw_content.as_bytes()),
        ),
        TranscriptAgent::Codex => memex_core::transcript::parse_codex_session(
            std::io::BufReader::new(raw_content.as_bytes()),
        ),
        TranscriptAgent::GeminiCli => {
            memex_core::transcript::parse_gemini_cli_session(&raw_content)
        }
        TranscriptAgent::OpenClaw => memex_core::transcript::parse_openclaw_session(&raw_content),
        TranscriptAgent::Hermes => memex_core::transcript::parse_hermes_session(&raw_content),
        TranscriptAgent::OpenCode => memex_core::transcript::parse_opencode_session(
            std::io::BufReader::new(raw_content.as_bytes()),
        ),
    };
    let agent_str = agent.as_str();

    let transcript = match transcript {
        Ok(t) => t,
        Err(e) => {
            return error_events(DaemonError::BadRequest(format!("parse error: {e}")));
        }
    };

    match transcript.filter {
        SessionFilter::Pass => {} // continue
        SessionFilter::NonSubstantive | SessionFilter::InternalSession => {
            return vec![Event::Done { status: 0 }]; // skip silently
        }
    }

    // Structured turns are the sole transcript source of truth.
    if transcript.turns.is_empty() {
        return vec![Event::Done { status: 0 }];
    }
    let canonical_transcript = memex_core::transcript::render_turns(&transcript.turns);

    // Content-addressed job_id: a renamed/relocated transcript with identical
    // content dedupes against itself rather than re-ingesting.
    let content_hash = memex_core::storage::content_hash(canonical_transcript.as_bytes());
    let job_id = format!("ingest-{}", &content_hash[..16]);

    if memex_core::raw::raw_path_for_hash(&memex.raw_dir(), &content_hash).exists() {
        tracing::info!(hash=%content_hash, "transcript ingest deduped: raw file present");
        return vec![Event::Done { status: 0 }];
    }

    // Persist before LLM dispatch so a crash doesn't lose the job;
    // INSERT OR IGNORE on the content-derived job_id dedupes concurrent ingests.
    if let Err(e) = search.insert_ingest_job(
        &job_id,
        memex_core::ingest_jobs::JobType::Transcript,
        &source_label,
        Some(agent_str),
        &content_hash,
        &effective_collections,
    ) {
        return error_events(DaemonError::Internal(format!(
            "insert_ingest_job failed: {e}"
        )));
    }

    let mut events = vec![Event::Parsing {
        job_id: job_id.clone(),
        transcript_path: source_label.clone(),
    }];

    events.push(Event::Distilling {
        job_id: job_id.clone(),
        transcript_path: source_label.clone(),
    });

    let segments: Vec<crate::daemon::queue::ExtractSegment> = transcript
        .turns
        .into_iter()
        .enumerate()
        .map(|(i, t)| crate::daemon::queue::ExtractSegment {
            index: Some(i + 1),
            role: Some(t.role),
            timestamp: t.timestamp,
            text: t.text,
        })
        .collect();

    let extracted = match run_worker_job(state, |reply| {
        BackendJob::Ingest(IngestJob {
            segments,
            source: source_label.clone(),
            chunk: None,
            reply,
        })
    })
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some(&e.to_string()));
            return error_events(e);
        }
    };

    if extracted.pages.is_empty() {
        let _ = search.update_ingest_job_status(&job_id, "completed", None);
        events.push(Event::Done { status: 0 });
        return events;
    }

    let title = if transcript.session_id.is_empty() {
        agent_str.to_string()
    } else {
        format!("{agent_str} {}", transcript.session_id)
    };
    let fm = memex_core::raw::RawFrontmatter {
        source: Some(source_label.clone()),
        source_kind: Some("transcript".into()),
        ingested_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        converter: None,
        title: Some(title.clone()),
    };

    let stored_event = match store_extracted_pages(
        extracted.pages,
        &canonical_transcript,
        &fm,
        &effective_collections,
        &job_id,
        state,
    )
    .await
    {
        Ok(e) => e,
        Err(err_events) => return err_events,
    };

    let _ = search.update_ingest_job_status(&job_id, "completed", None);

    tracing::info!(
        job_id = %job_id,
        transcript = %source_label,
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "ingest job completed"
    );

    events.push(stored_event);
    events.push(Event::Done { status: 0 });
    events
}

/// Handle a document-ingest request. Validates content, hashes it for
/// dedup, chunks long documents at H1/H2 boundaries, dispatches one Extract
/// per chunk concurrently, fragment-merges pages that share a slug across
/// chunks, then hands off to the shared post-Extract pipeline.
pub(super) async fn handle_ingest_document(
    source_path: String,
    content: String,
    collections: Vec<String>,
    state: &HandlerState,
) -> Vec<Event> {
    let cfg = state.config.ingest.clone();

    // ─── Validation ───────────────────────────────────────────────────
    if let Err(e) = validate_source_path(&source_path) {
        return error_events(DaemonError::BadRequest(e));
    }
    if let Err(e) = memex_core::search::validate_collection_names(&collections) {
        return error_events(DaemonError::BadRequest(e));
    }
    let redacted = match validate_and_redact_inbound_content(&content, cfg.fetch_max_bytes) {
        Ok(r) => r,
        Err(e) => return error_events(e),
    };

    // ─── Open memex + dedup on content hash ───────────────────────────
    let root_path = state.writer.bound_root().to_path_buf();
    let memex = match get_or_open_memex(state.writer.memex_handle(), &root_path) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();
    let effective_collections = memex_core::search::normalize_collections(&collections);

    let content_hash = memex_core::storage::content_hash(redacted.as_bytes());

    // ─── Dedup: skip if the content-addressed raw file is already on disk ──
    if memex_core::raw::raw_path_for_hash(&memex.raw_dir(), &content_hash).exists() {
        tracing::info!(hash=%content_hash, "ingest deduped: raw file present");
        return vec![Event::Done { status: 0 }];
    }

    // ─── Job ID + persist job row (pre-Extract) ────
    let job_id = format!(
        "doc-{}",
        memex_core::storage::content_hash(format!("{source_path}\0{content_hash}").as_bytes())
    );
    if let Err(e) = search.insert_ingest_job(
        &job_id,
        memex_core::ingest_jobs::JobType::Document,
        &source_path,
        None,
        &content_hash,
        &effective_collections,
    ) {
        return error_events(DaemonError::Internal(format!("insert_ingest_job: {e}")));
    }

    let mut events = vec![
        Event::Parsing {
            job_id: job_id.clone(),
            transcript_path: source_path.clone(),
        },
        Event::Distilling {
            job_id: job_id.clone(),
            transcript_path: source_path.clone(),
        },
    ];

    // ─── Chunked EXTRACT + cross-chunk MERGE consolidation ───────────
    let merged_pages = match extract_pages_from_content(&redacted, &source_path, state).await {
        Ok(p) => p,
        Err(e) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some(&e.message()));
            return error_events(e);
        }
    };

    if merged_pages.is_empty() {
        let _ = search.update_ingest_job_status(&job_id, "completed", None);
        events.push(Event::Done { status: 0 });
        return events;
    }

    // ─── Hand off to shared post-Extract pipeline ─────────────────────
    let source_title = derive_source_title(&redacted, &source_path);
    let kind = if memex_core::raw::is_url(&source_path) {
        "url"
    } else {
        "path"
    };
    let fm = memex_core::raw::RawFrontmatter {
        source: Some(source_path.clone()),
        source_kind: Some(kind.into()),
        ingested_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        converter: None,
        title: Some(source_title.clone()),
    };
    let stored_event = match store_extracted_pages(
        merged_pages,
        &redacted,
        &fm,
        &effective_collections,
        &job_id,
        state,
    )
    .await
    {
        Ok(e) => e,
        Err(err_events) => return err_events,
    };

    let _ = search.update_ingest_job_status(&job_id, "completed", None);

    events.push(stored_event);
    events.push(Event::Done { status: 0 });
    events
}

/// Run the post-Extract pipeline: validate pages → dedup search → optional
/// wiki-side Merge → transactional store via `store_ingest_batch` → embed →
/// filesystem writes. Used by both transcript and document ingest paths.
///
/// The caller hands in a fully-populated `RawFrontmatter` (source,
/// source_kind, title, optional agent/session_id). This helper does not
/// inspect those fields — it just serializes the frontmatter into the raw
/// file and uses `fm.source` / `fm.title` for downstream wiki bookkeeping.
///
/// `source_text` is the cleaned/redacted body. It's content-hashed for the
/// raw path and fed to `store_ingest_batch`, which `INSERT OR IGNORE`s on
/// content (safe under retries).
///
/// Returns `Ok(Event::Stored)` on success, or `Err(Vec<Event>)` containing
/// error+done events on any failure. Caller is responsible for prepending
/// per-source progress events (Parsing, Distilling) and for flushing the
/// trailing `Done` after the returned `Stored` event.
async fn store_extracted_pages(
    pages: Vec<crate::daemon::queue::ExtractedPage>,
    source_text: &str,
    fm: &memex_core::raw::RawFrontmatter,
    collections: &[String],
    job_id: &str,
    state: &HandlerState,
) -> Result<Event, Vec<Event>> {
    let source_path = fm.source.as_deref().unwrap_or("");
    let source_title = fm.title.as_deref().unwrap_or("");
    let root_path = state.writer.bound_root().to_path_buf();
    let memex = match get_or_open_memex(state.writer.memex_handle(), &root_path) {
        Ok(m) => m,
        Err(e) => return Err(error_events(e)),
    };
    let search = memex.search();
    let wiki_dir = memex.wiki_dir();

    // 1. Validate the extracted pages.
    let valid_pages = validate_extracted_pages(pages);
    if valid_pages.is_empty() {
        let _ = search.update_ingest_job_status(job_id, "completed", None);
        return Ok(Event::Stored {
            job_id: job_id.to_string(),
            source_docid: String::new(),
            wiki_pages: Vec::new(),
        });
    }

    let _ = search.update_ingest_job_status(job_id, "processing", None);

    // 2. Dedup search against existing wiki pages BEFORE slug-lock
    // acquisition. The dedup step is read-only (BM25+vector probe over
    // documents and on-disk reads of candidate wiki files), so it's safe
    // to run unsynchronized. Running it first lets us know the full set
    // of slugs that will ultimately be written — both new slugs from
    // valid_pages AND any existing slugs that fuzzy-matched as merge
    // targets — so step 3 can lock the union. Locking only proposed
    // slugs (the prior shape) raced two concurrent ingests whose
    // proposals fuzzy-matched onto the same existing slug: each held
    // its proposed-slug lock but not the merge-target lock, then both
    // store_ingest_batch calls hit `ON CONFLICT (doc_type, path) DO
    // UPDATE` on the same row, lost-update style.
    // Phase 1: compute candidate dedup slugs while holding the model
    // lock. Phase 2: release the lock and read existing-page bodies.
    // Holding the embedder mutex across `tokio::fs::read_to_string` of
    // every merge target stalls every concurrent query (the daemon
    // shares one warm embedder), and on slow disks the fan-out can be
    // tens of milliseconds per page.
    let dedup_slugs = {
        let mut model_guard = state.writer.embed_model().lock().await;
        match find_dedup_slugs(search, &valid_pages, model_guard.as_mut(), memex.root()) {
            Ok(s) => s,
            Err(e) => {
                let _ = search.update_ingest_job_status(job_id, "failed", Some(&e.to_string()));
                return Err(error_events(e));
            }
        }
    };
    let (new_pages, merge_pairs) =
        materialize_dedup_pairs(&wiki_dir, &valid_pages, dedup_slugs).await;

    tracing::info!(
        new = new_pages.len(),
        merge = merge_pairs.len(),
        "dedup search complete"
    );

    // 3. Pre-compute the inputs each per-slug task needs:
    //  - existing_titled = (slug, title) pairs already in the index;
    //    used by forward_link target set.
    //  - titled_pool     = auto_link_eligible-filtered union of
    //    existing_titled ∪ this batch's (slug, title); pre-filtering
    //    here means each per-slug task does no work besides iteration.
    //    forward_link itself handles self-exclusion via its
    //    `self_stem` argument.
    //  - known_slugs     = on-disk wiki stems ∪ batch slugs; used by
    //    scrub_wiki_links to drop `[[orphan]]` references the LLM may
    //    have emitted for pages that got absorbed during MERGE or
    //    hallucinated.
    let now_dt = chrono::Utc::now();
    let existing_titled = search.all_stems_and_titles().unwrap_or_default();
    let titled_pool: Vec<(String, String)> = existing_titled
        .iter()
        .cloned()
        .chain(new_pages.iter().map(|p| (p.slug.clone(), p.title.clone())))
        .filter(|(s, _)| memex_core::crosslink::auto_link_eligible(s))
        .collect();
    let mut known_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in &new_pages {
        known_slugs.insert(p.slug.clone());
    }
    for p in &merge_pairs {
        known_slugs.insert(p.slug.clone());
    }
    // Existing wiki stems come from the same `all_stems_and_titles`
    // query above — no need for a second `read_dir` pass over wiki_dir.
    for (stem, _) in &existing_titled {
        known_slugs.insert(stem.clone());
    }

    // 4. Per-slug fan-out: each slug is its own future that holds only
    // its own slug's lock from re-read through DB commit + embed. A
    // slow MERGE on one slug doesn't block another slug's commit, and
    // a concurrent ingest touching only the OTHER slug runs in
    // parallel. Futures share borrowed state (via SlugBatchCtx) and
    // are awaited in this scope, so no spawn / 'static bound needed.
    let ctx = SlugBatchCtx {
        wiki_dir: &wiki_dir,
        titled_pool: &titled_pool,
        known_slugs: &known_slugs,
        transcript_path: source_path,
        collections,
        now_dt,
    };
    let mut futs: Vec<_> = Vec::with_capacity(new_pages.len() + merge_pairs.len());
    for page in new_pages.iter().cloned() {
        futs.push(process_one_slug(state, &memex, SlugKind::New(page), &ctx));
    }
    for pair in merge_pairs.iter().cloned() {
        futs.push(process_one_slug(state, &memex, SlugKind::Merge(pair), &ctx));
    }
    let mut committed_slugs: Vec<String> = Vec::new();
    let results = futures::future::join_all(futs).await;
    for result in results {
        match result {
            Ok(Some(slug)) => committed_slugs.push(slug),
            Ok(None) => {} // merge skipped (LLM timeout or missing-slug); already logged
            Err(events) => return Err(events),
        }
    }

    // 5. Coordinator step: write the raw file, commit the source row,
    // embed it, run cross-page backlink maintenance for new slugs.
    // Raw write happens AFTER all per-slug commits so the dedup
    // invariant `raw on disk ⇒ wiki updates on disk` still holds.
    let source_hash = memex_core::storage::content_hash(source_text.as_bytes());
    let raw_path = memex_core::raw::raw_path_for_hash(&memex.raw_dir(), &source_hash);
    let raw_file = memex_core::raw::assemble_raw_file(fm, source_text);
    if !raw_path.exists() {
        if let Some(parent) = raw_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(path=%raw_path.display(), error=?e, "create raw dir failed");
        }
        if let Err(e) = memex_core::storage::atomic_write(&raw_path, raw_file.as_bytes()) {
            return Err(error_events(DaemonError::Storage(format!(
                "raw atomic_write failed at {}: {e}",
                raw_path.display()
            ))));
        }
    }
    let stored_source_hash = match search.store_raw_source(
        &raw_file,
        source_path,
        source_title,
        collections,
        memex.root(),
    ) {
        Ok(h) => h,
        Err(e) => {
            let _ = search.update_ingest_job_status(job_id, "failed", Some(&e.to_string()));
            return Err(error_events(DaemonError::Internal(format!(
                "raw source commit failed: {e}"
            ))));
        }
    };
    debug_assert_eq!(stored_source_hash, source_hash);

    // Embed the raw source and run cross-page backlink maintenance
    // under a single embed-model lock acquisition — both need the
    // model and run sequentially, so two separate `.lock().await`
    // calls would just thrash the lock with reconcile/watcher.
    {
        let mut guard = state.writer.embed_model().lock().await;
        let mut embed_ctx = EmbedCtx {
            search,
            model: guard.as_mut(),
            memex_root: memex.root(),
        };
        embed_and_mark(
            &mut embed_ctx,
            "raw",
            &raw_path,
            &stored_source_hash,
            source_title,
            source_text,
        );
        let eligible: Vec<(&str, &str)> = new_pages
            .iter()
            .filter(|p| memex_core::crosslink::auto_link_eligible(&p.slug))
            .map(|p| (p.slug.as_str(), p.title.as_str()))
            .collect();
        if !eligible.is_empty()
            && let Err(e) =
                memex_core::crosslink::maintain_backlinks_batch(&memex, &eligible, embed_ctx.model)
        {
            tracing::warn!(?e, "maintain_backlinks_batch failed during ingest");
        }
    }

    let source_docid = memex_core::docid::short(&stored_source_hash).to_string();
    Ok(Event::Stored {
        job_id: job_id.to_string(),
        source_docid,
        wiki_pages: committed_slugs,
    })
}

/// One slug's input to the fan-out. New = no existing wiki page (write
/// proposed body as-is); Merge = existing+proposed pair (dispatch a
/// single-slug MERGE LLM call before write).
enum SlugKind {
    New(crate::daemon::queue::ExtractedPage),
    Merge(crate::daemon::queue::MergePair),
}

/// Per-batch state shared by every per-slug task in one ingest.
/// Computed once before fan-out; passed by reference into each
/// `process_one_slug` / `build_one_wiki_record` call. Bundles the
/// six values that are otherwise positional parameters ripe for
/// ordering bugs.
struct SlugBatchCtx<'a> {
    wiki_dir: &'a Path,
    titled_pool: &'a [(String, String)],
    known_slugs: &'a std::collections::HashSet<String>,
    transcript_path: &'a str,
    collections: &'a [String],
    now_dt: chrono::DateTime<chrono::Utc>,
}

/// Process one slug end-to-end under that slug's write lock:
/// (re-read existing for Merge) → optional single-slug MERGE LLM →
/// build wiki record → write file → commit DB row → embed.
///
/// Returns `Ok(Some(slug))` on a committed update, `Ok(None)` if the
/// MERGE step skipped this slug (LLM timeout or missing-slug reply —
/// both already logged), or `Err(events)` on a propagating write/commit
/// failure that should abort the whole ingest.
async fn process_one_slug(
    state: &HandlerState,
    memex: &memex_core::Memex,
    kind: SlugKind,
    ctx: &SlugBatchCtx<'_>,
) -> Result<Option<String>, Vec<Event>> {
    let slug = match &kind {
        SlugKind::New(p) => p.slug.clone(),
        SlugKind::Merge(p) => p.slug.clone(),
    };
    let page_path = memex_core::wiki::wiki_path_for_slug(ctx.wiki_dir, &slug);
    let _slug_guard = acquire_slug_lock(&state.writer, &slug).await;

    // Re-read existing under lock (dedup snapshot may be stale by the
    // time we hold the lock — a concurrent ingest may have committed a
    // newer body in between). Cached for `build_one_wiki_record` so we
    // don't read the same file twice.
    let prior_content: Option<String> = tokio::fs::read_to_string(&page_path).await.ok();

    let final_page: crate::daemon::queue::ExtractedPage = match kind {
        SlugKind::New(p) => p,
        SlugKind::Merge(mut pair) => {
            if let Some(latest) = prior_content.as_deref() {
                let body = memex_core::validate::parse_frontmatter(latest)
                    .map(|(_fm, b)| b.to_string())
                    .unwrap_or_else(|_| latest.to_string());
                if body != pair.existing {
                    tracing::info!(
                        slug = %pair.slug,
                        "merge target changed since dedup; re-reading under lock"
                    );
                    pair.existing = body;
                }
            }
            match run_one_slug_merge(state, pair).await {
                Ok(Some(page)) => page,
                Ok(None) | Err(_) => return Ok(None),
            }
        }
    };

    let record = build_one_wiki_record(&final_page, prior_content.as_deref(), ctx);

    if let Err(e) = async_atomic_write(page_path.clone(), record.content.as_bytes().to_vec()).await
    {
        return Err(error_events(DaemonError::Storage(format!(
            "wiki file write failed for {slug}: {e}"
        ))));
    }

    let body_hash = match memex
        .search()
        .store_wiki_page(&record, ctx.collections, memex.root())
    {
        Ok(h) => h,
        Err(e) => {
            return Err(error_events(DaemonError::Internal(format!(
                "wiki page commit failed for {slug}: {e}"
            ))));
        }
    };

    let body_slice = memex_core::storage::split_frontmatter(&record.content)
        .map(|(_fm, b)| b)
        .unwrap_or(&record.content);
    {
        let mut guard = state.writer.embed_model().lock().await;
        let mut embed_ctx = EmbedCtx {
            search: memex.search(),
            model: guard.as_mut(),
            memex_root: memex.root(),
        };
        embed_and_mark(
            &mut embed_ctx,
            "wiki",
            &page_path,
            &body_hash,
            &record.title,
            body_slice,
        );
    }

    Ok(Some(slug))
}

/// Dispatch a single-slug MERGE LLM job and unwrap the matching slug
/// from the reply. `Ok(Some(page))` on success; `Ok(None)` on the two
/// soft-skip cases (LLM job failed, or the reply omitted the slug);
/// `Err(_)` is reserved for future hard-error variants. Callers reuse
/// this from both ingest and plan paths.
async fn run_one_slug_merge(
    state: &HandlerState,
    pair: crate::daemon::queue::MergePair,
) -> Result<Option<crate::daemon::queue::ExtractedPage>, ()> {
    use crate::daemon::queue::{BackendJob, MergeJob};
    let target_slug = pair.slug.clone();
    let pair_clone = pair.clone();
    let merge_result = run_worker_job(state, move |reply| {
        BackendJob::Merge(MergeJob {
            pages: vec![pair_clone],
            reply,
        })
    })
    .await;
    match merge_result {
        Ok(reply) => match reply
            .merged_pages
            .into_iter()
            .find(|p| p.slug == target_slug)
        {
            Some(merged) => Ok(Some(crate::daemon::queue::ExtractedPage {
                slug: target_slug,
                title: merged.title,
                body: merged.body,
            })),
            None => {
                tracing::warn!(
                    slug = %target_slug,
                    "merge output missing pair slug; skipping wiki update for this page"
                );
                Ok(None)
            }
        },
        Err(e) => {
            tracing::warn!(
                %e,
                skipped_slug = %target_slug,
                "merge job failed; skipping wiki update for this page"
            );
            Ok(None)
        }
    }
}

/// Build a single wiki record (frontmatter + scrubbed/forward-linked
/// body) for one extracted page. `prior_content` is the existing
/// `wiki/<slug>.md` markdown if any (caller reads it once, under the
/// slug lock, and shares it between the merge re-read and this call).
fn build_one_wiki_record(
    page: &crate::daemon::queue::ExtractedPage,
    prior_content: Option<&str>,
    ctx: &SlugBatchCtx<'_>,
) -> memex_core::search::IngestWikiPage {
    let scrubbed_body = memex_core::validate::scrub_wiki_links(&page.body, ctx.known_slugs);
    // titled_pool was pre-filtered by `auto_link_eligible` upstream;
    // forward_link skips self via its `self_stem` argument, so no
    // per-slug clone is needed here.
    let (linked_body, _) =
        memex_core::crosslink::forward_link(&scrubbed_body, ctx.titled_pool, &page.slug);

    let (created_at, sources) = prior_content
        .and_then(|c| memex_core::validate::parse_frontmatter(c).ok())
        .map(|(fm, _body)| {
            let mut srcs = fm.sources;
            if !srcs.iter().any(|s| s == ctx.transcript_path) {
                srcs.push(ctx.transcript_path.to_string());
            }
            (fm.created_at, srcs)
        })
        .unwrap_or_else(|| (ctx.now_dt, vec![ctx.transcript_path.to_string()]));

    let content = memex_core::wiki::compose_wiki_markdown(
        &page.title,
        &linked_body,
        created_at,
        &sources,
        ctx.collections,
        ctx.now_dt,
    )
    .expect("frontmatter always serializes");

    memex_core::search::IngestWikiPage {
        slug: page.slug.clone(),
        title: page.title.clone(),
        content,
    }
}

/// Extract pages from already-stored source content. Runs chunked
/// EXTRACT and cross-chunk MERGE consolidation. Returns the FINAL list
/// of proposals (one per slug). Caller is responsible for any post-
/// processing (e.g., MERGE-dry-run for wiki overlap).
///
/// Used by both transcript/document ingest and the source_plan handler.
pub(super) async fn extract_pages_from_content(
    content: &str,
    source_path: &str,
    state: &HandlerState,
) -> Result<Vec<crate::daemon::queue::ExtractedPage>, DaemonError> {
    use crate::daemon::queue::{
        BackendJob, ChunkPosition, ExtractSegment, IngestJob, MergeJob, MergePair,
    };

    let cfg = state.config.ingest.clone();
    let chunks = match memex_core::chunk::chunk_markdown(
        content,
        cfg.chunk_target_tokens,
        cfg.chunk_hard_cap_tokens,
        cfg.max_chunks,
    ) {
        Ok(c) => c,
        Err(e) => return Err(DaemonError::BadRequest(e.to_string())),
    };
    let total_chunks = chunks.len();
    let mut receivers = Vec::with_capacity(total_chunks);
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job = BackendJob::Ingest(IngestJob {
            segments: vec![ExtractSegment {
                index: None,
                role: None,
                timestamp: None,
                text: chunk,
            }],
            source: source_path.to_string(),
            chunk: Some(ChunkPosition {
                index: idx,
                total: total_chunks,
            }),
            reply: tx,
        });
        if state.jobs.submit(job).await.is_err() {
            return Err(DaemonError::Internal("worker queue closed".into()));
        }
        receivers.push(rx);
    }

    let mut all_pages = Vec::new();
    for rx in receivers {
        match rx.await {
            Ok(Ok(reply)) => all_pages.extend(reply.pages),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err(DaemonError::Internal("worker dropped reply".into())),
        }
    }

    // Cross-chunk fragment-merge: same slug from multiple chunks.
    let mut by_slug: std::collections::BTreeMap<String, Vec<crate::daemon::queue::ExtractedPage>> =
        std::collections::BTreeMap::new();
    for p in all_pages {
        if p.slug.is_empty() || p.title.is_empty() || p.body.is_empty() {
            continue;
        }
        let slug = crate::slugify(&p.slug);
        if slug.is_empty() {
            continue;
        }
        by_slug.entry(slug).or_default().push(p);
    }
    let mut merged: Vec<crate::daemon::queue::ExtractedPage> = Vec::new();
    for (slug, fragments) in by_slug {
        if fragments.len() == 1 {
            let mut p = fragments.into_iter().next().unwrap();
            p.slug = slug;
            merged.push(p);
            continue;
        }
        let title = fragments[0].title.clone();
        let mut accum_body = fragments[0].body.clone();
        for next in fragments.into_iter().skip(1) {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let job = BackendJob::Merge(MergeJob {
                pages: vec![MergePair {
                    slug: slug.clone(),
                    proposed: next.body.clone(),
                    existing: accum_body.clone(),
                }],
                reply: tx,
            });
            if state.jobs.submit(job).await.is_err() {
                return Err(DaemonError::Internal("merge queue closed".into()));
            }
            match rx.await {
                Ok(Ok(reply)) => {
                    if let Some(m) = reply.merged_pages.into_iter().next() {
                        accum_body = m.body;
                    }
                }
                _ => {
                    accum_body.push_str("\n\n---\n\n");
                    accum_body.push_str(&next.body);
                }
            }
        }
        merged.push(crate::daemon::queue::ExtractedPage {
            slug,
            title,
            body: memex_core::transcript::truncate(&accum_body, 20_000),
        });
    }
    Ok(merged)
}

/// Drop EXTRACT pages with empty/invalid slug-title-body, re-slugify
/// the rest, truncate bodies. Warns on bad slug shapes (dates, episode
/// words) but does not reject them — see `is_bad_slug`.
///
/// No `take(N)` cap: a previous version capped at 10 (sized for
/// transcripts which run a single Extract call). Document ingest fans
/// out to many concurrent chunk-level Extract calls and merges
/// fragments by slug before this point, so a 10-page cap silently
/// drops legitimate slug groups whenever a long document covers more
/// than 10 distinct subjects. The fragment-merge dedup is the right
/// cap — it's content-driven, not arbitrary.
fn validate_extracted_pages(
    raw: Vec<crate::daemon::queue::ExtractedPage>,
) -> Vec<crate::daemon::queue::ExtractedPage> {
    raw.into_iter()
        .filter_map(|page| {
            if page.slug.is_empty() || page.title.is_empty() || page.body.is_empty() {
                return None;
            }
            let slug = crate::slugify(&page.slug);
            if slug.is_empty() {
                return None;
            }
            if is_bad_slug(&slug) {
                tracing::warn!(
                    slug = %slug,
                    title = %page.title,
                    "non-subject slug emitted by EXTRACT (episode/date/multi-subject)"
                );
            }
            Some(crate::daemon::queue::ExtractedPage {
                slug,
                title: page.title,
                body: memex_core::transcript::truncate(&page.body, 20_000),
            })
        })
        .collect()
}

/// For each proposed page, title-BM25 search existing wiki; if a hit is
/// found and readable, route to `merge_pairs`. Otherwise it becomes a new
/// page. Errors from the search layer propagate up.
/// Phase 1 of dedup: walk every proposed page and ask the BM25+vector
/// search whether it overlaps an existing wiki slug. Synchronous and
/// model-lock-bound. The caller must hold the embedder mutex; we don't
/// take it here so the lifetime stays explicit.
fn find_dedup_slugs(
    search: &memex_core::search::Db,
    valid_pages: &[crate::daemon::queue::ExtractedPage],
    model: &mut dyn memex_core::embed::Embedder,
    memex_root: &Path,
) -> Result<Vec<Option<String>>, DaemonError> {
    let mut slugs = Vec::with_capacity(valid_pages.len());
    for page in valid_pages {
        let existing_slug =
            memex_core::retrieval::search_wiki_by_title(search, &page.title, model, memex_root)
                .map_err(|e| DaemonError::Internal(format!("dedup search: {e}")))?;
        tracing::info!(title = %page.title, result = ?existing_slug, "dedup search");
        slugs.push(existing_slug);
    }
    Ok(slugs)
}

/// Phase 2 of dedup: with the model lock released, read each merge
/// target's body off disk and bucket pages into new vs merge. Async
/// IO here doesn't stall queries; the embedder is free for other
/// tasks while disks fan out.
async fn materialize_dedup_pairs(
    wiki_dir: &Path,
    valid_pages: &[crate::daemon::queue::ExtractedPage],
    slugs: Vec<Option<String>>,
) -> (
    Vec<crate::daemon::queue::ExtractedPage>,
    Vec<crate::daemon::queue::MergePair>,
) {
    let mut new_pages = Vec::new();
    let mut merge_pairs = Vec::new();
    for (page, slug) in valid_pages.iter().zip(slugs.into_iter()) {
        match slug {
            Some(slug) => {
                let existing_path = memex_core::wiki::wiki_path_for_slug(wiki_dir, &slug);
                tracing::info!(slug = %slug, path = %existing_path.display(), "reading existing page for merge");
                if let Ok(existing_content) = tokio::fs::read_to_string(&existing_path).await {
                    // Strip frontmatter before sending to MERGE. If we send
                    // the full file, the LLM sometimes echoes the frontmatter
                    // block into its output body — when the handler then
                    // prepends a fresh frontmatter, the file ends with two
                    // consecutive `---` blocks.
                    let existing_body = memex_core::validate::parse_frontmatter(&existing_content)
                        .map(|(_fm, body)| body)
                        .unwrap_or(existing_content);
                    merge_pairs.push(crate::daemon::queue::MergePair {
                        slug,
                        proposed: page.body.clone(),
                        existing: existing_body,
                    });
                } else {
                    new_pages.push(page.clone());
                }
            }
            None => new_pages.push(page.clone()),
        }
    }
    (new_pages, merge_pairs)
}

/// Stable per-batch context for `embed_and_mark`: the index handle,
/// the embedding model, and the memex root used to derive the DB-relative
/// path stamped on the row.
struct EmbedCtx<'a> {
    search: &'a memex_core::search::Db,
    model: &'a mut dyn memex_core::embed::Embedder,
    memex_root: &'a Path,
}

/// Embed a freshly-stored doc. embed_document atomically stamps
/// `embed_model` + `embedded_at` by hash inside its own transaction,
/// so no follow-up UPDATE is needed here. Errors are logged, not
/// propagated — the doc is already on disk and indexed; missing
/// embedding is recoverable on a later reindex via lint.
fn embed_and_mark(
    ctx: &mut EmbedCtx<'_>,
    doc_type: &str,
    disk_path: &Path,
    hash: &str,
    title: &str,
    body: &str,
) {
    let rel = disk_path.strip_prefix(ctx.memex_root).unwrap_or(disk_path);
    let rel_str = memex_core::storage::rel_path_string(rel);
    if let Err(e) = memex_core::retrieval::embed_document(ctx.search, hash, title, body, ctx.model)
    {
        tracing::warn!(error = ?e, %doc_type, path = %rel_str, "embed_document failed");
    }
}

/// Heuristic: does this slug violate the one-subject rule?
/// Catches date-bearing and episode-word slugs — generic structural patterns,
/// not content-specific. Multi-subject detection is left to the EXTRACT prompt.
fn is_bad_slug(slug: &str) -> bool {
    // Year segment: *-2023, *-2023-08
    let has_year = slug
        .split('-')
        .any(|seg| seg.len() == 4 && seg.chars().all(|c| c.is_ascii_digit()));
    if has_year {
        return true;
    }
    // Month names as segments.
    const MONTHS: &[&str] = &[
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    if slug.split('-').any(|seg| MONTHS.contains(&seg)) {
        return true;
    }
    // Episode markers — unambiguously indicate an episode slug rather
    // than a subject slug. Keep narrow; content-specific terms
    // ("roadtrip", "wedding", etc.) belong in the prompt, not here.
    const EPISODE: &[&str] = &["conversation", "session", "episode"];
    if slug.split('-').any(|seg| EPISODE.contains(&seg)) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the is_bad_slug matrix. Subject slugs flow through; year /
    /// month / episode segments are flagged. The rules apply per
    /// hyphen-separated segment, not as substring matches — so
    /// "january-effect" (where the leading segment IS "january") is
    /// rejected, while "summary-march-meeting" (segment "march" is
    /// internal but still a month segment) is also rejected. Only
    /// slugs with no date / month / episode segment pass.
    #[test]
    fn is_bad_slug_matrix() {
        let cases: &[(&str, bool, &str)] = &[
            // Subject slugs: pass.
            ("auth-tokens", false, "clean two-token subject"),
            ("rest-patterns", false, "clean subject"),
            ("oauth-migration", false, "subject with dash"),
            ("performance-tuning", false, "subject"),
            (
                "january-effect",
                true,
                "month segment, even as concept name",
            ),
            ("summary-march-meeting", true, "month segment internal"),
            ("annual-2023-review", true, "4-digit year segment"),
            ("2023", true, "bare year"),
            ("conversation-with-bob", true, "episode marker"),
            ("session-recap", true, "episode marker — session"),
            ("episode-12", true, "episode marker — episode"),
            // Edge: 4 digits that aren't a year — still flagged.
            // The regex doesn't try to distinguish; cheap, false-positive-tolerant.
            (
                "port-8080-config",
                true,
                "4-digit segment matches year heuristic",
            ),
            // Edge: empty.
            ("", false, "empty slug — no segments to flag"),
        ];
        for (slug, expected_bad, why) in cases {
            assert_eq!(
                is_bad_slug(slug),
                *expected_bad,
                "is_bad_slug({slug:?}) — {why}"
            );
        }
    }
}
