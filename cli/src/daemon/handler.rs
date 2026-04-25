//! Per-connection request handler.
//!
//! Called by `server.rs` for each accepted connection. Handles exactly one
//! request per connection and emits a stream of `Event`s terminated by a
//! `done` event.

use crate::daemon::error::DaemonError;
use crate::daemon::memex_cache::MemexCache;
use crate::daemon::protocol::{Event, Request};
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
    pub config: Arc<crate::daemon::config::Config>,
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
            memex_root,
        } => {
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
            source,
            collections,
            memex_root,
        } => match source {
            crate::daemon::protocol::IngestSource::Transcript { path, agent } => {
                handle_ingest_transcript(
                    path,
                    agent.as_str().to_string(),
                    collections,
                    memex_root,
                    state,
                )
                .await
            }
            crate::daemon::protocol::IngestSource::Document {
                source_path,
                content,
            } => {
                handle_ingest_document(source_path, content, collections, memex_root, state).await
            }
        },

        Request::Write {
            title,
            content,
            tags,
            source,
            force,
            memex_root,
        } => {
            handle_write(title, content, tags, source, force, memex_root, state).await
        }

        Request::SourceAdd {
            source_path,
            content,
            collections,
            memex_root,
        } => {
            handle_source_add(source_path, content, collections, memex_root, state).await
        }

        Request::SourceDelete {
            ref_,
            force,
            memex_root,
        } => handle_source_delete(ref_, force, memex_root, state).await,

        // Delete, LintFix — stubs
        Request::Delete { .. }
        | Request::LintFix { .. } => {
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
    source: Option<String>,
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

    // Resolve --source <docid> if provided, then inject it into frontmatter.
    let final_content = if let Some(source_docid) = source.as_deref() {
        // 1. Cheap docid format check.
        let looks_like_docid = source_docid.starts_with("src-")
            || source_docid.starts_with("source-")
            || source_docid.starts_with("wiki-")
            // 6-char hex docids (allocate_docid output) — accept too.
            || (source_docid.len() >= 6
                && source_docid.chars().all(|c| c.is_ascii_hexdigit()));
        if !looks_like_docid {
            return error_events(DaemonError::BadRequest(format!(
                "--source value '{source_docid}' is not a docid (expected 'src-...' or 'source-...'). \
                 To attach a local file, run `memex source add` first to get a docid."
            )));
        }
        // 2. Resolve to confirm the source exists and get its source_path.
        let docs = match search.resolve_ref_documents(source_docid) {
            Ok(d) => d,
            Err(e) => {
                return error_events(DaemonError::Internal(format!("resolve source: {e}")));
            }
        };
        let source_doc = match docs.iter().find(|d| d.doc_type == "source") {
            Some(d) => d.clone(),
            None => {
                return error_events(DaemonError::BadRequest(format!(
                    "--source docid '{source_docid}' not found. Run `memex source add` first."
                )));
            }
        };
        // 3. Inject `sources: [<source_path>]` into the page's frontmatter.
        inject_source_into_frontmatter(&content, &source_doc.path)
    } else {
        content.clone()
    };

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
    let hash = match search.insert_content(&final_content) {
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
    if let Err(e) = async_atomic_write(page_path, final_content.as_bytes().to_vec()).await {
        return error_events(e);
    }

    // Embed wiki page + sources. Load model once for all.
    let mut model = match memex_core::retrieval::load_default_model() {
        Ok(m) => m,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("{e}")));
        }
    };
    if let Err(e) = memex_core::retrieval::embed_document(search, &hash, &final_content, &mut model)
    {
        return error_events(DaemonError::Internal(format!("embed wiki page: {e}")));
    }

    vec![Event::Written { slug, docid }, Event::Done { status: 0 }]
}

/// Append `<source_path>` to the page's `sources:` frontmatter list. If the
/// page has no frontmatter, prepend a minimal frontmatter block that includes it.
/// Idempotent — duplicate entries are not added. Falls back to a textual
/// inject (preserving the existing frontmatter verbatim) if the frontmatter
/// can't be parsed by the strict `PageFrontmatter` schema — agent-authored
/// pages frequently omit `created_at`/`updated_at`.
fn inject_source_into_frontmatter(content: &str, source_path: &str) -> String {
    let trimmed = content.trim_start();
    let has_frontmatter = trimmed.starts_with("---");
    if !has_frontmatter {
        return format!(
            "---\nsources:\n  - {}\n---\n\n{}",
            yaml_quote(source_path),
            content
        );
    }
    // Try strict parse first so we can de-dup against existing entries.
    if let Ok((mut fm, body)) = memex_core::validate::parse_frontmatter(content) {
        if !fm.sources.iter().any(|s| s == source_path) {
            fm.sources.push(source_path.to_string());
        }
        if let Ok(yaml) = serde_yaml::to_string(&fm) {
            return format!("---\n{yaml}---\n\n{}", body.trim_start());
        }
    }
    // Fallback: text-level inject without parsing the YAML schema. Splits
    // on the closing `---` and appends a `sources:` entry just before it.
    inject_source_textual(content, source_path)
}

