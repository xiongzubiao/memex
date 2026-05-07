//! Agent job queue. Producers (connection handlers) send `BackendJob` onto a
//! bounded async-channel; consumers (worker tasks) pull jobs and run one LLM
//! turn per job.

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// Outcome of a worker's attempt. Applies to every backend —
/// subprocess-based (Claude Code, Codex, Gemini CLI) and direct-API
/// (OpenAI API).
#[derive(Debug)]
pub enum WorkerError {
    /// Subprocess crashed (EOF before terminal result, non-zero exit with
    /// no structured signal, write error). Retried once before surfacing.
    /// Subprocess-only — the OpenAI API backend never produces this.
    Crash(String),
    /// Read/request exceeded `daemon.worker.timeout_sec`. Retried once
    /// before surfacing. Carries the configured deadline so error
    /// messages name the actual seconds, not the config-key string.
    Timeout { secs: u64 },
    /// Backend returned an error OR produced text that didn't parse as
    /// the expected JSON reply. `code` carries the backend-native
    /// identifier when available — e.g. `authentication_failed` / `404`
    /// from Claude Code, `unauthorized` from Codex, `invalid_api_key`
    /// from OpenAI, `rpc=<n>` for JSON-RPC. Forwarded into the CLI error
    /// message as `[code] message` so the full diagnostic reaches the
    /// user without special-casing any particular failure mode.
    Backend {
        message: String,
        code: Option<String>,
    },
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
    /// Optional intent prefix. Forwarded to `apply_intent_prefix` in the worker.
    pub intent: Option<String>,
    pub reply: oneshot::Sender<ExpandResult>,
}

pub type ExpandResult = Result<ExpandReply, WorkerError>;

/// Reply from a worker for a synthesis job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynthReply {
    pub answer: String,
    pub citations: Vec<String>,
}

/// Synthesis job: context block + question → synthesized answer.
#[derive(Debug)]
pub struct SynthJob {
    /// Already-formatted context block (all retrieved pages, rank-tagged).
    pub context: String,
    /// The user's question, verbatim.
    pub question: String,
    /// Optional intent prefix. Forwarded to `apply_intent_prefix` in the worker.
    pub intent: Option<String>,
    /// Worker sends the result here.
    pub reply: oneshot::Sender<SynthResult>,
}

pub type SynthResult = Result<SynthReply, WorkerError>;

/// Reply from a worker for an ingest extraction job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestReply {
    /// Wiki pages extracted from the transcript. Each has slug, title, tags, body.
    pub pages: Vec<ExtractedPage>,
}

/// A wiki page extracted from a session transcript by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedPage {
    pub slug: String,
    pub title: String,
    pub body: String,
}

pub type IngestResult = Result<IngestReply, WorkerError>;

/// One unit of LLM-visible content fed into the Extract task. Optional
/// fields convey the asymmetry between transcripts (per-turn role +
/// timestamp) and documents (single segment, no role).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractSegment {
    /// 1-based ordinal hint. Optional — array position is canonical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// Speaker identity for transcripts (e.g. "user", "assistant").
    /// Absent for documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// ISO 8601 timestamp when known. Absent for documents and for
    /// transcripts whose underlying turn lacked a timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// The body of this segment.
    pub text: String,
}

/// Position of a chunk within a multi-chunk document. The two fields are
/// always set together — a chunk that knows its index also knows the
/// total. Wrapping them in one struct prevents the inconsistent state
/// "index without total."
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ChunkPosition {
    /// 0-based chunk index.
    pub index: usize,
    /// Total chunks in this document.
    pub total: usize,
}

/// Ingest extraction job: cleaned segments → wiki pages.
/// Carries both transcript (multi-segment) and document (single-segment-
/// per-call) Extract calls.
#[derive(Debug)]
pub struct IngestJob {
    /// Segments to extract from. For transcripts, one per turn. For
    /// document chunks, exactly one segment carrying the chunk body.
    pub segments: Vec<ExtractSegment>,
    /// Provenance identifier (URL, file path, transcript path). Always
    /// populated by callers. Used in document framing for "this is chunk
    /// N of M from {source}" preamble and for slug/title hints.
    pub source: String,
    /// Position within a multi-chunk document. None for transcripts and
    /// single-chunk docs.
    pub chunk: Option<ChunkPosition>,
    /// Worker sends extracted pages here.
    pub reply: oneshot::Sender<IngestResult>,
}

/// Merge job: proposed page + existing page → merged page.
#[derive(Debug)]
pub struct MergeJob {
    /// Pages to merge: each pair is (proposed new content, existing content).
    pub pages: Vec<MergePair>,
    /// Worker sends the merged pages here.
    pub reply: oneshot::Sender<MergeResult>,
}

/// A pair of pages to merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergePair {
    pub slug: String,
    pub proposed: String,
    pub existing: String,
}

/// Reply from a merge job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeReply {
    pub merged_pages: Vec<ExtractedPage>,
}

pub type MergeResult = Result<MergeReply, WorkerError>;

/// Job enqueued onto the worker pool. Workers dispatch by variant.
#[derive(Debug)]
pub enum BackendJob {
    Expand(ExpandJob),
    Synth(SynthJob),
    Ingest(IngestJob),
    Merge(MergeJob),
}

/// Sender half. Cloneable; each producer clones one. Senders block
/// (`.send().await`) when the queue is full — backpressure.
pub type JobSender = async_channel::Sender<BackendJob>;

/// Receiver half. Cloneable; each worker clones one. Receivers block
/// (`.recv().await`) when the queue is empty.
pub type JobReceiver = async_channel::Receiver<BackendJob>;

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
        tx.send(BackendJob::Synth(SynthJob {
            context: "ctx".into(),
            question: "q".into(),
            intent: None,
            reply: reply_tx,
        }))
        .await
        .unwrap();
        let got = rx.recv().await.unwrap();
        match got {
            BackendJob::Synth(job) => {
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
