//! Retrieval actor: eager-loaded ONNX embedder running over a shared
//! `MemexHandle` handle.
//!
//! Orchestration (embed → BM25 + vector → RRF → MIN_SCORE) lives in
//! `memex_core::retrieval` and is shared with `memex search`. This module
//! owns: daemon IPC types (`Entry`, `RetrievalReq/Resp/Error`) and the
//! actor that serializes one retrieval request at a time against the
//! model. Memex handles come from the daemon-wide `MemexHandle` so query
//! and ingest paths share one SQLite connection per root. Each `Entry`
//! is either a wiki page or a source document — hence the generic name.

use crate::daemon::handler::SharedEmbedder;
use crate::daemon::memex_handle::MemexHandle;
use memex_core::retrieval::{self as core_retrieval, HybridResult, Signal, embed_query};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Canonical citable identifier. Always the docid (content-hash-derived,
    /// guaranteed unique across the memex root). Use this in `[[id]]`
    /// citations from the synthesis LLM.
    pub id: String,
    /// Human-readable label. Wiki: the slug (e.g. "caroline"). Source:
    /// the first-user-message truncated to 80 chars. Provided as a hint
    /// to the LLM for quick topic orientation; not guaranteed unique.
    pub title: String,
    pub doc_type: String,
    pub rank: u32,
    pub signal: Signal,
    pub body: String,
}

/// Raw expansion terms before embedding. Pairs with
/// `memex_core::retrieval::Expansion` which holds the embedded form.
#[derive(Debug, Clone, Default)]
pub struct ExpansionTerms {
    pub lex: String,
    pub vec: String,
    pub hyde: String,
}

#[derive(Debug)]
pub struct RetrievalReq {
    pub memex_root: std::path::PathBuf,
    pub question: String,
    pub top_k: usize,
    pub collections: Vec<String>,
    /// Optional caller-supplied "what the user is really after" string.
    /// Boosts focused-snippet line picking (`core::snippet`) when set, and
    /// forces the full expansion+rerank pipeline (`is_strong_signal`
    /// returns false when intent is `Some`).
    pub intent: Option<String>,
    /// Backend-provided expansion terms (lex as strings; vec/hyde get
    /// embedded inside the actor). None = no expansion (initial probe).
    pub expansion: Option<ExpansionTerms>,
    pub reply: oneshot::Sender<Result<RetrievalResp, RetrievalError>>,
}

#[derive(Debug)]
pub struct RetrievalResp {
    pub entries: Vec<Entry>,
    pub signal: Signal,
}

#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    #[error("memex_root invalid: {0}")]
    InvalidRoot(String),
    /// No documents matched. `collections` carries the filter list (empty
    /// = no filter) so the handler can produce a precise error message:
    /// "corpus is empty" vs. "filter excluded everything".
    #[error("no documents matched")]
    Empty { collections: Vec<String> },
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Sender half for enqueuing retrieval requests. Cloneable.
pub type RetrievalSender = mpsc::Sender<RetrievalReq>;

pub struct RetrievalActor {
    rx: mpsc::Receiver<RetrievalReq>,
    memex_handle: Arc<MemexHandle>,
    /// Shared with the request handler — same warm model, one ONNX
    /// session in memory. Locked per embed call (~50 ms warm); writes
    /// and queries serialize on it, which is fine for a single-user
    /// CLI tool and saves ~1.5 GB resident vs. per-actor models.
    embed_model: SharedEmbedder,
}

/// Spawn the retrieval actor with a shared embedder. The daemon owns
/// exactly one warm model; the handler (search/dedup/index) and this
/// actor (query embeds) both lock it on demand.
pub fn spawn(memex_handle: Arc<MemexHandle>, embed_model: SharedEmbedder) -> RetrievalSender {
    let (tx, rx) = mpsc::channel(64);
    let actor = RetrievalActor {
        rx,
        memex_handle,
        embed_model,
    };
    tokio::spawn(async move { actor.run().await });
    tx
}

impl RetrievalActor {
    async fn run(mut self) {
        while let Some(req) = self.rx.recv().await {
            let resp = self
                .handle(
                    req.memex_root.clone(),
                    &req.question,
                    req.top_k,
                    req.expansion,
                    &req.collections,
                    req.intent.as_deref(),
                )
                .await;
            let _ = req.reply.send(resp);
        }
    }

