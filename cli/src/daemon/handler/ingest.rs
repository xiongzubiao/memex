//! `Request::Ingest` handlers (transcript + document) plus the shared
//! post-Extract pipeline (`store_extracted_pages`) and its support
//! functions: dedup, fragment-merge, embed-and-mark, atomic file writes.

use std::path::Path;

use crate::daemon::error::DaemonError;
use crate::daemon::handler::{
    HandlerState, acquire_slug_locks, async_atomic_write, error_events, get_or_open_memex,
    read_file_capped, run_worker_job, validate_and_redact_inbound_content, validate_source_path,
};
use crate::daemon::handler::source::derive_source_title;
use crate::daemon::protocol::Event;

/// Handle a transcript-ingest request: parse, dispatch Extract, run the
/// shared post-Extract pipeline (dedup → optional Merge → store → embed).
pub(super) async fn handle_ingest_transcript(
    transcript_path: String,
    agent: String,
    collections: Vec<String>,
    state: &HandlerState,
) -> Vec<Event> {
    use crate::daemon::queue::{BackendJob, IngestJob};
    use memex_core::transcript::SessionFilter;
    use std::path::PathBuf;

    let t0 = std::time::Instant::now();

    if let Err(e) = memex_core::search::validate_collection_names(&collections) {
        return error_events(DaemonError::BadRequest(e));
    }

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

    // Open memex via the per-root handle cache. Re-opening per-request races
    // on schema-init DDL under concurrent ingest.
    let root_path = state.writer.bound_root().to_path_buf();
    let memex = match get_or_open_memex(state.writer.memex_handle(), &root_path) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();
    let effective_collections = memex_core::search::normalize_collections(&collections);

    let transcript = match agent.as_str() {
        "claude-code" => memex_core::transcript::parse_claude_code_session(
            std::io::BufReader::new(raw_content.as_bytes()),
        ),
        "codex" => memex_core::transcript::parse_codex_session(std::io::BufReader::new(
            raw_content.as_bytes(),
        )),
        "gemini-cli" => memex_core::transcript::parse_gemini_cli_session(&raw_content),
        _ => {
            return error_events(DaemonError::BadRequest(format!("unknown agent: {agent}")));
        }
    };

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

    // Hash the cleaned text. This drives BOTH job_id (so a renamed or
    // re-located transcript with identical content is recognized as a
    // duplicate) and the dedup check on documents.hash later in the
    // pipeline. Path-based job_id was the old behavior and re-ingested
    // when a transcript was moved; content-based dedup is the right
    // behavior because content identity is what matters for retrieval.
    let content_hash = memex_core::storage::content_hash(canonical_transcript.as_bytes());
    let job_id = format!("ingest-{}", &content_hash[..16]);

    // Pre-LLM dedup: if any ingest-produced document with this hash
    // already exists, skip the LLM call (mirrors handle_ingest_document).
    match search.ingest_dedup_exists(&content_hash) {
        Ok(true) => {
            tracing::info!(hash=%content_hash, "transcript ingest deduped: content already stored");
            return vec![Event::Done { status: 0 }];
        }
        Ok(false) => {}
        Err(e) => {
            return error_events(DaemonError::Internal(format!("dedup check: {e}")));
        }
    }

    // Persist the job before LLM dispatch so a crash doesn't lose it;
    // INSERT OR IGNORE on the content-derived job_id dedupes concurrent
    // ingests of the same content.
    if let Err(e) = search.insert_ingest_job(
        &job_id,
        memex_core::search::JobType::Transcript,
        &transcript_path,
        Some(&agent),
        &content_hash,
        &effective_collections,
    ) {
        return error_events(DaemonError::Internal(format!(
            "insert_ingest_job failed: {e}"
        )));
    }

    // Emit progress
    let mut events = vec![Event::Parsing {
        job_id: job_id.clone(),
        transcript_path: transcript_path.clone(),
    }];

    events.push(Event::Distilling {
        job_id: job_id.clone(),
        transcript_path: transcript_path.clone(),
    });

    let segments: Vec<crate::daemon::queue::ExtractSegment> = transcript
        .turns
        .iter()
        .enumerate()
        .map(|(i, t)| crate::daemon::queue::ExtractSegment {
            index: Some(i + 1),
            role: Some(t.role.clone()),
            timestamp: t.timestamp.clone(),
            text: t.text.clone(),
        })
        .collect();

    let extracted = match run_worker_job(state, |reply| {
        BackendJob::Ingest(IngestJob {
            segments,
            source: transcript_path.clone(),
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

    let title = memex_core::transcript::truncate(&transcript.first_user_message, 80);
    let summary = memex_core::index::extract_summary(&canonical_transcript, 120);

    let session_id_opt: Option<&str> = if transcript.session_id.is_empty() {
        None
    } else {
        Some(&transcript.session_id)
    };
    let stored_event = match store_extracted_pages(
        extracted.pages,
        &canonical_transcript,
        &transcript_path,
        &title,
        &summary,
        &effective_collections,
        &job_id,
        Some(&agent),
        session_id_opt,
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
        transcript = %transcript_path,
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
    use crate::daemon::queue::{
        BackendJob, ChunkPosition, ExtractSegment, IngestJob, MergeJob, MergePair,
    };

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

    // ─── Dedup: skip if this exact content is already stored ──────────
    match search.ingest_dedup_exists(&content_hash) {
        Ok(true) => {
            tracing::info!(hash=%content_hash, "ingest deduped: source already present");
            return vec![Event::Done { status: 0 }];
        }
        Ok(false) => {}
        Err(e) => {
            return error_events(DaemonError::Internal(format!("dedup check: {e}")));
        }
    }

    // ─── Job ID + persist job row (pre-Extract) ────
    let job_id = format!(
        "doc-{}",
        memex_core::storage::content_hash(
            format!("{source_path}\0{content_hash}").as_bytes()
        )
    );
    if let Err(e) = search.insert_ingest_job(
        &job_id,
        memex_core::search::JobType::Document,
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

    // ─── Chunk ────────────────────────────────────────────────────────
    let chunks = match memex_core::chunk::chunk_markdown(
        &redacted,
        cfg.chunk_target_tokens,
        cfg.chunk_hard_cap_tokens,
        cfg.max_chunks,
    ) {
        Ok(c) => c,
        Err(e) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some(&e.to_string()));
            return error_events(DaemonError::BadRequest(e.to_string()));
        }
    };
    let total_chunks = chunks.len();
    tracing::info!(job_id = %job_id, total_chunks, "dispatching document chunks");

    // ─── Dispatch chunks concurrently ─────────────────────────────────
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
            source: source_path.clone(),
            chunk: Some(ChunkPosition {
                index: idx,
                total: total_chunks,
            }),
            reply: tx,
        });
        if state.jobs.submit(job).await.is_err() {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some("worker queue closed"));
            return error_events(DaemonError::Internal("worker queue closed".into()));
        }
        receivers.push(rx);
    }

    // ─── Gather chunk results ─────────────────────────────────────────
    let mut all_pages: Vec<crate::daemon::queue::ExtractedPage> = Vec::new();
    for rx in receivers {
        match rx.await {
            Ok(Ok(reply)) => all_pages.extend(reply.pages),
            Ok(Err(e)) => {
                let de: DaemonError = e.into();
                let _ = search.update_ingest_job_status(&job_id, "failed", Some(&de.message()));
                return error_events(de);
            }
            Err(_) => {
                let _ = search.update_ingest_job_status(
                    &job_id,
                    "failed",
                    Some("worker dropped reply"),
                );
                return error_events(DaemonError::Internal("worker dropped reply".into()));
            }
        }
    }

    // ─── Cross-chunk fragment-merge: same slug from multiple chunks ──
    let mut by_slug: std::collections::BTreeMap<
        String,
        Vec<crate::daemon::queue::ExtractedPage>,
    > = std::collections::BTreeMap::new();
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

    let mut merged_pages: Vec<crate::daemon::queue::ExtractedPage> = Vec::new();
    for (slug, fragments) in by_slug {
        if fragments.len() == 1 {
            let mut p = fragments.into_iter().next().unwrap();
            p.slug = slug;
            merged_pages.push(p);
            continue;
        }
        // ≥ 2 fragments — chain MergeJobs sequentially.
        let title = fragments[0].title.clone();
        let tags: Vec<String> = fragments
            .iter()
            .flat_map(|p| p.tags.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
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
                let _ =
                    search.update_ingest_job_status(&job_id, "failed", Some("merge queue closed"));
                return error_events(DaemonError::Internal("merge queue closed".into()));
            }
            match rx.await {
                Ok(Ok(reply)) => {
                    if let Some(merged) = reply.merged_pages.into_iter().next() {
                        accum_body = merged.body;
                    }
                }
                Ok(Err(e)) => {
                    // Cross-chunk merge failed; keep accum_body and append divider.
                    tracing::warn!(?e, slug = %slug, "fragment-merge worker error; concatenating with divider");
                    accum_body.push_str("\n\n---\n\n");
                    accum_body.push_str(&next.body);
                }
                Err(_) => {
                    tracing::warn!(slug = %slug, "fragment-merge dropped reply; concatenating with divider");
                    accum_body.push_str("\n\n---\n\n");
                    accum_body.push_str(&next.body);
                }
            }
        }
        merged_pages.push(crate::daemon::queue::ExtractedPage {
            slug,
            title,
            tags,
            body: memex_core::transcript::truncate(&accum_body, 20_000),
        });
    }

    if merged_pages.is_empty() {
        let _ = search.update_ingest_job_status(&job_id, "completed", None);
        events.push(Event::Done { status: 0 });
        return events;
    }

    // ─── Hand off to shared post-Extract pipeline ─────────────────────
    let source_title = derive_source_title(&redacted, &source_path);
    let source_summary = memex_core::index::extract_summary(&redacted, 120);
    let stored_event = match store_extracted_pages(
        merged_pages,
        &redacted,
        &source_path,
        &source_title,
        &source_summary,
        &effective_collections,
        &job_id,
        None,
        None,
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
/// Pre-conditions:
/// - `source_text` is the cleaned/redacted source content. The helper
///   passes it to `store_ingest_batch`, which uses `INSERT OR IGNORE` on
///   content; it's safe to call even when the content row already exists
///   (the document path inserts content earlier for hash-dedup; transcript
///   does not).
/// - `source_title` and `source_summary` are caller-derived strings.
///
/// Returns `Ok(Event::Stored)` on success, or `Err(Vec<Event>)` containing
/// error+done events on any failure. Caller is responsible for prepending
/// per-source progress events (Parsing, Distilling) and for flushing the
/// trailing `Done` after the returned `Stored` event.
#[allow(clippy::too_many_arguments)]
async fn store_extracted_pages(
    pages: Vec<crate::daemon::queue::ExtractedPage>,
    source_text: &str,
    source_path: &str,
    source_title: &str,
    _source_summary: &str,
    collections: &[String],
    job_id: &str,
    agent_label: Option<&str>,
    session_id_opt: Option<&str>,
    state: &HandlerState,
) -> Result<Event, Vec<Event>> {
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
        match find_dedup_slugs(
            search,
            &valid_pages,
            model_guard.as_mut(),
            memex.root(),
        ) {
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

    // 3. Acquire per-slug write locks for the UNION of slugs that will
    // be written: new_pages (proposed slugs) + merge_pairs (existing
    // slugs that dedup mapped to). acquire_slug_locks sorts +
    // deduplicates, so concurrent ingests targeting overlapping slugs
    // serialize cleanly without deadlock.
    let lock_slugs: Vec<String> = new_pages
        .iter()
        .map(|p| p.slug.clone())
        .chain(merge_pairs.iter().map(|p| p.slug.clone()))
        .collect();
    let _slug_guards = acquire_slug_locks(&state.writer, lock_slugs).await;

    // 4. Re-read merge targets under the slug lock. Dedup ran
    // unsynchronized, so its `existing` snapshot may be stale by the
    // time we hold the lock. A concurrent ingest that just released the
    // lock could have committed a newer body. Submitting the dedup-time
    // snapshot to the merge worker would silently drop that ingest's
    // contribution (last-write-wins). Re-reading under the lock ensures
    // the merge sees the latest committed state.
    let mut merge_pairs = merge_pairs;
    for pair in &mut merge_pairs {
        let path = wiki_dir.join(format!("{}.md", pair.slug));
        if let Ok(latest) = tokio::fs::read_to_string(&path).await {
            let body = memex_core::validate::parse_frontmatter(&latest)
                .map(|(_fm, body)| body.to_string())
                .unwrap_or(latest);
            if body != pair.existing {
                tracing::info!(
                    slug = %pair.slug,
                    "merge target changed since dedup; re-reading under lock"
                );
                pair.existing = body;
            }
        }
    }

    // 5. Optional wiki-side merge.
    let merged_pages = run_wiki_merge(state, &merge_pairs).await;

    // 6. Build wiki records. Pull existing (stem, title) pairs so
    // build_wiki_records can run forward_link against the union of
    // existing pages + this batch — same eligibility filter as
    // `memex write`.
    let now_dt = chrono::Utc::now();
    let now = now_dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let all_pages: Vec<_> = new_pages.iter().chain(merged_pages.iter()).collect();
    let existing_titled = search.all_stems_and_titles().unwrap_or_default();
    let wiki_pages = build_wiki_records(
        &all_pages,
        &wiki_dir,
        &existing_titled,
        source_path,
        collections,
        now_dt,
    )
    .await;

    // 7. Filesystem-canonical write order: bodies on disk are the
    // source of truth, so they land FIRST. If any write fails, abort
    // before touching the DB — the alternative (commit row, then write
    // file, then notice failure) leaves a documents row pointing at
    // nothing readable, and lint can't reconstruct the body (the
    // MissingFile fix is a no-op until reconcile-against-raw lands).
    // The reverse order leaves orphan files on DB-commit failure,
    // which lint's reindex-from-disk path DOES recover.
    let source_hash = memex_core::storage::content_hash(source_text.as_bytes());
    let raw_path = memex_core::raw::raw_path_for_hash(&memex.raw_dir(), &source_hash);
    // Transcript ingest emits source_kind=transcript; document/url
    // ingest falls through to the URL/path heuristic.
    let kind = if agent_label.is_some() {
        "transcript"
    } else if memex_core::raw::is_url(source_path) {
        "url"
    } else {
        "path"
    };
    let fm = memex_core::raw::RawFrontmatter {
        source: Some(source_path.to_string()),
        source_kind: Some(kind.into()),
        ingested_at: Some(now.clone()),
        converter: None,
        title: Some(source_title.to_string()),
        agent: agent_label.map(|s| s.to_string()),
        session_id: session_id_opt
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
    };
    let raw_file = memex_core::raw::assemble_raw_file(&fm, source_text);
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

    // 8. Wiki files. Same fail-fast contract: a write failure aborts
    // before the DB commit, so the row never gets created with no
    // backing file.
    let wiki_failures = write_wiki_files(&wiki_dir, &wiki_pages).await;
    if !wiki_failures.is_empty() {
        let detail = wiki_failures
            .iter()
            .map(|(slug, err)| format!("{slug}: {err}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(error_events(DaemonError::Storage(format!(
            "wiki file write failed for {} page(s): {detail}",
            wiki_failures.len()
        ))));
    }

    // 9-10 + cross-link: durable commit, embed every doc, maintain
    // backlinks across the wiki — all under the same slug-lock window
    // so concurrent ingests don't race the post-commit work.
    let input = IngestBatchInput {
        memex: &memex,
        job_id,
        raw_file: &raw_file,
        raw_path: &raw_path,
        source_path,
        source_title,
        source_text,
        source_hash: &source_hash,
        wiki_pages: &wiki_pages,
        new_pages: &new_pages,
        collections,
        now: &now,
    };
    let batch_result = commit_and_finalize_ingest(state, input).await?;
    let source_docid = memex_core::docid::short(&batch_result.source_hash).to_string();
    Ok(Event::Stored {
        job_id: job_id.to_string(),
        source_docid,
        wiki_pages: batch_result.wiki_hashes.into_iter().map(|(s, _)| s).collect(),
    })
}

