//! Shared retrieval orchestration: BM25 (wiki + source) + vector search →
//! RRF fusion → MIN_SCORE filter. Used by the `memex search` command and by
//! the daemon's query path.

use crate::embed::{EmbeddingModel, catch_unwind_silent, embed_text, load_model};
use crate::search::{self, Bm25Search, MIN_SCORE, SearchResult};
use crate::vector::vector_search_collapsed;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Signal classification from wiki BM25 top-1/top-2 scores.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Signal {
    Strong,
    Weak,
}

/// Output of hybrid retrieval: fused + filtered results plus signal.
pub struct HybridResult {
    pub results: Vec<SearchResult>,
    pub signal: Signal,
}

/// Agent-provided expansion terms, already embedded where vector search
/// is needed. Each field can be empty.
#[derive(Debug, Default)]
pub struct Expansion {
    /// Lexical expansion terms — each runs as an additional BM25 probe
    /// (wiki weighted 2x via RRF; source included unless `wiki_only`).
    pub lex: Vec<String>,
    /// Vector expansion term embeddings — each runs as an additional
    /// chunk-level vector search contributing to RRF (1x weight).
    pub vec_embs: Vec<Vec<f32>>,
    /// HyDE term embeddings — same handling as `vec_embs`. Separate name
    /// for clarity; the retrieval treatment is identical.
    pub hyde_embs: Vec<Vec<f32>>,
}

/// Hybrid BM25 + vector retrieval with optional expansion.
/// When `wiki_only` is true, skips source collection searches.
pub fn hybrid_retrieve_expanded(
    search: &Bm25Search,
    question: &str,
    q_emb: &[f32],
    expansion: &Expansion,
    wiki_only: bool,
) -> Result<HybridResult> {
    let wiki = search.search_collection(question, "wiki", 20)?;

    let s1 = wiki.first().map(|r| r.score as f64).unwrap_or(0.0);
    let s2 = wiki.get(1).map(|r| r.score as f64).unwrap_or(0.0);
    let signal = if search::is_strong_signal(s1, s2) {
        Signal::Strong
    } else {
        Signal::Weak
    };

    let mut lists = vec![wiki];
    let mut wiki_indices: Vec<usize> = vec![0];

    if !wiki_only {
        let source = search.search_collection(question, "source", 20)?;
        lists.push(source);
    }

    // Primary vector search (skipped when embedding is empty, i.e. no ONNX model).
    if !q_emb.is_empty() {
        let vec_primary = vector_search_as_results(search, q_emb)?;
        let vec_primary: Vec<SearchResult> = if wiki_only {
            vec_primary.into_iter().filter(|r| r.collection == "wiki").collect()
        } else {
            vec_primary
        };
        if !vec_primary.is_empty() {
            lists.push(vec_primary);
        }
    }

    // Lex expansion — one extra wiki pair per term (+ source if not wiki_only).
    for term in &expansion.lex {
        let lex_wiki = search.search_collection(term, "wiki", 20)?;
        wiki_indices.push(lists.len());
        lists.push(lex_wiki);
        if !wiki_only {
            let lex_source = search.search_collection(term, "source", 20)?;
            lists.push(lex_source);
        }
    }

    // Vec + hyde expansion — vector-only (skipped when embeddings are empty).
    for emb in &expansion.vec_embs {
        if emb.is_empty() { continue; }
        let r = vector_search_as_results(search, emb)?;
        if !r.is_empty() {
            lists.push(r);
        }
    }
    for emb in &expansion.hyde_embs {
        if emb.is_empty() { continue; }
        let r = vector_search_as_results(search, emb)?;
        if !r.is_empty() {
            lists.push(r);
        }
    }

    let fused = search::rrf_fuse(&lists, &wiki_indices, 60);
    let results: Vec<SearchResult> = fused.into_iter().filter(|r| r.score >= MIN_SCORE).collect();

    Ok(HybridResult { results, signal })
}

/// Default embedding model file path: `~/.memex/models/embedding-gemma-300m.onnx`.
pub fn default_model_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".memex/models/embedding-gemma-300m.onnx"))
}