    async fn handle(
        &mut self,
        root: std::path::PathBuf,
        question: &str,
        top_k: usize,
        expansion: Option<ExpansionTerms>,
        collections: &[String],
        intent: Option<&str>,
    ) -> Result<RetrievalResp, RetrievalError> {
        // Lock once per request — embed all needed terms (query +
        // optional expansion lex/vec/hyde) under the same critical
        // section so we don't ping-pong with the handler between
        // each ~50 ms inference.
        //
        // Sequential per-term embedding here is intentional. Batching
        // all three into one `embed_queries_batch` call was tried (see
        // `core/tests/embed_query_batch_bench.rs`) and measured 1.35×
        // SLOWER (49 ms/query worse) on the typical retrieval shape:
        // question ~8 tokens, vec ~3 tokens, hyde ~100-200 tokens. The
        // padding-to-max-row waste (3 × hyde_len² of attention work)
        // outweighs the per-call overhead saving. Keep sequential
        // unless model/hardware shifts the crossover; the helper is
        // available for batches where input lengths are similar.
        let (q_emb, expansion_embedded) = {
            let mut model_guard = self.embed_model.lock().await;
            let model = model_guard.as_mut();
            let q_emb = embed_query(model, question)
                .map_err(|e| RetrievalError::Other(anyhow::anyhow!(e)))?;

            let expansion_embedded = match expansion.as_ref() {
                None => None,
                Some(e) => {
                    let vec_embs = if e.vec.is_empty() {
                        vec![]
                    } else {
                        vec![
                            embed_query(model, &e.vec)
                                .map_err(|err| RetrievalError::Other(anyhow::anyhow!(err)))?,
                        ]
                    };
                    let hyde_embs = if e.hyde.is_empty() {
                        vec![]
                    } else {
                        vec![
                            embed_query(model, &e.hyde)
                                .map_err(|err| RetrievalError::Other(anyhow::anyhow!(err)))?,
                        ]
                    };
                    Some(core_retrieval::Expansion {
                        lex: if e.lex.is_empty() {
                            vec![]
                        } else {
                            vec![e.lex.clone()]
                        },
                        vec_embs,
                        hyde_embs,
                        hyde: e.hyde.clone(),
                    })
                }
            };
            (q_emb, expansion_embedded)
        };

        let memex = self
            .memex_handle
            .get_or_open(&root)
            .map_err(|e| RetrievalError::InvalidRoot(e.to_string()))?;
        let default_exp = core_retrieval::Expansion::default();
        let exp = expansion_embedded.as_ref().unwrap_or(&default_exp);
        let HybridResult { results, signal } = core_retrieval::hybrid_retrieve_expanded(
            memex.search(),
            question,
            &q_emb,
            exp,
            collections,
            intent,
            memex.root(),
        )
        .map_err(|e| RetrievalError::Other(anyhow::anyhow!(e)))?;

        let top: Vec<_> = results.into_iter().take(top_k).collect();

        let mut entries = Vec::with_capacity(top.len());
        for (idx, r) in top.into_iter().enumerate() {
            let title = if r.doc_type == "wiki" {
                // For wiki, prefer the slug (filename stem) as title — it's
                // the canonical subject name (e.g. "caroline"), more useful
                // for orientation than the frontmatter title.
                r.path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| r.title.clone())
            } else {
                r.title.clone()
            };
            let rank = (idx as u32) + 1;
            entries.push(Entry {
                id: memex_core::docid::short(&r.hash).to_string(),
                title,
                doc_type: r.doc_type.clone(),
                rank,
                signal,
                // Focused snippet from the vector search path (~300 chars,
                // diff-style header). BM25-only hits have empty snippets;
                // the synth pipeline tolerates that.
                body: r.body,
            });
        }

        if entries.is_empty() {
            return Err(RetrievalError::Empty {
                collections: collections.to_vec(),
            });
        }
        Ok(RetrievalResp { entries, signal })
    }

}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_serializes_with_expected_shape() {
        let e = Entry {
            id: "abc".into(),
            title: "auth-migration".into(),
            doc_type: "wiki".into(),
            rank: 1,
            signal: Signal::Strong,
            body: "hello".into(),
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""id":"abc""#));
        assert!(s.contains(r#""title":"auth-migration""#));
        assert!(s.contains(r#""doc_type":"wiki""#));
        assert!(s.contains(r#""rank":1"#));
        assert!(s.contains(r#""signal":"strong""#));
    }
}