/// Phase 5: dispatch the wiki-side MERGE LLM job for every dedup-matched
/// slug. On success, zip the worker's reply back to the input pairs and
/// emit one `ExtractedPage` per merged slug. On failure, log + skip the
/// affected slugs: falling back to "write the proposed pages as new"
/// would route through `store_ingest_batch`'s
/// `ON CONFLICT(doc_type, path) DO UPDATE` and destructively overwrite
/// the accumulated existing page with just this session's contribution.
/// Skipping leaves the existing page intact for the next ingest to
/// merge against.
async fn run_wiki_merge(
    state: &HandlerState,
    merge_pairs: &[crate::daemon::queue::MergePair],
) -> Vec<crate::daemon::queue::ExtractedPage> {
    use crate::daemon::queue::{BackendJob, MergeJob};
    if merge_pairs.is_empty() {
        return Vec::new();
    }
    let merge_result = run_worker_job(state, |reply| {
        BackendJob::Merge(MergeJob {
            pages: merge_pairs.to_vec(),
            reply,
        })
    })
    .await;
    match merge_result {
        Ok(reply) => merge_pairs
            .iter()
            .enumerate()
            .filter_map(|(i, pair)| {
                reply.merged_pages.get(i).map(|page| {
                    crate::daemon::queue::ExtractedPage {
                        slug: pair.slug.clone(),
                        title: page.title.clone(),
                        tags: page.tags.clone(),
                        body: page.body.clone(),
                    }
                })
            })
            .collect(),
        Err(e) => {
            let skipped: Vec<&str> = merge_pairs.iter().map(|p| p.slug.as_str()).collect();
            tracing::warn!(
                %e,
                skipped_slugs = ?skipped,
                "merge job failed; skipping wiki update for these pages"
            );
            Vec::new()
        }
    }
}