/// Best-effort ONNX model load. Returns `None` if the file is missing, if the
/// loader fails, or if the loader panics (ONNX init can trap on missing libs).
pub fn load_default_model() -> Option<EmbeddingModel> {
    let path = default_model_path()?;
    if !path.exists() {
        return None;
    }
    let path_str = path.to_str()?;
    match catch_unwind_silent(|| load_model(path_str, "embedding-gemma-300m")) {
        Some(Ok(m)) => Some(m),
        _ => None,
    }
}

/// Embed `text` using the provided model. Returns empty vec if the model
/// is missing or the call fails. Callers should skip vector search when empty.
pub fn embed_query(model: Option<&mut EmbeddingModel>, text: &str) -> Vec<f32> {
    if let Some(m) = model
        && let Ok(v) = embed_text(m, text)
    {
        return v;
    }
    Vec::new()
}

/// Chunk and embed a document body into the vector search index.
/// Pass a pre-loaded model to avoid redundant ONNX loads when embedding
/// multiple documents. Skips entirely if no model is available.
pub fn embed_document(search: &Bm25Search, hash: &str, body: &str, model: &mut EmbeddingModel) {
    let chunks = crate::embed::chunk_text(body, 900, 0.15);
    let _ = search.with_connection(|conn| {
        crate::vector::delete_chunks(conn, hash)?;
        for (seq, chunk) in chunks.iter().enumerate() {
            let embedding = embed_query(Some(model), &chunk.text);
            let _ = crate::vector::store_chunk(
                conn,
                hash,
                seq as i32,
                &chunk.text,
                chunk.pos,
                chunk.len,
                "embedding-gemma-300m",
                &embedding,
            );
        }
        Ok(())
    });
}

/// Search for a wiki page by title for dedup. Title-column-only BM25
/// and vector search run independently and fuse via RRF. This catches
/// both lexical matches ("Auth Migration" matches "Auth Migration Timeline")
/// and semantic matches ("OAuth Token Refresh" matches "Authentication Token
/// Renewal") when the ONNX model is available.
pub fn search_wiki_by_title(
    search: &Bm25Search,
    title: &str,
    model: Option<&mut EmbeddingModel>,
) -> Option<String> {
    let bm25 = search.search_title_only(title, "wiki", 5).ok().unwrap_or_default();

    let title_emb = embed_query(model, title);
    let vec_wiki = if !title_emb.is_empty() {
        let vec_results = vector_search_as_results(search, &title_emb).ok().unwrap_or_default();
        vec_results.into_iter().filter(|r| r.collection == "wiki").collect()
    } else {
        Vec::new()
    };

    if bm25.is_empty() && vec_wiki.is_empty() {
        return None;
    }

    let fused = search::rrf_fuse(&[bm25, vec_wiki], &[0], 60);
    let best = fused.into_iter().next()?;

    let path_str = best.path.to_string_lossy();
    let slug = path_str
        .strip_prefix("wiki/").unwrap_or(&path_str)
        .strip_suffix(".md").unwrap_or(&path_str);
    Some(slug.to_string())
}

/// Map chunk-level vector search hits to document-level SearchResults. Uses
/// the best chunk text as the snippet.
pub fn vector_search_as_results(search: &Bm25Search, q_emb: &[f32]) -> Result<Vec<SearchResult>> {
    let vec_results = search.with_connection(|conn| vector_search_collapsed(conn, q_emb, 20))?;

    let mut out = Vec::new();
    for vr in &vec_results {
        let docs = search.lookup_documents_by_hash(&vr.hash)?;
        for doc in docs {
            out.push(SearchResult {
                path: PathBuf::from(&doc.path),
                title: doc.title,
                score: vr.score,
                snippet: vr.chunk_text.clone(),
                collection: doc.collection,
                docid: doc.docid,
            });
        }
    }
    Ok(out)
}

/// Display stem for a SearchResult. Wiki: filename without extension, falling
/// back to docid. Source: full path as string.
pub fn result_stem(r: &SearchResult) -> String {
    if r.collection == "wiki" {
        r.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&r.docid)
            .to_string()
    } else {
        r.path.to_string_lossy().into_owned()
    }
}