/// Best-effort textual injection of a `sources:` entry into the YAML
/// frontmatter block. Used when the strict `PageFrontmatter` schema fails
/// (commonly because `created_at`/`updated_at` are missing in agent-emitted
/// drafts). Preserves the existing frontmatter bytes verbatim and only
/// rewrites the `sources:` field.
fn inject_source_textual(content: &str, source_path: &str) -> String {
    // Locate frontmatter delimiters on their own lines.
    let after_first = match content.strip_prefix("---\n").or_else(|| content.strip_prefix("---")) {
        Some(rest) => rest,
        None => return content.to_string(),
    };
    let close_idx = match after_first.find("\n---") {
        Some(i) => i,
        None => return content.to_string(),
    };
    let yaml_block = &after_first[..close_idx];
    // After the closing `---` line, the body follows on the next line.
    let after_close = &after_first[close_idx + "\n---".len()..];
    let body = after_close.strip_prefix('\n').unwrap_or(after_close);

    // Detect existing sources: entries that contain `source_path`. Cheap
    // heuristic: substring match. Avoids duplicate writes on retry.
    if yaml_block.contains(source_path) {
        return content.to_string();
    }

    // Append `sources:\n  - <quoted>` to the YAML block. If `sources:` is
    // already present, just add another item under it.
    let mut new_yaml = String::with_capacity(yaml_block.len() + 64);
    new_yaml.push_str(yaml_block.trim_end_matches('\n'));
    if !new_yaml.ends_with('\n') {
        new_yaml.push('\n');
    }
    if yaml_block.lines().any(|l| l.trim_start().starts_with("sources:")) {
        new_yaml.push_str(&format!("  - {}\n", yaml_quote(source_path)));
    } else {
        new_yaml.push_str(&format!("sources:\n  - {}\n", yaml_quote(source_path)));
    }
    format!("---\n{new_yaml}---\n{body}")
}

/// YAML scalar quoting — wrap in double quotes if the value contains
/// anything other than `[a-zA-Z0-9./-:_]`.
fn yaml_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '-' | ':' | '_'))
    {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

async fn handle_source_add(
    source_path: String,
    content: String,
    collections: Vec<String>,
    memex_root: String,
    state: &HandlerState,
) -> Vec<Event> {
    use std::path::PathBuf;

    if let Err(e) = validate_source_path(&source_path) {
        return error_events(DaemonError::BadRequest(e));
    }
    let cfg = state.config.ingest.clone();
    let redacted = match validate_and_redact_inbound_content(&content, cfg.fetch_max_bytes) {
        Ok(r) => r,
        Err(e) => return error_events(e),
    };

    let root = PathBuf::from(&memex_root);
    let memex = match get_or_open_memex(&state.memex_cache, &root) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();
    let effective_collections = memex_core::search::normalize_collections(&collections);

    // Store content; idempotent on repeated identical content.
    let hash = match search.insert_content(&redacted) {
        Ok(h) => h,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("insert_content: {e}")));
        }
    };

    // Look up existing source document by hash; reuse docid if present.
    let existing_docid = search.lookup_source_docid_by_hash(&hash).unwrap_or(None);
    let is_new = existing_docid.is_none();
    let docid = if let Some(d) = existing_docid {
        d
    } else {
        let existing = search.existing_docids().unwrap_or_default();
        let existing_vec: Vec<String> = existing.into_iter().collect();
        let title = derive_source_title(&redacted, &source_path);
        let summary = memex_core::index::extract_summary(&redacted, 120);
        let now = memex_core::search::now_rfc3339();
        let docid =
            memex_core::docid::allocate_docid(&hash, "source", &source_path, &existing_vec);
        if let Err(e) = search.upsert_document(
            "source",
            &source_path,
            &title,
            &hash,
            &docid,
            "",
            &summary,
            &now,
            &now,
        ) {
            return error_events(DaemonError::Internal(format!("upsert_document: {e}")));
        }
        match memex_core::retrieval::load_default_model() {
            Ok(mut model) => {
                if let Err(e) =
                    memex_core::retrieval::embed_document(search, &hash, &redacted, &mut model)
                {
                    return error_events(DaemonError::Internal(format!("embed: {e}")));
                }
            }
            Err(e) => return error_events(DaemonError::Internal(format!("load_model: {e}"))),
        }
        docid
    };

    // Apply collections for new sources, and for re-adds with an explicit
    // --collection. Plain `memex source add` against existing content
    // leaves collections alone (idempotent on metadata).
    if is_new || !collections.is_empty() {
        if let Err(e) = search.set_document_collections_by_path(
            "source",
            &source_path,
            &effective_collections,
        ) {
            return error_events(DaemonError::Internal(format!(
                "set_document_collections_by_path: {e}"
            )));
        }
    }

    vec![Event::SourceAdded { docid }, Event::Done { status: 0 }]
}