/// Per-call inputs for `commit_and_finalize_ingest`. Bundled so the
/// fn signature stays readable; `state: &HandlerState` covers the
/// daemon-shared bits (search handle, embedder, writer session) and
/// this struct holds the per-request data flowing through the pipeline.
struct IngestBatchInput<'a> {
    memex: &'a memex_core::Memex,
    job_id: &'a str,
    raw_file: &'a str,
    raw_path: &'a Path,
    source_path: &'a str,
    source_title: &'a str,
    source_text: &'a str,
    source_hash: &'a str,
    wiki_pages: &'a [memex_core::search::IngestWikiPage],
    new_pages: &'a [crate::daemon::queue::ExtractedPage],
    collections: &'a [String],
    now: &'a str,
}

/// Phases 9 + 10 + auto cross-link: do the durable DB commit, then
/// embed every committed doc, then sweep wiki for backlinks to the new
/// pages. All three are bundled so the embedder lock is acquired once
/// and the slug-lock window covers every post-commit mutation.
///
/// Failure semantics:
/// - Phase 9 (DB commit) failure aborts the function and returns Err;
///   wiki + raw files are durably on disk so lint's reindex path
///   recovers.
/// - Phase 10 (embed) failures are logged inside `embed_and_mark` and
///   do not propagate — chunks are best-effort, lint will retry.
/// - Cross-link failure is logged and ignored — backlinks are passive
///   and reconcile will eventually catch missing ones.
async fn commit_and_finalize_ingest(
    state: &HandlerState,
    input: IngestBatchInput<'_>,
) -> Result<memex_core::search::IngestBatchResult, Vec<Event>> {
    let search = input.memex.search();
    let wiki_dir = input.memex.wiki_dir();

    // Phase 9: transactional DB commit.
    let batch_result = match search.store_ingest_batch(
        input.raw_file,
        input.source_path,
        input.source_title,
        input.wiki_pages,
        input.collections,
        input.now,
    ) {
        Ok(r) => r,
        Err(e) => {
            let _ = search.update_ingest_job_status(
                input.job_id,
                "failed",
                Some(&e.to_string()),
            );
            return Err(error_events(DaemonError::Internal(format!(
                "storage failed: {e}"
            ))));
        }
    };
    debug_assert_eq!(
        batch_result.source_hash, input.source_hash,
        "source_hash recomputed in store_ingest_batch must match the one used to write the raw file"
    );

    // Phase 10: embed source + each wiki page under the shared model.
    {
        let mut guard = state.writer.embed_model().lock().await;
        let mut embed_ctx = EmbedCtx {
            search,
            model: guard.as_mut(),
            memex_root: input.memex.root(),
        };
        embed_and_mark(
            &mut embed_ctx,
            "raw",
            input.raw_path,
            &batch_result.source_hash,
            input.source_title,
            input.source_text,
        );
        for (page, (slug, page_hash)) in
            input.wiki_pages.iter().zip(&batch_result.wiki_hashes)
        {
            debug_assert_eq!(&page.slug, slug);
            let wiki_disk = wiki_dir.join(format!("{slug}.md"));
            embed_and_mark(
                &mut embed_ctx,
                "wiki",
                &wiki_disk,
                page_hash,
                &page.title,
                &page.content,
            );
        }
    }

    // Auto cross-link backward: sweep wiki pages for un-linked mentions
    // of every newly-created (not merged) eligible slug. Batched form
    // walks the wiki dir once across all new pages.
    {
        let eligible: Vec<(&str, &str)> = input
            .new_pages
            .iter()
            .filter(|p| memex_core::crosslink::auto_link_eligible(&p.slug))
            .map(|p| (p.slug.as_str(), p.title.as_str()))
            .collect();
        if !eligible.is_empty() {
            let mut guard = state.writer.embed_model().lock().await;
            if let Err(e) = memex_core::crosslink::maintain_backlinks_batch(
                input.memex,
                &eligible,
                guard.as_mut(),
            ) {
                tracing::warn!(?e, "maintain_backlinks_batch failed during ingest");
            }
        }
    }

    Ok(batch_result)
}

