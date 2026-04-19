//! Retrieval actor: eager-loaded ONNX + SQLite reader, probe + read top-K.
//!
//! Orchestration (embed → BM25 + vector → RRF → MIN_SCORE) lives in
//! `memex_core::retrieval` and is shared with `memex search`. This module owns:
//! daemon IPC types (`Page`, `RetrievalReq/Resp/Error`), the actor + handle
//! cache, and reading the page body from disk.

use memex_core::retrieval::{
    self as core_retrieval, HybridResult, Signal, embed_query, hybrid_retrieve, load_default_model,
};
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    pub docid: String,
    pub stem: String,
    pub collection: String,
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
    /// Agent-provided expansion terms (lex as strings; vec/hyde get
    /// embedded inside the actor). None = no expansion (initial probe).
    pub expansion: Option<ExpansionTerms>,
    pub reply: oneshot::Sender<Result<RetrievalResp, RetrievalError>>,
}

#[derive(Debug)]
pub struct RetrievalResp {
    pub pages: Vec<Page>,
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

/// Cap on per-root SQLite handles kept in memory. Typical use is one root
/// per user; integration tests rotate tempdirs and would leak handles
/// without this bound.
const MAX_HANDLES: usize = 8;

pub struct RetrievalActor {
    rx: mpsc::Receiver<RetrievalReq>,
    handles: HashMap<PathBuf, memex_core::Memex>,
    /// LRU order of keys in `handles`, oldest at index 0.
    lru_order: Vec<PathBuf>,
    model: Option<memex_core::embed::EmbeddingModel>,
}

/// Spawn the retrieval actor. ONNX is loaded eagerly; missing/broken model
/// logs a warning and falls back to the hash embedding.
pub fn spawn() -> RetrievalSender {
    let (tx, rx) = mpsc::channel(64);
    let model = load_default_model();
    if model.is_none() {
        tracing::warn!("ONNX model not available; using hash embedding fallback");
    }
    let actor = RetrievalActor {
        rx,
        handles: HashMap::new(),
        lru_order: Vec::new(),
        model,
    };
    tokio::spawn(async move { actor.run().await });
    tx
}

impl RetrievalActor {
    async fn run(mut self) {
        while let Some(req) = self.rx.recv().await {
            let resp = self.handle(
                req.memex_root.clone(),
                &req.question,
                req.top_k,
                req.expansion,
            );
            let _ = req.reply.send(resp);
        }
    }

    fn handle(
        &mut self,
        root: PathBuf,
        question: &str,
        top_k: usize,
        expansion: Option<ExpansionTerms>,
    ) -> Result<RetrievalResp, RetrievalError> {
        // Embed first — releases the &mut self borrow on `model` before we
        // borrow `handles` via `get_or_open`.
        let q_emb = embed_query(self.model.as_mut(), question);

        // Embed expansion vec/hyde terms if present.
        let expansion_embedded = expansion.as_ref().map(|e| core_retrieval::Expansion {
            lex: if e.lex.is_empty() {
                vec![]
            } else {
                vec![e.lex.clone()]
            },
            vec_embs: if e.vec.is_empty() {
                vec![]
            } else {
                vec![embed_query(self.model.as_mut(), &e.vec)]
            },
            hyde_embs: if e.hyde.is_empty() {
                vec![]
            } else {
                vec![embed_query(self.model.as_mut(), &e.hyde)]
            },
        });

        let memex = self.get_or_open(&root)?;
        let HybridResult { results, signal } = match &expansion_embedded {
            Some(exp) => {
                core_retrieval::hybrid_retrieve_expanded(memex.search(), question, &q_emb, exp)
            }
            None => hybrid_retrieve(memex.search(), question, &q_emb),
        }
        .map_err(|e| RetrievalError::Other(anyhow::anyhow!(e)))?;

        let mut pages = Vec::with_capacity(top_k);
        for r in results.into_iter().take(top_k) {
            let stem = core_retrieval::result_stem(&r);
            let body = read_page_body(&root, &r).unwrap_or_default();
            let rank = (pages.len() as u32) + 1;
            pages.push(Page {
                docid: r.docid.clone(),
                stem,
                collection: r.collection.clone(),
                rank,
                signal,
                body,
            });
        }

        if pages.is_empty() {
            return Err(RetrievalError::Empty);
        }
        Ok(RetrievalResp { pages, signal })
    }

    fn get_or_open(&mut self, root: &Path) -> Result<&memex_core::Memex, RetrievalError> {
        if self.handles.contains_key(root) {
            // Promote to most-recently-used.
            if let Some(pos) = self.lru_order.iter().position(|p| p == root) {
                let key = self.lru_order.remove(pos);
                self.lru_order.push(key);
            }
        } else {
            if self.handles.len() >= MAX_HANDLES {
                let oldest = self.lru_order.remove(0);
                self.handles.remove(&oldest);
            }
            let m = memex_core::Memex::open(root.to_path_buf())
                .with_context(|| format!("opening memex at {root:?}"))
                .map_err(|e| RetrievalError::InvalidRoot(e.to_string()))?;
            self.handles.insert(root.to_path_buf(), m);
            self.lru_order.push(root.to_path_buf());
        }
        Ok(self.handles.get(root).unwrap())
    }
}

/// Read a page's full body from disk. Wiki paths are relative to MEMEX_ROOT;
/// source paths are absolute.
fn read_page_body(root: &Path, r: &memex_core::search::SearchResult) -> anyhow::Result<String> {
    let full = if r.path.is_absolute() {
        r.path.clone()
    } else {
        root.join(&r.path)
    };
    Ok(std::fs::read_to_string(&full).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_serializes_with_expected_shape() {
        let p = Page {
            docid: "abc".into(),
            stem: "auth-migration".into(),
            collection: "wiki".into(),
            rank: 1,
            signal: Signal::Strong,
            body: "hello".into(),
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains(r#""docid":"abc""#));
        assert!(s.contains(r#""collection":"wiki""#));
        assert!(s.contains(r#""rank":1"#));
        assert!(s.contains(r#""signal":"strong""#));
    }
}
