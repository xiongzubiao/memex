//! Shared retrieval orchestration: BM25 (wiki + source) + vector search →
//! RRF fusion → MIN_SCORE filter. Used by the `memex search` command and by
//! the daemon's query path.

use crate::embed::{EmbeddingModel, catch_unwind_silent, embed_text, load_model};
use crate::search::{self, Bm25Search, MIN_SCORE, SearchResult};
use crate::vector::vector_search_collapsed;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf};

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

/// Backend-provided expansion terms, already embedded where vector search
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
/// When `wiki_only` is true, skips source doc_type searches.
pub fn hybrid_retrieve_expanded(
    search: &Bm25Search,
    question: &str,
    q_emb: &[f32],
    expansion: &Expansion,
    wiki_only: bool,
    collections: &[String],
) -> Result<HybridResult> {
    let collections = search::normalize_collections(collections);
    let wiki = search.search_by_doc_type_in_collections(question, "wiki", 20, &collections)?;

    let s1 = wiki.first().map(|r| r.score as f64).unwrap_or(0.0);
    let s2 = wiki.get(1).map(|r| r.score as f64).unwrap_or(0.0);
    let signal = if search::is_strong_signal(s1, s2) {
        Signal::Strong
    } else {
        Signal::Weak
    };

    // Per-list RRF weights, factored as `wiki_factor × positional_factor`:
    // - Wiki factor (2×): trust curated wiki content over raw source.
    // - Positional factor (1×, disabled): QMD enables positional because
    //   it has a post-RRF reranker to repair any over-boost; memex does not.
    const W_WIKI_PRIMARY: f32 = 2.0;
    const W_SOURCE_PRIMARY: f32 = 1.0;
    const W_WIKI_EXPANSION: f32 = 2.0;
    const W_SOURCE_EXPANSION: f32 = 1.0;

    let mut lists = vec![wiki];
    let mut weights: Vec<f32> = vec![W_WIKI_PRIMARY];

    if !wiki_only {
        let source =
            search.search_by_doc_type_in_collections(question, "source", 20, &collections)?;
        lists.push(source);
        weights.push(W_SOURCE_PRIMARY);
    }

    // Run vector search per doc_type so each gets its own fair pool of
    // 20 candidates. A mixed query would let sources (typically 10× more
    // docs than wiki) crowd wiki out of the top-k.
    let push_vector_per_doc_type = |q: &[f32],
                                    wiki_weight: f32,
                                    source_weight: f32,
                                    lists: &mut Vec<Vec<SearchResult>>,
                                    weights: &mut Vec<f32>|
     -> Result<()> {
        let vec_wiki = vector_search_as_results(search, q, Some("wiki"), &collections)?;
        if !vec_wiki.is_empty() {
            lists.push(vec_wiki);
            weights.push(wiki_weight);
        }
        if !wiki_only {
            let vec_source = vector_search_as_results(search, q, Some("source"), &collections)?;
            if !vec_source.is_empty() {
                lists.push(vec_source);
                weights.push(source_weight);
            }
        }
        Ok(())
    };

    // Primary vector search (skipped when embedding is empty, i.e. no ONNX model).
    if !q_emb.is_empty() {
        push_vector_per_doc_type(
            q_emb,
            W_WIKI_PRIMARY,
            W_SOURCE_PRIMARY,
            &mut lists,
            &mut weights,
        )?;
    }

    // Lex expansion — one extra wiki pair per term (+ source if not wiki_only).
    for term in &expansion.lex {
        let lex_wiki = search.search_by_doc_type_in_collections(term, "wiki", 20, &collections)?;
        lists.push(lex_wiki);
        weights.push(W_WIKI_EXPANSION);
        if !wiki_only {
            let lex_source =
                search.search_by_doc_type_in_collections(term, "source", 20, &collections)?;
            lists.push(lex_source);
            weights.push(W_SOURCE_EXPANSION);
        }
    }

    // Vec + hyde expansion — vector-only, per-doc_type split.
    for emb in &expansion.vec_embs {
        if emb.is_empty() {
            continue;
        }
        push_vector_per_doc_type(
            emb,
            W_WIKI_EXPANSION,
            W_SOURCE_EXPANSION,
            &mut lists,
            &mut weights,
        )?;
    }
    for emb in &expansion.hyde_embs {
        if emb.is_empty() {
            continue;
        }
        push_vector_per_doc_type(
            emb,
            W_WIKI_EXPANSION,
            W_SOURCE_EXPANSION,
            &mut lists,
            &mut weights,
        )?;
    }

    let fused = search::rrf_fuse(&lists, &weights, 60);
    let results: Vec<SearchResult> = fused.into_iter().filter(|r| r.score >= MIN_SCORE).collect();

    Ok(HybridResult { results, signal })
}