/// Turn a batch of merged/new extracted pages into insert-ready records:
/// scrub dead wiki links, read each page's prior frontmatter (in parallel),
/// preserve `created_at`, accumulate `sources`, and compose the full
/// markdown with frontmatter.
async fn build_wiki_records(
    all_pages: &[&crate::daemon::queue::ExtractedPage],
    wiki_dir: &Path,
    existing_titled: &[(String, String)],
    transcript_path: &str,
    effective_collections: &[String],
    now_dt: chrono::DateTime<chrono::Utc>,
) -> Vec<memex_core::search::IngestWikiPage> {
    // Known slugs = surviving pages in this batch + everything already on
    // disk. Used to scrub `[[other-slug]]` references the LLM may have
    // emitted for pages that got absorbed during MERGE or hallucinated.
    let mut known_slugs: std::collections::HashSet<String> =
        all_pages.iter().map(|p| p.slug.clone()).collect();
    if let Ok(mut entries) = tokio::fs::read_dir(wiki_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                known_slugs.insert(stem.to_string());
            }
        }
    }

    // Forward-link target set: existing pages from the index + new
    // pages being written in this batch (so two new pages mentioning
    // each other cross-link). Eligibility filter applied per call site.
    let mut titled_pool: Vec<(String, String)> = existing_titled.to_vec();
    for p in all_pages {
        titled_pool.push((p.slug.clone(), p.title.clone()));
    }

    // Preserve created_at and accumulate sources across merges. Without
    // this, each merge would overwrite the previous session list and
    // created_at, leaving merged pages looking single-source.
    let prior_read_handles: Vec<_> = all_pages
        .iter()
        .map(|page| {
            let existing_path = wiki_dir.join(format!("{}.md", page.slug));
            tokio::spawn(async move { tokio::fs::read_to_string(&existing_path).await.ok() })
        })
        .collect();

    let mut records = Vec::with_capacity(all_pages.len());
    for (page, prior) in all_pages.iter().zip(prior_read_handles) {
        let scrubbed_body = memex_core::validate::scrub_wiki_links(&page.body, &known_slugs);
        // Auto cross-link forward: bracket eligible existing-title
        // mentions in this body. Self excluded by stem comparison.
        let eligible: Vec<(String, String)> = titled_pool
            .iter()
            .filter(|(s, _)| s != &page.slug && memex_core::crosslink::auto_link_eligible(s))
            .cloned()
            .collect();
        let (linked_body, _linked) =
            memex_core::crosslink::forward_link(&scrubbed_body, &eligible, &page.slug);
        let scrubbed_body = linked_body;
        let safe_title = page.title.replace(['\n', '\r'], " ");

        let existing_content = prior.await.ok().flatten();
        let (created_at, sources) = existing_content
            .as_deref()
            .and_then(|c| memex_core::validate::parse_frontmatter(c).ok())
            .map(|(fm, _body)| {
                let mut srcs = fm.sources;
                if !srcs.iter().any(|s| s == transcript_path) {
                    srcs.push(transcript_path.to_string());
                }
                (fm.created_at, srcs)
            })
            .unwrap_or_else(|| (now_dt, vec![transcript_path.to_string()]));

        let yaml = serde_yaml::to_string(&memex_core::types::PageFrontmatterRef {
            title: &safe_title,
            summary: None,
            tags: &page.tags,
            collections: effective_collections,
            created_at,
            updated_at: now_dt,
            sources: &sources,
        })
        .expect("frontmatter always serializes");
        records.push(memex_core::search::IngestWikiPage {
            slug: page.slug.clone(),
            title: page.title.clone(),
            content: format!("---\n{yaml}---\n\n{scrubbed_body}"),
            tags: page.tags.join(","),
        });
    }
    records
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
                tags: page.tags,
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
    search: &memex_core::search::Bm25Search,
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
                let existing_path = wiki_dir.join(format!("{slug}.md"));
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
    search: &'a memex_core::search::Bm25Search,
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
    if let Err(e) = memex_core::retrieval::embed_document(ctx.search, hash, title, body, ctx.model) {
        tracing::warn!(error = ?e, %doc_type, path = %rel_str, "embed_document failed");
    }
}

