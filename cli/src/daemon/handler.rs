//! Per-connection request handler.
//!
//! Called by `server.rs` for each accepted connection. Handles exactly one
//! request per connection and emits a stream of `Event`s terminated by a
//! `done` event.

use crate::daemon::error::DaemonError;
use crate::daemon::memex_cache::MemexCache;
use crate::daemon::protocol::{Event, Request, SUPPORTED_VERSIONS};
use chrono::Utc;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex as TokioMutex, OwnedMutexGuard};

/// Map key identifying a wiki page: (memex_root, slug).
type SlugKey = (String, String);

/// Daemon-shared state accessible to handlers.
pub struct HandlerState {
    pub pid: u32,
    pub started_at: chrono::DateTime<Utc>,
    pub retrieval: crate::daemon::retrieval::RetrievalSender,
    pub jobs: Arc<crate::daemon::worker::WorkerPool>,
    /// Shared per-root Memex handle cache. Both the query path (via the
    /// retrieval actor) and the ingest path read from it so only one
    /// SQLite connection per root is open at a time.
    pub memex_cache: Arc<MemexCache>,
    /// Per-(memex_root, slug) write locks. Acquired before `dedup_against_existing`
    /// reads the existing wiki file and released after `write_wiki_files`,
    /// so parallel ingests that produce the same slug don't race on the
    /// read-modify-write sequence. The outer StdMutex only guards the
    /// map itself (brief lock); the inner TokioMutex is held across the
    /// long-running MERGE LLM call via `.await`.
    pub slug_locks: Arc<StdMutex<HashMap<SlugKey, Arc<TokioMutex<()>>>>>,
}

