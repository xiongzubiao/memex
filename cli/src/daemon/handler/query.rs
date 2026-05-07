//! `Request::Query` handler — dispatches to the retrieval actor, runs
//! optional expansion on weak signal, and either returns raw context or
//! synthesizes an answer via the worker pool.

use crate::daemon::context as ctx_fmt;
use crate::daemon::error::DaemonError;
use crate::daemon::handler::{HandlerState, error_events, filter_hallucinated};
use crate::daemon::protocol::Event;
use crate::daemon::queue::{BackendJob, ExpandJob, SynthJob};
use crate::daemon::retrieval::{ExpansionTerms, RetrievalError, RetrievalReq};

/// Hard cap on `top_k` from a query request. Each retrieved chunk gets
/// its body slice attached and forwarded into the synthesis prompt or
/// raw context JSON, so an unbounded `top_k` is a prompt-size /
/// response-size amplification vector — a single client request asking
/// for `top_k=100000` would force the daemon to read every chunk in the
/// index, then ship the whole corpus to the answerer LLM. The largest
/// realistic use case (LoCoMo eval at top_200) is well under this cap;
/// 1000 leaves headroom for unusual workloads while still bounding the
/// blast radius.
const MAX_TOP_K: usize = 1000;

pub(super) async fn handle_query(
    question: String,
    raw: bool,
    top_k: usize,
    collections: Vec<String>,
    intent: Option<String>,
    state: &HandlerState,
) -> Vec<Event> {
    // Reject empty/whitespace-only questions outright. Without this
    // guard the retrieval pipeline runs with an empty BM25 query and
    // a zero-vector embedding — BM25 returns nothing but the vector
    // search returns ALL chunks (every vector is equally close to
    // zero), so the user gets a flood of unrelated results back. A
    // CLI typo (`memex query ""`) shouldn't dump the entire wiki.
    if question.trim().is_empty() {
        return error_events(DaemonError::BadRequest(
            "question is empty; provide a non-empty query".into(),
        ));
    }
    if let Err(e) = memex_core::search::validate_collection_names(&collections) {
        return error_events(DaemonError::BadRequest(e));
    }
    let top_k = top_k.min(MAX_TOP_K);

    // Retrieval: dispatch to the retrieval actor. Shared by raw + synth.
    let memex_root = state.reader().bound_root.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let send_result = state
        .retrieval
        .send(RetrievalReq {
            memex_root: memex_root.clone(),
            question: question.clone(),
            top_k,
            collections: collections.clone(),
            intent: intent.clone(),
            expansion: None,
            reply: tx,
        })
        .await;
    if send_result.is_err() {
        return error_events(DaemonError::Internal("retrieval actor unavailable".into()));
    }
    let retrieval_timeout =
        std::time::Duration::from_secs(state.config.query.retrieval_timeout_sec);
    let retrieval_resp = match tokio::time::timeout(retrieval_timeout, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => {
            return error_events(DaemonError::Internal(
                "retrieval actor dropped reply".into(),
            ));
        }
        Err(_) => {
            return error_events(DaemonError::Internal(format!(
                "retrieval timed out after {}s",
                state.config.query.retrieval_timeout_sec
            )));
        }
    };
    let retrieval_resp = match retrieval_resp {
        Ok(r) => r,
        Err(RetrievalError::Empty { collections }) => {
            return error_events(DaemonError::RetrievalEmpty { collections });
        }
        Err(RetrievalError::InvalidRoot(msg)) => {
            return error_events(DaemonError::BadRequest(format!(
                "invalid memex_root: {msg}"
            )));
        }
        Err(RetrievalError::Other(e)) => {
            return error_events(DaemonError::Internal(e.to_string()));
        }
    };

    // Expansion runs on weak-signal probes before either raw or synth
    // returns, so both paths work from the same retrieval pipeline.
    // Synth then composes an answer; raw returns the expanded context.

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
                intent: intent.clone(),
                reply: etx,
            }))
            .await;
        if expand_sent.is_err() {
            tracing::warn!("expand queue closed; falling back to un-expanded retrieval");
            initial_entries
        } else {
            match erx.await {
                Ok(Ok(exp)) => {
                    // Drop any field that shares zero words with the
                    // user's query — a strong signal of LLM
                    // hallucination. (QMD llm.ts:1183 hasQueryTerm.)
                    let lex = filter_hallucinated(&exp.lex, &question);
                    let vec = filter_hallucinated(&exp.vec, &question);
                    let hyde = filter_hallucinated(&exp.hyde, &question);
                    tracing::info!(lex = %lex, vec = %vec, hyde = %hyde, "expansion terms received");
                    events_pre.push(Event::Expansion {
                        lex: lex.clone(),
                        vec: vec.clone(),
                        hyde: hyde.clone(),
                    });
                    // Re-retrieve with expansion.
                    let (tx2, rx2) = tokio::sync::oneshot::channel();
                    let send2 = state
                        .retrieval
                        .send(RetrievalReq {
                            memex_root: memex_root.clone(),
                            question: question.clone(),
                            top_k,
                            collections: collections.clone(),
                            intent: intent.clone(),
                            expansion: Some(ExpansionTerms { lex, vec, hyde }),
                            reply: tx2,
                        })
                        .await;
                    if send2.is_err() {
                        tracing::warn!("retrieval actor closed during expansion retry");
                        initial_entries
                    } else {
                        match tokio::time::timeout(retrieval_timeout, rx2).await {
                            Ok(Ok(Ok(r))) => r.entries,
                            Ok(Ok(Err(e))) => {
                                tracing::warn!(?e, "expanded retrieval failed; falling back");
                                initial_entries
                            }
                            Ok(Err(_)) => {
                                tracing::warn!(
                                    "expanded retrieval actor dropped reply; falling back"
                                );
                                initial_entries
                            }
                            Err(_) => {
                                tracing::warn!(
                                    "expanded retrieval timed out; falling back to initial entries"
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
        intent: intent.clone(),
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
