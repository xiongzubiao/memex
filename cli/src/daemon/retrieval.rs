//! Retrieval actor: eager-loaded ONNX embedder running over a shared
//! `MemexCache` handle.
//!
//! Orchestration (embed → BM25 + vector → RRF → MIN_SCORE) lives in
//! `memex_core::retrieval` and is shared with `memex search`. This module
//! owns: daemon IPC types (`Entry`, `RetrievalReq/Resp/Error`) and the
//! actor that serializes one retrieval request at a time against the
//! model. Memex handles come from the daemon-wide `MemexCache` so query
//! and ingest paths share one SQLite connection per root. Each `Entry`
//! is either a wiki page or a source document — hence the generic name.

use crate::daemon::memex_cache::MemexCache;
use memex_core::retrieval::{
    self as core_retrieval, HybridResult, Signal, embed_query, load_default_model,
};
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
    #[error("no indexed content for this wiki")]
    Empty,
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

/// Sender half for enqueuing retrieval requests. Cloneable.
pub type RetrievalSender = mpsc::Sender<RetrievalReq>;

pub struct RetrievalActor {
    rx: mpsc::Receiver<RetrievalReq>,
    memex_cache: Arc<MemexCache>,
    model: memex_core::embed::EmbeddingModel,
}

/// Spawn the retrieval actor. Errors if the ONNX model or runtime library
/// cannot be loaded — the daemon refuses to start without a working embedder.
pub fn spawn(memex_cache: Arc<MemexCache>) -> memex_core::error::Result<RetrievalSender> {
    let (tx, rx) = mpsc::channel(64);
    let model = load_default_model()?;
    let actor = RetrievalActor {
        rx,
        memex_cache,
        model,
    };
    tokio::spawn(async move { actor.run().await });
    Ok(tx)
}

impl RetrievalActor {
    async fn run(mut self) {
        while let Some(req) = self.rx.recv().await {
            let resp = self.handle(
                req.memex_root.clone(),
                &req.question,
                req.top_k,
                req.expansion,
                &req.collections,
            );
            let _ = req.reply.send(resp);
        }
    }

    fn handle(
        &mut self,
        root: std::path::PathBuf,
        question: &str,
        top_k: usize,
        expansion: Option<ExpansionTerms>,
        collections: &[String],
    ) -> Result<RetrievalResp, RetrievalError> {
        let q_emb = embed_query(&mut self.model, question)
            .map_err(|e| RetrievalError::Other(anyhow::anyhow!(e)))?;

        // Embed expansion vec/hyde terms if present.
        let expansion_embedded = match expansion.as_ref() {
            None => None,
            Some(e) => {
                let vec_embs = if e.vec.is_empty() {
                    vec![]
                } else {
                    vec![
                        embed_query(&mut self.model, &e.vec)
                            .map_err(|err| RetrievalError::Other(anyhow::anyhow!(err)))?,
                    ]
                };
                let hyde_embs = if e.hyde.is_empty() {
                    vec![]
                } else {
                    vec![
                        embed_query(&mut self.model, &e.hyde)
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
                })
            }
        };

        let memex = self
            .memex_cache
            .get_or_open(&root)
            .map_err(|e| RetrievalError::InvalidRoot(e.to_string()))?;
        let default_exp = core_retrieval::Expansion::default();
        let exp = expansion_embedded.as_ref().unwrap_or(&default_exp);
        let HybridResult { results, signal } = core_retrieval::hybrid_retrieve_expanded(
            memex.search(),
            question,
            &q_emb,
            exp,
            false,
            collections,
        )
        .map_err(|e| RetrievalError::Other(anyhow::anyhow!(e)))?;

        let top: Vec<_> = results.into_iter().take(top_k).collect();

        // Single batch read: one SQLite round-trip + one mutex cycle fetches
        // every entry's body, replacing N per-entry reads.
        let hashes: Vec<&str> = top.iter().map(|r| r.hash.as_str()).collect();
        let bodies = memex
            .search()
            .with_connection(|conn| memex_core::content::get_content_batch(conn, &hashes))
            .map_err(|e| RetrievalError::Other(anyhow::anyhow!(e)))?;

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
            let body = bodies
                .get(&r.hash)
                .map(|raw| {
                    memex_core::validate::parse_frontmatter(raw)
                        .map(|(_fm, b)| b)
                        .unwrap_or_else(|_| raw.clone())
                })
                .unwrap_or_default();
            let rank = (idx as u32) + 1;
            entries.push(Entry {
                id: r.docid.clone(),
                title,
                doc_type: r.doc_type.clone(),
                rank,
                signal,
                body,
            });
        }

        if entries.is_empty() {
            return Err(RetrievalError::Empty);
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