/// Acquire per-slug locks for a batch, in sorted order to avoid deadlock
/// between concurrent ingests that touch overlapping slug sets.
///
/// The map grows by one `Arc<Mutex<()>>` per distinct (root, slug) ever
/// seen — negligible for typical wikis (hundreds to low-thousands of
/// entries). If a future long-running daemon accumulates millions, GC
/// idle entries with:
///   `state.slug_locks.lock().unwrap()
///        .retain(|_, m| Arc::strong_count(m) > 1);`
/// (strong_count == 1 means only the map holds it; no live waiter or
/// holder. Safe to drop.) Trigger periodically or on idle reap.
async fn acquire_slug_locks(
    state: &HandlerState,
    memex_root: &str,
    mut slugs: Vec<String>,
) -> Vec<OwnedMutexGuard<()>> {
    slugs.sort();
    slugs.dedup();
    let locks: Vec<Arc<TokioMutex<()>>> = {
        let mut map = state
            .slug_locks
            .lock()
            .expect("slug_locks map poisoned");
        slugs
            .into_iter()
            .map(|s| {
                map.entry((memex_root.to_string(), s))
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

/// Look up the shared Memex handle for `root`, opening on first use.
/// Thin wrapper that maps cache-level errors into `DaemonError::Internal`.
fn get_or_open_memex(
    cache: &MemexCache,
    root: &Path,
) -> Result<Arc<memex_core::Memex>, DaemonError> {
    cache
        .get_or_open(root)
        .map_err(|e| DaemonError::Internal(format!("cannot open memex: {e}")))
}

/// `atomic_write` on the blocking thread pool, so the fsync + rename don't
/// stall the tokio runtime.
async fn async_atomic_write(
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
async fn read_file_capped(path: &Path, max_bytes: u64) -> Result<String, DaemonError> {
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
async fn run_worker_job<R, F>(
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

/// Turn a batch of merged/new extracted pages into insert-ready records:
/// scrub dead wiki links, read each page's prior frontmatter (in parallel),
/// preserve `created_at`, accumulate `sources`, and compose the full
/// markdown with frontmatter.
async fn build_wiki_records(
    all_pages: &[&crate::daemon::queue::ExtractedPage],
    wiki_dir: &Path,
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

/// Drop EXTRACT pages with empty/invalid slug-title-body, re-slugify the
/// rest, truncate bodies, and cap at 10. Warns on bad slug shapes (dates,
/// episode words) but does not reject them — see `is_bad_slug`.
fn validate_extracted_pages(
    raw: Vec<crate::daemon::queue::ExtractedPage>,
) -> Vec<crate::daemon::queue::ExtractedPage> {
    raw.into_iter()
        .take(10)
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
async fn dedup_against_existing(
    search: &memex_core::search::Bm25Search,
    wiki_dir: &Path,
    valid_pages: &[crate::daemon::queue::ExtractedPage],
    model: &mut memex_core::embed::EmbeddingModel,
) -> Result<
    (
        Vec<crate::daemon::queue::ExtractedPage>,
        Vec<crate::daemon::queue::MergePair>,
    ),
    DaemonError,
> {
    let mut new_pages = Vec::new();
    let mut merge_pairs = Vec::new();
    for page in valid_pages {
        let existing_slug = memex_core::retrieval::search_wiki_by_title(search, &page.title, model)
            .map_err(|e| DaemonError::Internal(format!("dedup search: {e}")))?;
        tracing::info!(title = %page.title, result = ?existing_slug, "dedup search");
        match existing_slug {
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
    Ok((new_pages, merge_pairs))
}

/// Embed the source transcript and every wiki page from the ingest batch.
/// Reuses hashes from `store_ingest_batch` so no bytes are rehashed.
fn embed_ingested(
    search: &memex_core::search::Bm25Search,
    batch_result: &memex_core::search::IngestBatchResult,
    canonical_transcript: &str,
    wiki_pages: &[memex_core::search::IngestWikiPage],
    model: &mut memex_core::embed::EmbeddingModel,
) -> Result<(), DaemonError> {
    memex_core::retrieval::embed_document(
        search,
        &batch_result.source_hash,
        canonical_transcript,
        model,
    )
    .map_err(|e| DaemonError::Internal(format!("embed transcript: {e}")))?;
    for (page, (slug, page_hash)) in wiki_pages.iter().zip(&batch_result.wiki_hashes) {
        debug_assert_eq!(&page.slug, slug);
        memex_core::retrieval::embed_document(search, page_hash, &page.content, model)
            .map_err(|e| DaemonError::Internal(format!("embed wiki page {slug}: {e}")))?;
        tracing::debug!(slug = %slug, "embedded wiki page");
    }
    Ok(())
}

/// Canonicalize a user-supplied source path, store its content in the DB,
/// and embed it. A missing file logs a warning and returns `Ok(())` — we
/// treat sources as best-effort enrichment, not a hard requirement.
async fn store_additional_source(
    search: &memex_core::search::Bm25Search,
    model: &mut memex_core::embed::EmbeddingModel,
    source_path_str: &str,
    now: &str,
) -> Result<(), DaemonError> {
    let source_path = std::path::PathBuf::from(source_path_str);
    let Ok(source_path) = tokio::fs::canonicalize(&source_path).await else {
        tracing::warn!(path = source_path_str, "source not found; skipping");
        return Ok(());
    };
    let Ok(source_content) = tokio::fs::read_to_string(&source_path).await else {
        tracing::warn!(path = %source_path.display(), "source read failed; skipping");
        return Ok(());
    };
    let source_abs = source_path.to_string_lossy().to_string();
    let source_title = source_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let source_summary = memex_core::index::extract_summary(&source_content, 120);
    let source_hash = search
        .insert_content(&source_content)
        .map_err(|e| DaemonError::Internal(format!("insert_content failed: {e}")))?;
    let source_docid =
        memex_core::docid::allocate_docid(&source_hash, "source", &source_abs, &[]);
    search
        .upsert_document(
            "source",
            &source_abs,
            &source_title,
            &source_hash,
            &source_docid,
            "",
            &source_summary,
            now,
            now,
        )
        .map_err(|e| DaemonError::Internal(format!("upsert_document failed: {e}")))?;
    memex_core::retrieval::embed_document(search, &source_hash, &source_content, model)
        .map_err(|e| DaemonError::Internal(format!("embed source {source_abs}: {e}")))?;
    Ok(())
}

/// Atomically write every wiki record to disk, in parallel. Failures are
/// logged (the DB row already exists; `memex lint` detects the gap).
async fn write_wiki_files(wiki_dir: &Path, records: &[memex_core::search::IngestWikiPage]) {
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
    for h in handles {
        match h.await {
            Ok((_, Ok(()))) => {}
            Ok((slug, Err(e))) => {
                tracing::warn!(slug = %slug, %e, "failed to write wiki page file");
            }
            Err(e) => tracing::warn!(?e, "wiki write task panicked"),
        }
    }
}

/// Given a parsed request, return the stream of events to emit, in order.
/// The final event is always `Event::Done { status }`.
pub async fn handle(req: Request, state: &HandlerState) -> Vec<Event> {
    match req {
        Request::Ping { v } => {
            if !SUPPORTED_VERSIONS.contains(&v) {
                return error_events(DaemonError::VersionMismatch {
                    supported: SUPPORTED_VERSIONS.to_vec(),
                });
            }
            vec![
                Event::Pong {
                    pid: state.pid,
                    started_at: state.started_at.to_rfc3339(),
                },
                Event::Done { status: 0 },
            ]
        }
        Request::Query {
            v,
            question,
            raw,
            top_k,
            collections,
            memex_root,
        } => {
            if !SUPPORTED_VERSIONS.contains(&v) {
                return error_events(DaemonError::VersionMismatch {
                    supported: SUPPORTED_VERSIONS.to_vec(),
                });
            }
            // Retrieval: dispatch to the retrieval actor. Shared by raw + synth.
            use crate::daemon::retrieval::{RetrievalError, RetrievalReq};
            let (tx, rx) = tokio::sync::oneshot::channel();
            let collections = collections;
            let send_result = state
                .retrieval
                .send(RetrievalReq {
                    memex_root: memex_root.clone().into(),
                    question: question.clone(),
                    top_k,
                    collections: collections.clone(),
                    expansion: None,
                    reply: tx,
                })
                .await;
            if send_result.is_err() {
                return error_events(DaemonError::Internal("retrieval actor unavailable".into()));
            }
            let retrieval_resp = match rx.await {
                Ok(r) => r,
                Err(_) => {
                    return error_events(DaemonError::Internal(
                        "retrieval actor dropped reply".into(),
                    ));
                }
            };
            let retrieval_resp = match retrieval_resp {
                Ok(r) => r,
                Err(RetrievalError::Empty) => return error_events(DaemonError::RetrievalEmpty),
                Err(RetrievalError::InvalidRoot(msg)) => {
                    return error_events(DaemonError::BadRequest(format!(
                        "invalid memex_root: {msg}"
                    )));
                }
                Err(RetrievalError::Other(e)) => {
                    return error_events(DaemonError::Internal(e.to_string()));
                }
            };

            // Expansion runs on weak-signal probes before either raw or
            // synth returns, so both paths work from the same retrieval
            // pipeline. Synth then composes an answer; raw returns the
            // expanded context directly.
            use crate::daemon::context as ctx_fmt;
            use crate::daemon::queue::{BackendJob, ExpandJob, SynthJob};
            use crate::daemon::retrieval::ExpansionTerms;

            // Cache the initial entries/signal so fallback branches can reuse them.
            let initial_signal = retrieval_resp.signal;
            let initial_entries = retrieval_resp.entries;

            let mut events_pre: Vec<Event> = Vec::new();
            let entries_for_ctx = if matches!(initial_signal, memex_core::retrieval::Signal::Weak) {
                // Enqueue ExpandJob. Fall back to initial entries on any error.
                let (etx, erx) = tokio::sync::oneshot::channel();
                let expand_sent = state
                    .jobs
                    .submit(BackendJob::Expand(ExpandJob {
                        question: question.clone(),
                        reply: etx,
                    }))
                    .await;
                if expand_sent.is_err() {
                    tracing::warn!("expand queue closed; falling back to un-expanded retrieval");
                    initial_entries
                } else {
                    match erx.await {
                        Ok(Ok(exp)) => {
                            tracing::info!(lex = %exp.lex, vec = %exp.vec, hyde = %exp.hyde, "expansion terms received");
                            events_pre.push(Event::Expansion {
                                lex: exp.lex.clone(),
                                vec: exp.vec.clone(),
                                hyde: exp.hyde.clone(),
                            });
                            // Re-retrieve with expansion.
                            let (tx2, rx2) = tokio::sync::oneshot::channel();
                            let send2 = state
                                .retrieval
                                .send(RetrievalReq {
                                    memex_root: memex_root.into(),
                                    question: question.clone(),
                                    top_k,
                                    collections: collections.clone(),
                                    expansion: Some(ExpansionTerms {
                                        lex: exp.lex,
                                        vec: exp.vec,
                                        hyde: exp.hyde,
                                    }),
                                    reply: tx2,
                                })
                                .await;
                            if send2.is_err() {
                                tracing::warn!("retrieval actor closed during expansion retry");
                                initial_entries
                            } else {
                                match rx2.await {
                                    Ok(Ok(r)) => r.entries,
                                    Ok(Err(e)) => {
                                        tracing::warn!(
                                            ?e,
                                            "expanded retrieval failed; falling back"
                                        );
                                        initial_entries
                                    }
                                    Err(_) => {
                                        tracing::warn!(
                                            "expanded retrieval actor dropped reply; falling back"
                                        );
                                        initial_entries
                                    }
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(
                                ?e,
                                "expand job failed; falling back to un-expanded retrieval"
                            );
                            initial_entries
                        }
                        Err(_) => {
                            tracing::warn!("expand worker dropped reply; falling back");
                            initial_entries
                        }
                    }
                }
            } else {
                initial_entries
            };

            if raw {
                let mut out = events_pre;
                out.push(Event::Context {
                    entries: entries_for_ctx
                        .into_iter()
                        .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
                        .collect(),
                });
                out.push(Event::Done { status: 0 });
                return out;
            }

            let context = ctx_fmt::format(&entries_for_ctx);
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let job = BackendJob::Synth(SynthJob {
                context,
                question: question.clone(),
                reply: reply_tx,
            });
            if state.jobs.submit(job).await.is_err() {
                return error_events(DaemonError::Internal("worker queue closed".into()));
            }
            let synth_reply = match reply_rx.await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return error_events(e.into()),
                Err(_) => {
                    return error_events(DaemonError::Internal("worker dropped reply".into()));
                }
            };

            let mut out = events_pre;
            out.push(Event::Answer {
                text: synth_reply.answer,
                citations: synth_reply.citations,
            });
            out.push(Event::Done { status: 0 });
            out
        }

        Request::Ingest {
            v,
            transcript_path,
            agent,
            collections,
            memex_root,
        } => {
            if !SUPPORTED_VERSIONS.contains(&v) {
                return error_events(DaemonError::VersionMismatch {
                    supported: SUPPORTED_VERSIONS.to_vec(),
                });
            }
            handle_ingest(transcript_path, agent, collections, memex_root, state).await
        }

        Request::Write {
            v,
            title,
            content,
            tags,
            sources,
            force,
            memex_root,
        } => {
            if !SUPPORTED_VERSIONS.contains(&v) {
                return error_events(DaemonError::VersionMismatch {
                    supported: SUPPORTED_VERSIONS.to_vec(),
                });
            }
            handle_write(title, content, tags, sources, force, memex_root, state).await
        }

        // Delete and LintFix — stubs
        Request::Delete { v, .. } | Request::LintFix { v, .. } => {
            if !SUPPORTED_VERSIONS.contains(&v) {
                return error_events(DaemonError::VersionMismatch {
                    supported: SUPPORTED_VERSIONS.to_vec(),
                });
            }
            vec![
                Event::Error {
                    code: "not_implemented".into(),
                    message: "Delete/LintFix routing not yet implemented.".into(),
                    status: 1,
                },
                Event::Done { status: 1 },
            ]
        }
    }
}

/// Handle a write request: store wiki page via daemon.
/// Replicates the `run_write` logic but inside the daemon process (warm ONNX).
async fn handle_write(
    title: String,
    content: String,
    tags: Vec<String>,
    sources: Vec<String>,
    force: bool,
    memex_root: String,
    state: &HandlerState,
) -> Vec<Event> {
    use std::path::PathBuf;

    if title.trim().is_empty() {
        return error_events(DaemonError::BadRequest("empty title".into()));
    }
    if content.trim().is_empty() {
        return error_events(DaemonError::BadRequest("empty content".into()));
    }

    let root_path = PathBuf::from(&memex_root);
    let memex = match get_or_open_memex(&state.memex_cache, &root_path) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };

    let search = memex.search();
    let slug = crate::slugify(&title);
    if slug.is_empty() {
        return error_events(DaemonError::BadRequest("title produces empty slug".into()));
    }

    // Check for existing page
    if !force {
        let wiki_path = memex.wiki_dir().join(format!("{slug}.md"));
        if wiki_path.exists() {
            return error_events(DaemonError::BadRequest(format!(
                "page '{slug}' already exists. Use force=true to overwrite."
            )));
        }
    }

    // Store content
    let hash = match search.insert_content(&content) {
        Ok(h) => h,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("insert_content failed: {e}")));
        }
    };

    let now = memex_core::search::now_rfc3339();
    let docid = memex_core::docid::allocate_docid(&hash, "wiki", &slug, &[]);
    let tags_str = tags.join(",");

    if let Err(e) = search.upsert_document(
        "wiki", &slug, &title, &hash, &docid, &tags_str, "", &now, &now,
    ) {
        return error_events(DaemonError::Internal(format!(
            "upsert_document failed: {e}"
        )));
    }

    // Write markdown file
    let wiki_dir = memex.wiki_dir();
    let _ = tokio::fs::create_dir_all(&wiki_dir).await;
    let page_path = wiki_dir.join(format!("{slug}.md"));
    if let Err(e) = async_atomic_write(page_path, content.as_bytes().to_vec()).await {
        return error_events(e);
    }

    // Embed wiki page + sources. Load model once for all.
    let mut model = match memex_core::retrieval::load_default_model() {
        Ok(m) => m,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("{e}")));
        }
    };
    if let Err(e) = memex_core::retrieval::embed_document(search, &hash, &content, &mut model) {
        return error_events(DaemonError::Internal(format!("embed wiki page: {e}")));
    }

    for source_path_str in &sources {
        if let Err(e) =
            store_additional_source(search, &mut model, source_path_str, &now).await
        {
            return error_events(e);
        }
    }

    vec![Event::Written { slug, docid }, Event::Done { status: 0 }]
}

/// Handle an ingest request: validate, dedup, parse, filter, dispatch to
/// worker, dedup search, optional merge, validate output, store atomically.
async fn handle_ingest(
    transcript_path: String,
    agent: String,
    collections: Vec<String>,
    memex_root: String,
    state: &HandlerState,
) -> Vec<Event> {
    use crate::daemon::queue::{BackendJob, IngestJob, MergeJob};
    use memex_core::transcript::SessionFilter;
    use std::hash::{Hash, Hasher};
    use std::path::PathBuf;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    transcript_path.hash(&mut hasher);
    let job_id = format!("ingest-{:x}", hasher.finish());
    let t0 = std::time::Instant::now();

    let path = PathBuf::from(&transcript_path);
    if !path.is_absolute() {
        return error_events(DaemonError::BadRequest(
            "transcript_path must be absolute".into(),
        ));
    }

    const MAX_TRANSCRIPT_BYTES: u64 = 50 * 1024 * 1024;
    let raw_content = match read_file_capped(&path, MAX_TRANSCRIPT_BYTES).await {
        Ok(c) => c,
        Err(e) => return error_events(e),
    };

    // Open memex via the per-root handle cache. Re-opening per-request races
    // on schema-init DDL under concurrent ingest.
    let root_path = PathBuf::from(&memex_root);
    let memex = match get_or_open_memex(&state.memex_cache, &root_path) {
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

    // Hash the cleaned text — that's what `store_ingest_batch` writes to the
    // content table. Hashing `raw_content` instead would never match any
    // stored row, so dedup always missed and every re-ingest paid full LLM
    // cost.
    let content_hash = memex_core::storage::content_hash(canonical_transcript.as_bytes());
    if search.content_exists(&content_hash).unwrap_or(false) {
        return vec![Event::Done { status: 0 }]; // already ingested
    }

    // Persist the job before LLM dispatch so a crash doesn't lose it;
    // INSERT OR IGNORE dedupes concurrent ingests of the same transcript.
    if let Err(e) = search.insert_ingest_job(
        &job_id,
        &transcript_path,
        &content_hash,
        &agent,
        &memex_root,
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

    let extracted = match run_worker_job(state, |reply| {
        BackendJob::Ingest(IngestJob {
            turns: transcript.turns.clone(),
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

    let valid_pages = validate_extracted_pages(extracted.pages);

    let _ = search.update_ingest_job_status(&job_id, "processing", None);

    // Serialize dedup-read → MERGE → write for this batch's slugs.
    // Locks the EXTRACT-proposed slugs; if dedup later routes to a
    // different existing slug (title-similarity match with a different
    // slug), that edge case still races. Covers the common same-person
    // slug stability case that caused >half of john.md's sources to be
    // silently dropped on LoCoMo conv6.
    let lock_slugs: Vec<String> = valid_pages.iter().map(|p| p.slug.clone()).collect();
    let _slug_guards = acquire_slug_locks(state, &memex_root, lock_slugs).await;

    // Dedup: title-BM25 + vector rerank to decide new-page vs merge-with-existing.
    let mut model = match memex_core::retrieval::load_default_model() {
        Ok(m) => m,
        Err(e) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some(&e.to_string()));
            return error_events(DaemonError::Internal(format!("{e}")));
        }
    };
    let wiki_dir = memex.wiki_dir();
    let (mut new_pages, merge_pairs) =
        match dedup_against_existing(search, &wiki_dir, &valid_pages, &mut model).await {
            Ok(r) => r,
            Err(e) => {
                let _ = search.update_ingest_job_status(&job_id, "failed", Some(&e.to_string()));
                return error_events(e);
            }
        };

    tracing::info!(
        new = new_pages.len(),
        merge = merge_pairs.len(),
        "dedup search complete"
    );
    let mut merged_pages = Vec::new();
    if !merge_pairs.is_empty() {
        let merge_result = run_worker_job(state, |reply| {
            BackendJob::Merge(MergeJob {
                pages: merge_pairs.clone(),
                reply,
            })
        })
        .await;
        match merge_result {
            Ok(reply) => {
                // Force merged pages to use the existing slugs, not whatever
                // the LLM returned. The merge should update the existing page.
                for (i, pair) in merge_pairs.iter().enumerate() {
                    if let Some(page) = reply.merged_pages.get(i) {
                        merged_pages.push(crate::daemon::queue::ExtractedPage {
                            slug: pair.slug.clone(),
                            title: page.title.clone(),
                            tags: page.tags.clone(),
                            body: page.body.clone(),
                        });
                    }
                }
            }
            Err(e) => {
                tracing::warn!(%e, "merge job failed; storing proposed pages as new");
                for pair in &merge_pairs {
                    new_pages.push(crate::daemon::queue::ExtractedPage {
                        slug: pair.slug.clone(),
                        title: pair.slug.replace('-', " "),
                        tags: vec![],
                        body: pair.proposed.clone(),
                    });
                }
            }
        }
    }

    let now_dt = chrono::Utc::now();
    let now = now_dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let title = memex_core::transcript::truncate(&transcript.first_user_message, 80);
    let summary = memex_core::index::extract_summary(&canonical_transcript, 120);

    let all_pages: Vec<_> = new_pages.iter().chain(merged_pages.iter()).collect();
    let wiki_pages = build_wiki_records(
        &all_pages,
        &wiki_dir,
        &transcript_path,
        &effective_collections,
        now_dt,
    )
    .await;

    let batch_result = match search.store_ingest_batch(
        &canonical_transcript,
        &transcript_path,
        &title,
        &summary,
        &wiki_pages,
        &effective_collections,
        &now,
    ) {
        Ok(r) => r,
        Err(e) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some(&e.to_string()));
            return error_events(DaemonError::Internal(format!("storage failed: {e}")));
        }
    };

    // File writes run after DB commit because they can't be rolled back;
    // lint detects any DB row without its file.
    write_wiki_files(&wiki_dir, &wiki_pages).await;

    if let Err(e) = embed_ingested(
        search,
        &batch_result,
        &canonical_transcript,
        &wiki_pages,
        &mut model,
    ) {
        let _ = search.update_ingest_job_status(&job_id, "failed", Some(&e.to_string()));
        return error_events(e);
    }

    let _ = search.update_ingest_job_status(&job_id, "completed", None);

    tracing::info!(
        job_id = %job_id,
        transcript = %transcript_path,
        new = new_pages.len(),
        merged = merged_pages.len(),
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "ingest job completed"
    );

    events.push(Event::Stored {
        job_id,
        source_docid: batch_result.source_docid,
        wiki_pages: batch_result.wiki_hashes.into_iter().map(|(s, _)| s).collect(),
    });
    events.push(Event::Done { status: 0 });
    events
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

fn error_events(err: DaemonError) -> Vec<Event> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> HandlerState {
        let (r_tx, _r_rx) = tokio::sync::mpsc::channel(1);
        HandlerState {
            pid: 1234,
            started_at: Utc::now(),
            retrieval: r_tx,
            jobs: Arc::new(crate::daemon::worker::WorkerPool::new_inert_for_test()),
            memex_cache: MemexCache::new(),
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    #[tokio::test]
    async fn ping_returns_pong_then_done() {
        let state = test_state();
        let events = handle(Request::Ping { v: 1 }, &state).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::Pong { pid, .. } if *pid == 1234));
        assert!(matches!(&events[1], Event::Done { status: 0 }));
    }

    #[tokio::test]
    async fn unsupported_version_returns_version_mismatch() {
        let state = test_state();
        let events = handle(Request::Ping { v: 999 }, &state).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::Error { code, .. } if code == "version_mismatch"));
        assert!(matches!(&events[1], Event::Done { status: 1 }));
    }

    #[tokio::test]
    async fn query_raw_error_when_actor_down() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx); // actor has exited
        let state = HandlerState {
            pid: 0,
            started_at: Utc::now(),
            retrieval: tx,
            jobs: Arc::new(crate::daemon::worker::WorkerPool::new_inert_for_test()),
            memex_cache: MemexCache::new(),
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
        };
        let events = handle(
            Request::Query {
                v: 1,
                question: "q".into(),
                raw: true,
                top_k: 5,
                collections: vec![],
                memex_root: "/x".into(),
            },
            &state,
        )
        .await;
        assert!(matches!(&events[0], Event::Error { code, .. } if code == "internal"));
        assert!(matches!(&events[1], Event::Done { status: 1 }));
    }
}