/// Default embedding model file path: `~/.memex/models/embedding-gemma-300m.onnx`.
pub fn default_model_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".memex/models/embedding-gemma-300m.onnx"))
}

/// Load the default ONNX embedding model. Errors with
/// `MemexError::EmbeddingUnavailable` when the model file is missing, the
/// ONNX Runtime dylib cannot be loaded, or the loader panics.
pub fn load_default_model() -> crate::error::Result<EmbeddingModel> {
    let path =
        default_model_path().ok_or_else(|| crate::error::MemexError::EmbeddingUnavailable {
            path: std::path::PathBuf::from("~/.memex/models/embedding-gemma-300m.onnx"),
            reason: "cannot resolve home directory".into(),
        })?;
    if !path.exists() {
        return Err(crate::error::MemexError::EmbeddingUnavailable {
            path,
            reason: "model file not found (run `memex` plugin postinstall, \
                     or download embedding-gemma-300m.onnx into \
                     ~/.memex/models/)"
                .into(),
        });
    }
    let path_str = path
        .to_str()
        .ok_or_else(|| crate::error::MemexError::EmbeddingUnavailable {
            path: path.clone(),
            reason: "model path is not valid UTF-8".into(),
        })?;
    match catch_unwind_silent(|| load_model(path_str, "embedding-gemma-300m")) {
        Some(Ok(m)) => Ok(m),
        Some(Err(e)) => Err(crate::error::MemexError::EmbeddingUnavailable {
            path,
            reason: format!("ONNX loader failed: {e}"),
        }),
        None => Err(crate::error::MemexError::EmbeddingUnavailable {
            path,
            reason: "ONNX Runtime dylib load panicked (install libonnxruntime \
                     via `brew install onnxruntime`, your distro's package \
                     manager, or place the dylib in ~/.memex/lib/)"
                .into(),
        }),
    }
}

/// Embed `text` using the provided model. Propagates any ONNX inference errors.
pub fn embed_query(model: &mut EmbeddingModel, text: &str) -> crate::error::Result<Vec<f32>> {
    embed_text(model, text)
}

/// Chunk and embed a document body into the vector search index.
/// Errors propagate from embedding or storage failures.
pub fn embed_document(
    search: &Bm25Search,
    hash: &str,
    body: &str,
    model: &mut EmbeddingModel,
) -> crate::error::Result<()> {
    let chunks = crate::embed::chunk_text(body, 900, 0.15);
    let mut embeddings: Vec<(usize, Vec<f32>, crate::embed::Chunk)> =
        Vec::with_capacity(chunks.len());
    for (seq, chunk) in chunks.into_iter().enumerate() {
        let embedding = embed_text(model, &chunk.text)?;
        embeddings.push((seq, embedding, chunk));
    }
    search.with_transaction(|tx| {
        // Re-inserting content (INSERT OR IGNORE) guards against a concurrent
        // session's cleanup_orphaned_content racing between our store_ingest_batch
        // commit and this embedding step — a concurrent MERGE can delete the
        // content row between those two points, causing FK failures in store_chunk.
        crate::content::insert_content(tx, body)?;
        crate::vector::delete_chunks(tx, hash)?;
        for (seq, embedding, chunk) in &embeddings {
            crate::vector::store_chunk(
                tx,
                hash,
                *seq as i32,
                &chunk.text,
                chunk.pos,
                chunk.len,
                "embedding-gemma-300m",
                embedding,
            )?;
        }
        Ok(())
    })
}

