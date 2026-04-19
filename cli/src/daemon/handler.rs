//! Per-connection request handler.
//!
//! Called by `server.rs` for each accepted connection. Handles exactly one
//! request per connection and emits a stream of `Event`s terminated by a
//! `done` event. Plan 1: `ping` only; `query` returns `not_implemented`.

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
                    tracing::warn!(%msg, "claude auth failed");
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
    }
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