async fn handle_source_delete(
    ref_: String,
    force: bool,
    memex_root: String,
    state: &HandlerState,
) -> Vec<Event> {
    use std::path::PathBuf;

    let root = PathBuf::from(&memex_root);
    let memex = match get_or_open_memex(&state.memex_cache, &root) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();

    // Resolve ref_ → source document.
    let source_doc = match resolve_source_ref(search, &ref_) {
        Ok(Some(d)) => d,
        Ok(None) => {
            return error_events(DaemonError::BadRequest(format!(
                "source not found: '{ref_}'. Use 'src-...' for docid or 'path:<source-path>'."
            )));
        }
        Err(e) => return error_events(DaemonError::Internal(format!("resolve: {e}"))),
    };

    // Find wiki pages referencing this source's path.
    let referencing: Vec<String> = search
        .wiki_pages_referencing_source(&source_doc.path)
        .unwrap_or_default();

    if !referencing.is_empty() && !force {
        return error_events(DaemonError::BadRequest(format!(
            "{} wiki pages reference this source: {}. Pass --force to delete anyway \
             (the references will become dangling and 'memex lint' will report them).",
            referencing.len(),
            referencing.join(", ")
        )));
    }

    // Delete the source document row + cleanup orphaned content.
    let path_str = source_doc.path.clone();
    let docid = source_doc.docid.clone();
    if let Err(e) = search.delete_document_with_cleanup(&path_str) {
        return error_events(DaemonError::Internal(format!("delete: {e}")));
    }

    vec![
        Event::SourceDeleted {
            docid,
            source_path: path_str,
            dangling_wiki_pages: referencing,
        },
        Event::Done { status: 0 },
    ]
}

/// Resolve a source ref ("src-..." docid or "path:<source-path>") to a source
/// document row. Returns Ok(None) if not found, Err on lookup error.
fn resolve_source_ref(
    search: &memex_core::search::Bm25Search,
    ref_: &str,
) -> Result<Option<memex_core::types::Document>, memex_core::error::MemexError> {
    if let Some(rest) = ref_.strip_prefix("path:") {
        return search.lookup_source_by_path(rest);
    }
    let docs = search.resolve_ref_documents(ref_)?;
    Ok(docs.into_iter().find(|d| d.doc_type == "source"))
}

/// First H1 line, falling back to URL last segment / file stem / verbatim.
fn derive_source_title(content: &str, source_path: &str) -> String {
    if let Some(line) = content.lines().next() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let t = rest.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if (source_path.starts_with("http://") || source_path.starts_with("https://"))
        && let Some(stem) = source_path.rsplit('/').find(|s| !s.is_empty())
    {
        return crate::slugify(stem);
    }
    if let Some(stem) = std::path::Path::new(source_path)
        .file_stem()
        .and_then(|s| s.to_str())
    {
        return stem.to_string();
    }
    source_path.to_string()
}

