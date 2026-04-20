//! Per-connection request handler.
//!
//! Called by `server.rs` for each accepted connection. Handles exactly one
//! request per connection and emits a stream of `Event`s terminated by a
//! `done` event.

use crate::daemon::error::DaemonError;
use crate::daemon::protocol::{Event, Request, SUPPORTED_VERSIONS};
use chrono::Utc;
use std::sync::Arc;

/// Daemon-shared state accessible to handlers.
pub struct HandlerState {
    pub pid: u32,
    pub started_at: chrono::DateTime<Utc>,
    pub retrieval: crate::daemon::retrieval::RetrievalSender,
    pub jobs: Arc<crate::daemon::worker::WorkerPool>,
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
            let send_result = state
                .retrieval
                .send(RetrievalReq {
                    memex_root: memex_root.clone().into(),
                    question: question.clone(),
                    top_k,
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

            if raw {
                return vec![
                    Event::Context {
                        pages: retrieval_resp
                            .pages
                            .into_iter()
                            .map(|p| serde_json::to_value(p).unwrap_or(serde_json::Value::Null))
                            .collect(),
                    },
                    Event::Done { status: 0 },
                ];
            }

            // Synthesis path. If the probe is weak, try expansion first.
            use crate::daemon::context as ctx_fmt;
            use crate::daemon::queue::{AgentJob, ExpandJob, SynthJob, WorkerError};
            use crate::daemon::retrieval::ExpansionTerms;

            // Cache the initial pages/signal so fallback branches can reuse them.
            let initial_signal = retrieval_resp.signal;
            let initial_pages = retrieval_resp.pages;

            let mut events_pre: Vec<Event> = Vec::new();
            let pages_for_ctx = if matches!(initial_signal, memex_core::retrieval::Signal::Weak) {
                // Enqueue ExpandJob. Fall back to initial pages on any error.
                let (etx, erx) = tokio::sync::oneshot::channel();
                let expand_sent = state
                    .jobs
                    .submit(AgentJob::Expand(ExpandJob {
                        question: question.clone(),
                        reply: etx,
                    }))
                    .await;
                if expand_sent.is_err() {
                    tracing::warn!("expand queue closed; falling back to un-expanded retrieval");
                    initial_pages
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
                                initial_pages
                            } else {
                                match rx2.await {
                                    Ok(Ok(r)) => r.pages,
                                    Ok(Err(e)) => {
                                        tracing::warn!(
                                            ?e,
                                            "expanded retrieval failed; falling back"
                                        );
                                        initial_pages
                                    }
                                    Err(_) => {
                                        tracing::warn!(
                                            "expanded retrieval actor dropped reply; falling back"
                                        );
                                        initial_pages
                                    }
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!(
                                ?e,
                                "expand job failed; falling back to un-expanded retrieval"
                            );
                            initial_pages
                        }
                        Err(_) => {
                            tracing::warn!("expand worker dropped reply; falling back");
                            initial_pages
                        }
                    }
                }
            } else {
                initial_pages
            };

            let context = ctx_fmt::format(&pages_for_ctx);
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let job = AgentJob::Synth(SynthJob {
                context,
                question: question.clone(),
                reply: reply_tx,
            });
            if state.jobs.submit(job).await.is_err() {
                return error_events(DaemonError::Internal("worker queue closed".into()));
            }
            let synth_reply = match reply_rx.await {
                Ok(Ok(r)) => r,
                Ok(Err(WorkerError::Crash(msg))) => {
                    tracing::warn!(%msg, "worker subprocess crashed");
                    return error_events(DaemonError::SubprocessCrashed);
                }
                Ok(Err(WorkerError::Timeout)) => {
                    return error_events(DaemonError::SubprocessTimeout);
                }
                Ok(Err(WorkerError::AuthFailed(msg))) => {
                    tracing::warn!(%msg, "agent auth failed");
                    return error_events(DaemonError::AuthFailed);
                }
                Ok(Err(WorkerError::AgentError(msg))) => {
                    return error_events(DaemonError::AgentUnavailable(msg));
                }
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
            memex_root,
        } => {
            if !SUPPORTED_VERSIONS.contains(&v) {
                return error_events(DaemonError::VersionMismatch {
                    supported: SUPPORTED_VERSIONS.to_vec(),
                });
            }
            handle_ingest(transcript_path, agent, memex_root, state).await
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
            handle_write(title, content, tags, sources, force, memex_root).await
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
) -> Vec<Event> {
    use std::path::PathBuf;

    if title.trim().is_empty() {
        return error_events(DaemonError::BadRequest("empty title".into()));
    }
    if content.trim().is_empty() {
        return error_events(DaemonError::BadRequest("empty content".into()));
    }

    let root_path = PathBuf::from(&memex_root);
    let memex = match memex_core::Memex::open(root_path) {
        Ok(m) => m,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("cannot open memex: {e}")));
        }
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
        return error_events(DaemonError::Internal(format!("upsert_document failed: {e}")));
    }

    // Write markdown file
    let wiki_dir = memex.wiki_dir();
    let _ = std::fs::create_dir_all(&wiki_dir);
    let page_path = wiki_dir.join(format!("{slug}.md"));
    if let Err(e) = std::fs::write(&page_path, &content) {
        return error_events(DaemonError::Internal(format!("write file failed: {e}")));
    }

    // Embed wiki page + sources. Load model once for all.
    if let Some(ref mut model) = memex_core::retrieval::load_default_model() {
        memex_core::retrieval::embed_document(search, &hash, &content, model);

        for source_path_str in &sources {
            let source_path = std::path::PathBuf::from(source_path_str);
            if !source_path.exists() {
                eprintln!("warning: source not found: {source_path_str} (skipping)");
                continue;
            }
            if let Ok(source_path) = std::fs::canonicalize(&source_path)
                && let Ok(source_content) = std::fs::read_to_string(&source_path)
            {
                let source_abs = source_path.to_string_lossy().to_string();
                let source_title = source_path.file_stem().unwrap_or_default().to_string_lossy().to_string();
                let source_summary = memex_core::index::extract_summary(&source_content, 120);
                let source_hash = search.insert_content(&source_content).unwrap_or_default();
                let source_docid = memex_core::docid::allocate_docid(&source_hash, "source", &source_abs, &[]);
                let _ = search.upsert_document("source", &source_abs, &source_title, &source_hash, &source_docid, "", &source_summary, &now, &now);
                memex_core::retrieval::embed_document(search, &source_hash, &source_content, model);
            }
        }
    }

    vec![
        Event::Written { slug, docid },
        Event::Done { status: 0 },
    ]
}