/// Atomically write every wiki record to disk, in parallel. Returns
/// the list of slugs that failed to write so the caller can surface
/// the failure as a daemon error rather than silently continuing with
/// a documents row whose file is missing on disk.
async fn write_wiki_files(
    wiki_dir: &Path,
    records: &[memex_core::search::IngestWikiPage],
) -> Vec<(String, String)> {
    let _ = tokio::fs::create_dir_all(wiki_dir).await;
    let handles: Vec<_> = records
        .iter()
        .map(|page| {
            let page_path = wiki_dir.join(format!("{}.md", page.slug));
            let slug = page.slug.clone();
            let fut = async_atomic_write(page_path, page.content.as_bytes().to_vec());
            tokio::spawn(async move { (slug, fut.await) })
        })
        .collect();
    let mut failures = Vec::new();
    for h in handles {
        match h.await {
            Ok((_, Ok(()))) => {}
            Ok((slug, Err(e))) => {
                tracing::warn!(slug = %slug, %e, "failed to write wiki page file");
                failures.push((slug, e.to_string()));
            }
            Err(e) => {
                tracing::warn!(?e, "wiki write task panicked");
                failures.push((String::new(), format!("write task panicked: {e}")));
            }
        }
    }
    failures
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
        "january", "february", "march", "april", "may", "june",
        "july", "august", "september", "october", "november", "december",
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
            ("january-effect", true, "month segment, even as concept name"),
            ("summary-march-meeting", true, "month segment internal"),
            ("annual-2023-review", true, "4-digit year segment"),
            ("2023", true, "bare year"),
            ("conversation-with-bob", true, "episode marker"),
            ("session-recap", true, "episode marker — session"),
            ("episode-12", true, "episode marker — episode"),
            // Edge: 4 digits that aren't a year — still flagged.
            // The regex doesn't try to distinguish; cheap, false-positive-tolerant.
            ("port-8080-config", true, "4-digit segment matches year heuristic"),
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