/// Handle an ingest request: validate, dedup, parse, filter, dispatch to
/// worker, dedup search, optional merge, validate output, store atomically.
async fn handle_ingest_transcript(
    transcript_path: String,
    agent: String,
    collections: Vec<String>,
    memex_root: String,
    state: &HandlerState,
) -> Vec<Event> {
    use crate::daemon::queue::{BackendJob, IngestJob};
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

    let raw_content =
        match read_file_capped(&path, crate::daemon::config::INGEST_MAX_BYTES as u64).await {
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
        memex_core::search::JobType::Transcript,
        &transcript_path,
        Some(&agent),
        &content_hash,
        &memex_root,
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

    let stored_event = match store_extracted_pages(
        extracted.pages,
        &canonical_transcript,
        &transcript_path,
        &title,
        &summary,
        &effective_collections,
        &memex_root,
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
        transcript = %transcript_path,
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "ingest job completed"
    );

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
    source_summary: &str,
    collections: &[String],
    memex_root: &str,
    job_id: &str,
    state: &HandlerState,
) -> Result<Event, Vec<Event>> {
    use crate::daemon::queue::{BackendJob, MergeJob};
    use std::path::PathBuf;

    let root_path = PathBuf::from(memex_root);
    let memex = match get_or_open_memex(&state.memex_cache, &root_path) {
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

    // 2. Acquire per-slug write locks.
    let lock_slugs: Vec<String> = valid_pages.iter().map(|p| p.slug.clone()).collect();
    let _slug_guards = acquire_slug_locks(state, memex_root, lock_slugs).await;

    // 3. Load embedding model once for dedup-search reranking + final embed.
    let mut model = match memex_core::retrieval::load_default_model() {
        Ok(m) => m,
        Err(e) => {
            let _ = search.update_ingest_job_status(job_id, "failed", Some(&e.to_string()));
            return Err(error_events(DaemonError::Internal(format!("{e}"))));
        }
    };

    // 4. Dedup search against existing wiki pages.
    let (mut new_pages, merge_pairs) =
        match dedup_against_existing(search, &wiki_dir, &valid_pages, &mut model).await {
            Ok(r) => r,
            Err(e) => {
                let _ = search.update_ingest_job_status(job_id, "failed", Some(&e.to_string()));
                return Err(error_events(e));
            }
        };

    tracing::info!(
        new = new_pages.len(),
        merge = merge_pairs.len(),
        "dedup search complete"
    );

    // 5. Optional wiki-side merge.
    let mut merged_pages: Vec<crate::daemon::queue::ExtractedPage> = Vec::new();
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

    // 6. Build wiki records.
    let now_dt = chrono::Utc::now();
    let now = now_dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let all_pages: Vec<_> = new_pages.iter().chain(merged_pages.iter()).collect();
    let wiki_pages =
        build_wiki_records(&all_pages, &wiki_dir, source_path, collections, now_dt).await;

    // 7. Transactional store.
    let batch_result = match search.store_ingest_batch(
        source_text,
        source_path,
        source_title,
        source_summary,
        &wiki_pages,
        collections,
        &now,
    ) {
        Ok(r) => r,
        Err(e) => {
            let _ = search.update_ingest_job_status(job_id, "failed", Some(&e.to_string()));
            return Err(error_events(DaemonError::Internal(format!(
                "storage failed: {e}"
            ))));
        }
    };

    // 8. File writes after DB commit. Failures are logged; lint detects gaps.
    write_wiki_files(&wiki_dir, &wiki_pages).await;

    // 9. Embed source + wiki pages.
    if let Err(e) = embed_ingested(search, &batch_result, source_text, &wiki_pages, &mut model) {
        let _ = search.update_ingest_job_status(job_id, "failed", Some(&e.to_string()));
        return Err(error_events(e));
    }

    Ok(Event::Stored {
        job_id: job_id.to_string(),
        source_docid: batch_result.source_docid,
        wiki_pages: batch_result.wiki_hashes.into_iter().map(|(s, _)| s).collect(),
    })
}

/// Handle a document-ingest request. Validates content, hashes it for
/// dedup, chunks long documents at H1/H2 boundaries, dispatches one Extract
/// per chunk concurrently, fragment-merges pages that share a slug across
/// chunks, then hands off to the shared post-Extract pipeline.
async fn handle_ingest_document(
    source_path: String,
    content: String,
    collections: Vec<String>,
    memex_root: String,
    state: &HandlerState,
) -> Vec<Event> {
    use crate::daemon::queue::{
        BackendJob, ChunkPosition, ExtractSegment, IngestJob, MergeJob, MergePair,
    };
    use std::path::PathBuf;

    let cfg = state.config.ingest.clone();

    // ─── Validation ───────────────────────────────────────────────────
    if let Err(e) = validate_source_path(&source_path) {
        return error_events(DaemonError::BadRequest(e));
    }
    let redacted = match validate_and_redact_inbound_content(&content, cfg.fetch_max_bytes) {
        Ok(r) => r,
        Err(e) => return error_events(e),
    };

    // ─── Open memex + dedup on content hash ───────────────────────────
    let root_path = PathBuf::from(&memex_root);
    let memex = match get_or_open_memex(&state.memex_cache, &root_path) {
        Ok(m) => m,
        Err(e) => return error_events(e),
    };
    let search = memex.search();
    let effective_collections = memex_core::search::normalize_collections(&collections);

    let content_hash = memex_core::storage::content_hash(redacted.as_bytes());
    if search.content_exists(&content_hash).unwrap_or(false) {
        return vec![Event::Done { status: 0 }];
    }

    // ─── Job ID + persist content + persist job row (pre-Extract) ────
    let job_id = format!(
        "doc-{}",
        memex_core::storage::content_hash(
            format!("{source_path}\0{content_hash}").as_bytes()
        )
    );
    if let Err(e) = search.insert_content(&redacted) {
        return error_events(DaemonError::Internal(format!("insert_content: {e}")));
    }
    if let Err(e) = search.insert_ingest_job(
        &job_id,
        memex_core::search::JobType::Document,
        &source_path,
        None,
        &content_hash,
        &memex_root,
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
        &memex_root,
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

/// Reject source identifiers that are too long or contain control chars
/// that would corrupt frontmatter / logs / IPC line framing.
fn validate_source_path(s: &str) -> Result<(), String> {
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
fn validate_and_redact_inbound_content(
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

    fn test_state() -> HandlerState {
        let (r_tx, _r_rx) = tokio::sync::mpsc::channel(1);
        HandlerState {
            pid: 1234,
            started_at: Utc::now(),
            retrieval: r_tx,
            jobs: Arc::new(crate::daemon::worker::WorkerPool::new_inert_for_test()),
            memex_cache: MemexCache::new(),
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
            config: Arc::new(crate::daemon::config::Config::default()),
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
        let state = HandlerState {
            pid: 0,
            started_at: Utc::now(),
            retrieval: tx,
            jobs: Arc::new(crate::daemon::worker::WorkerPool::new_inert_for_test()),
            memex_cache: MemexCache::new(),
            slug_locks: Arc::new(StdMutex::new(HashMap::new())),
            config: Arc::new(crate::daemon::config::Config::default()),
        };
        let events = handle(
            Request::Query {
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

    #[test]
    fn source_path_validator_accepts_url() {
        super::validate_source_path("https://example.com/post").unwrap();
    }

    #[test]
    fn source_path_validator_accepts_filesystem_path() {
        super::validate_source_path("/abs/path/to/file.md").unwrap();
    }

    #[test]
    fn source_path_validator_rejects_empty() {
        assert!(super::validate_source_path("").is_err());
    }

    #[test]
    fn source_path_validator_rejects_overlong() {
        let s = "a".repeat(2049);
        assert!(super::validate_source_path(&s).is_err());
    }

    #[test]
    fn source_path_validator_rejects_newline() {
        assert!(super::validate_source_path("https://x/p\ninjected").is_err());
    }

    #[test]
    fn source_path_validator_rejects_null() {
        assert!(super::validate_source_path("path\0null").is_err());
    }

    #[test]
    fn source_path_validator_accepts_tab() {
        super::validate_source_path("a\tb").unwrap();
    }

    #[test]
    fn validate_and_redact_passes_normal_content() {
        let r = super::validate_and_redact_inbound_content("# Title\n\nbody", 10_000).unwrap();
        assert!(r.contains("# Title"));
        assert!(r.contains("body"));
    }

    #[test]
    fn validate_and_redact_rejects_oversize() {
        let big = "a".repeat(1001);
        let err = super::validate_and_redact_inbound_content(&big, 1000).unwrap_err();
        assert!(matches!(err, super::DaemonError::BadRequest(_)));
    }

    #[test]
    fn validate_and_redact_rejects_post_redaction_empty() {
        // A 50-char string of all whitespace — empty after trim post-redaction.
        let r = super::validate_and_redact_inbound_content("   \n\t  \n  ", 1000);
        assert!(r.is_err(), "expected empty-content error, got: {r:?}");
    }

    #[test]
    fn validate_and_redact_strips_secrets_in_output() {
        let content = "see token sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA in body";
        let r = super::validate_and_redact_inbound_content(content, 10_000).unwrap();
        assert!(!r.contains("sk-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"), "got: {r}");
    }
}