/// Handle an ingest request: validate, dedup, parse, filter, dispatch to worker,
/// dedup search, optional merge, validate output, store atomically.
/// Implements spec sections 3.1-3.9.
async fn handle_ingest(
    transcript_path: String,
    agent: String,
    memex_root: String,
    state: &HandlerState,
) -> Vec<Event> {
    use crate::daemon::queue::{AgentJob, IngestJob, MergeJob, MergePair, WorkerError};
    use memex_core::transcript::SessionFilter;
    use std::hash::{Hash, Hasher};
    use std::path::PathBuf;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    transcript_path.hash(&mut hasher);
    let job_id = format!("ingest-{:x}", hasher.finish());

    // 3.1 Path validation: must be absolute
    let path = PathBuf::from(&transcript_path);
    if !path.is_absolute() {
        return error_events(DaemonError::BadRequest(
            "transcript_path must be absolute".into(),
        ));
    }

    // 3.1 Read file for dedup + parsing (cap at 50MB to prevent OOM)
    const MAX_TRANSCRIPT_BYTES: usize = 50 * 1024 * 1024;
    let raw_content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            return error_events(DaemonError::BadRequest(format!(
                "cannot read transcript: {e}"
            )));
        }
    };
    if raw_content.len() > MAX_TRANSCRIPT_BYTES {
        return error_events(DaemonError::BadRequest(format!(
            "transcript too large: {} bytes (max {})",
            raw_content.len(),
            MAX_TRANSCRIPT_BYTES
        )));
    }

    // Open memex once, used for dedup + job queue + storage
    let content_hash = memex_core::storage::content_hash(raw_content.as_bytes());
    let root_path = PathBuf::from(&memex_root);
    let memex = match memex_core::Memex::open(root_path) {
        Ok(m) => m,
        Err(e) => {
            return error_events(DaemonError::Internal(format!("cannot open memex: {e}")));
        }
    };
    let search = memex.search();

    // 3.1 Content-hash dedup
    if search.content_exists(&content_hash).unwrap_or(false) {
        return vec![Event::Done { status: 0 }]; // already ingested
    }

    // 3.2 Parse and clean
    let transcript = match agent.as_str() {
        "claude-code" => {
            memex_core::transcript::parse_claude_code_session(std::io::BufReader::new(
                raw_content.as_bytes(),
            ))
        }
        "codex" => memex_core::transcript::parse_codex_session(std::io::BufReader::new(
            raw_content.as_bytes(),
        )),
        "gemini-cli" => memex_core::transcript::parse_gemini_cli_session(&raw_content),
        _ => {
            return error_events(DaemonError::BadRequest(format!(
                "unknown agent: {agent}"
            )));
        }
    };

    let transcript = match transcript {
        Ok(t) => t,
        Err(e) => {
            return error_events(DaemonError::BadRequest(format!("parse error: {e}")));
        }
    };

    // 3.3 Filter
    match transcript.filter {
        SessionFilter::Pass => {} // continue
        SessionFilter::NonSubstantive | SessionFilter::InternalSession => {
            return vec![Event::Done { status: 0 }]; // skip silently
        }
    }

    // 3.4 Durable job queue: persist before LLM dispatch.
    // INSERT OR IGNORE dedupes concurrent ingests of the same transcript.
    if let Err(e) = search.insert_ingest_job(&job_id, &transcript_path, &content_hash, &agent, &memex_root) {
        return error_events(DaemonError::Internal(format!("insert_ingest_job failed: {e}")));
    }

    // Emit progress
    let mut events = vec![Event::Parsing {
        job_id: job_id.clone(),
        transcript_path: transcript_path.clone(),
    }];

    // 3.5 Dispatch Extract job to worker pool
    events.push(Event::Distilling {
        job_id: job_id.clone(),
        transcript_path: transcript_path.clone(),
    });

    let (extract_tx, extract_rx) = tokio::sync::oneshot::channel();
    let extract_job = AgentJob::Ingest(IngestJob {
        transcript: transcript.cleaned_text.clone(),
        reply: extract_tx,
    });

    if state.jobs.submit(extract_job).await.is_err() {
        let _ = search.update_ingest_job_status(&job_id, "failed", Some("worker queue closed"));
        return error_events(DaemonError::Internal("worker queue closed".into()));
    }

    let extracted = match extract_rx.await {
        Ok(Ok(reply)) => reply,
        Ok(Err(WorkerError::Crash(_))) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some("subprocess crashed"));
            return error_events(DaemonError::SubprocessCrashed);
        }
        Ok(Err(WorkerError::Timeout)) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some("subprocess timeout"));
            return error_events(DaemonError::SubprocessTimeout);
        }
        Ok(Err(WorkerError::AuthFailed(_))) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some("auth failed"));
            return error_events(DaemonError::AuthFailed);
        }
        Ok(Err(WorkerError::AgentError(msg))) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some(&msg));
            return error_events(DaemonError::AgentUnavailable(msg));
        }
        Err(_) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some("worker dropped reply"));
            return error_events(DaemonError::Internal("worker dropped reply".into()));
        }
    };

    if extracted.pages.is_empty() {
        let _ = search.update_ingest_job_status(&job_id, "completed", None);
        events.push(Event::Done { status: 0 });
        return events;
    }

    // 3.8 Validate LLM output
    let mut valid_pages = Vec::new();
    for page in extracted.pages.into_iter().take(10) {
        if page.slug.is_empty() || page.title.is_empty() || page.body.is_empty() {
            continue;
        }
        let slug = crate::slugify(&page.slug);
        if slug.is_empty() {
            continue;
        }
        valid_pages.push(crate::daemon::queue::ExtractedPage {
            slug,
            title: page.title,
            tags: page.tags,
            body: memex_core::transcript::truncate(&page.body, 20_000),
        });
    }

    let _ = search.update_ingest_job_status(&job_id, "processing", None);

    // 3.6 Dedup search: for each proposed page, check if a wiki page with
    // overlapping topic already exists. Title-only BM25 + vector reranking.
    let mut model = memex_core::retrieval::load_default_model();
    let mut new_pages = Vec::new();
    let mut merge_pairs = Vec::new();

    for page in &valid_pages {
        let existing_slug = memex_core::retrieval::search_wiki_by_title(search, &page.title, model.as_mut());
        tracing::info!(title = %page.title, result = ?existing_slug, "dedup search");
        match existing_slug {
            Some(slug) => {
                let wiki_dir = memex.wiki_dir();
                let existing_path = wiki_dir.join(format!("{slug}.md"));
                tracing::info!(slug = %slug, path = %existing_path.display(), "reading existing page for merge");
                if let Ok(existing_content) = std::fs::read_to_string(&existing_path) {
                    merge_pairs.push(MergePair {
                        slug,
                        proposed: page.body.clone(),
                        existing: existing_content,
                    });
                } else {
                    new_pages.push(page.clone());
                }
            }
            None => {
                new_pages.push(page.clone());
            }
        }
    }

    // 3.7 Merge: dispatch MergeJob if there are pages to merge
    tracing::info!(new = new_pages.len(), merge = merge_pairs.len(), "dedup search complete");
    let mut merged_pages = Vec::new();
    if !merge_pairs.is_empty() {
        let (merge_tx, merge_rx) = tokio::sync::oneshot::channel();
        let merge_job = AgentJob::Merge(MergeJob {
            pages: merge_pairs.clone(),
            reply: merge_tx,
        });
        if state.jobs.submit(merge_job).await.is_ok() {
            match merge_rx.await {
                Ok(Ok(reply)) => {
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
                Ok(Err(e)) => {
                    tracing::warn!(?e, "merge job failed; storing proposed pages as new");
                    // Fallback: store proposed content as new pages
                    for pair in &merge_pairs {
                        new_pages.push(crate::daemon::queue::ExtractedPage {
                            slug: pair.slug.clone(),
                            title: pair.slug.replace('-', " "),
                            tags: vec![],
                            body: pair.proposed.clone(),
                        });
                    }
                }
                Err(_) => {
                    tracing::warn!("merge worker dropped reply; storing proposed pages as new");
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
    }

    // 3.9 Store source + wiki pages in a single DB transaction, then write
    // files to disk. File writes are after COMMIT because they can't be
    // rolled back. If a file write fails, DB row exists but file is missing,
    // which `memex lint` detects.
    let now = memex_core::search::now_rfc3339();
    let title = memex_core::transcript::truncate(&transcript.first_user_message, 80);
    let summary = memex_core::index::extract_summary(&transcript.cleaned_text, 120);

    let all_pages: Vec<_> = new_pages.iter().chain(merged_pages.iter()).collect();
    let wiki_pages: Vec<(String, String, String, String)> = all_pages
        .iter()
        .map(|page| {
            let safe_title = page.title.replace('\n', " ").replace('\r', "");
            let content = format!(
                "---\ntitle: \"{}\"\nsummary: \"\"\ntags: [{}]\ncreated_at: {}\nupdated_at: {}\nsources: [{}]\n---\n\n{}",
                safe_title.replace('"', "\\\""),
                page.tags.iter().map(|t| {
                    let safe_tag = t.replace('\\', "\\\\").replace('"', "\\\"");
                    format!("\"{safe_tag}\"")
                }).collect::<Vec<_>>().join(", "),
                &now, &now, &transcript_path, page.body
            );
            (page.slug.clone(), page.title.clone(), content, page.tags.join(","))
        })
        .collect();

    let (cleaned_hash, source_docid, wiki_page_slugs) = match search.store_ingest_batch(
        &transcript.cleaned_text,
        &transcript_path,
        &title,
        &summary,
        &wiki_pages,
        &now,
    ) {
        Ok(r) => r,
        Err(e) => {
            let _ = search.update_ingest_job_status(&job_id, "failed", Some(&e.to_string()));
            return error_events(DaemonError::Internal(format!("storage failed: {e}")));
        }
    };

    // Write wiki files to disk (after DB commit).
    let wiki_dir = memex.wiki_dir();
    let _ = std::fs::create_dir_all(&wiki_dir);
    for (slug, _title, content, _tags) in &wiki_pages {
        let page_path = wiki_dir.join(format!("{slug}.md"));
        if let Err(e) = std::fs::write(&page_path, content) {
            tracing::warn!(slug = %slug, ?e, "failed to write wiki page file");
        }
    }

    // Embed source + wiki pages. Reuse the model loaded for dedup.
    if let Some(ref mut m) = model {
        memex_core::retrieval::embed_document(search, &cleaned_hash, &transcript.cleaned_text, m);
        for (slug, _title, content, _tags) in &wiki_pages {
            let page_hash = memex_core::storage::content_hash(content.as_bytes());
            memex_core::retrieval::embed_document(search, &page_hash, content, m);
            tracing::debug!(slug = %slug, "embedded wiki page");
        }
    }

    let _ = search.update_ingest_job_status(&job_id, "completed", None);

    events.push(Event::Stored {
        job_id,
        source_docid: source_docid.clone(),
        wiki_pages: wiki_page_slugs,
    });
    events.push(Event::Done { status: 0 });
    events
}

fn error_events(err: DaemonError) -> Vec<Event> {
    let status = err.exit_code();
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
        };
        let events = handle(
            Request::Query {
                v: 1,
                question: "q".into(),
                raw: true,
                top_k: 5,
                memex_root: "/x".into(),
            },
            &state,
        )
        .await;
        assert!(matches!(&events[0], Event::Error { code, .. } if code == "internal"));
        assert!(matches!(&events[1], Event::Done { status: 1 }));
    }
}
