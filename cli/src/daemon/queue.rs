//! Agent job queue. Producers (connection handlers) send `AgentJob` onto a
//! bounded async-channel; consumers (worker tasks) pull jobs and run one LLM
//! turn per job.

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Reply from a worker for a synthesis job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthReply {
    pub answer: String,
    pub citations: Vec<String>,
}

/// Outcome of a worker's attempt to synthesize. Error variants are typed
/// so the handler can map them to the right `DaemonError` code.
#[derive(Debug)]
pub enum WorkerError {
    /// Subprocess crashed (EOF before terminal result, non-zero exit with
    /// no structured signal, write error). Retried once before surfacing.
    Crash(String),
    /// Subprocess read exceeded `daemon.worker.timeout_sec`. Retried once
    /// before surfacing.
    Timeout,
    /// Claude returned `is_error=true` with `error=authentication_failed`.
    /// Not retried — user needs to re-auth.
    AuthFailed(String),
    /// Claude returned `is_error=true` for any other reason, OR the
    /// assistant text didn't parse as our expected JSON reply. Raw text
    /// is surfaced so the user can see what Claude said.
    AgentError(String),
}

pub type SynthResult = Result<SynthReply, WorkerError>;

/// Synthesis job: context block + question → synthesized answer.
#[derive(Debug)]
pub struct SynthJob {
    /// Already-formatted context block (all retrieved pages, rank-tagged).
    pub context: String,
    /// The user's question, verbatim.
    pub question: String,
    /// Worker sends the result here.
    pub reply: oneshot::Sender<SynthResult>,
}

/// Reply from a worker for an expansion job. Each term is a single string
/// (one lex, one vec, one hyde) — matching the typed-flag semantics in
/// `memex search --lex --vec --hyde`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExpandReply {
    pub lex: String,
    pub vec: String,
    pub hyde: String,
}

/// Expansion job: given the user's question, produce three rewrite terms.
#[derive(Debug)]
pub struct ExpandJob {
    pub question: String,
    pub reply: oneshot::Sender<ExpandResult>,
}

pub type ExpandResult = Result<ExpandReply, WorkerError>;

/// Job enqueued onto the worker pool. Workers dispatch by variant.
#[derive(Debug)]
pub enum AgentJob {
    Synth(SynthJob),
    Expand(ExpandJob),
}

/// Sender half. Cloneable; each producer clones one. Senders block
/// (`.send().await`) when the queue is full — backpressure.
pub type JobSender = async_channel::Sender<AgentJob>;

/// Receiver half. Cloneable; each worker clones one. Receivers block
/// (`.recv().await`) when the queue is empty.
pub type JobReceiver = async_channel::Receiver<AgentJob>;

/// Create a bounded job queue with `capacity` = workers × 4.
pub fn queue(workers: usize) -> (JobSender, JobReceiver) {
    async_channel::bounded(workers * 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queue_send_recv_roundtrip() {
        let (tx, rx) = queue(2);
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(AgentJob::Synth(SynthJob {
            context: "ctx".into(),
            question: "q".into(),
            reply: reply_tx,
        }))
        .await
        .unwrap();
        let got = rx.recv().await.unwrap();
        match got {
            AgentJob::Synth(job) => {
                assert_eq!(job.question, "q");
                let _ = job.reply.send(Ok(SynthReply {
                    answer: "hi".into(),
                    citations: vec![],
                }));
            }
            _ => panic!("unexpected job variant"),
        }
        let reply = reply_rx.await.unwrap().unwrap();
        assert_eq!(reply.answer, "hi");
    }

    #[test]
    fn queue_capacity_scales_with_workers() {
        let (tx, _rx) = queue(1);
        assert_eq!(tx.capacity(), Some(4));
        let (tx, _rx) = queue(16);
        assert_eq!(tx.capacity(), Some(64));
    }
}