/// Search for a wiki page by title for dedup. Requires a **lexical anchor**:
/// BM25 on the wiki title column must return at least one candidate. If
/// BM25 is empty (no shared title tokens between the proposed title and
/// any existing page), returns `None` — the caller treats it as "no
/// existing page; create new."
///
/// The vector search over existing wiki pages is used only to re-rank
/// within the BM25-anchored candidates, not as a standalone signal —
/// vector alone produces nonsense matches across unrelated entities
/// (e.g. "Caroline" fuzzy-matching "Gina" because both embed as "person"),
/// which corrupts the merge pipeline downstream.
pub fn search_wiki_by_title(
    search: &Bm25Search,
    title: &str,
    model: &mut EmbeddingModel,
) -> crate::error::Result<Option<String>> {
    let bm25 = search
        .search_title_only(title, "wiki", 5)
        .ok()
        .unwrap_or_default();
    if bm25.is_empty() {
        return Ok(None);
    }

    // Re-rank the BM25 candidates using vector similarity (fused RRF). This
    // promotes the semantically closest BM25 hit when there are several
    // (e.g. "Auth Migration" over "Auth Service").
    let title_emb = embed_query(model, title)?;
    let vec_wiki = vector_search_as_results(search, &title_emb, Some("wiki"), &[])
        .ok()
        .unwrap_or_default();

    let fused = search::rrf_fuse(&[bm25, vec_wiki], &[2.0, 1.0], 60);
    let Some(best) = fused.into_iter().next() else {
        return Ok(None);
    };

    let path_str = best.path.to_string_lossy();
    let slug = path_str
        .strip_prefix("wiki/")
        .unwrap_or(&path_str)
        .strip_suffix(".md")
        .unwrap_or(&path_str);
    Ok(Some(slug.to_string()))
}

/// Map chunk-level vector search hits to document-level SearchResults. Uses
/// the best chunk text as the snippet.
pub fn vector_search_as_results(
    search: &Bm25Search,
    q_emb: &[f32],
    doc_type: Option<&str>,
    collections: &[String],
) -> Result<Vec<SearchResult>> {
    // Per-doc_type pool size. Larger than the BM25 list limit (20)
    // because vector search needs more depth to compensate for lacking a
    // post-RRF reranker — QMD sends the fused top 40 to its reranker;
    // we instead front-load depth into the candidate lists.
    let selected = search::normalize_collections(collections);
    let fetch_limit = if selected.len() == 1 && selected[0] == "default" {
        40
    } else {
        120
    };
    let vec_results = search
        .with_connection(|conn| vector_search_collapsed(conn, q_emb, fetch_limit, doc_type))?;

    let selected: HashSet<String> = selected.into_iter().collect();
    let mut out = Vec::new();
    for vr in &vec_results {
        let docs = search.lookup_documents_by_hash(&vr.hash)?;
        for doc in docs {
            if !selected.is_empty() {
                let memberships = search.document_collections_by_path(&doc.doc_type, &doc.path)?;
                if !memberships.iter().any(|name| selected.contains(name)) {
                    continue;
                }
            }
            out.push(SearchResult {
                path: PathBuf::from(&doc.path),
                title: doc.title,
                score: vr.score,
                snippet: vr.chunk_text.clone(),
                doc_type: doc.doc_type,
                docid: doc.docid,
                hash: vr.hash.clone(),
            });
        }
    }
    Ok(out)
}

