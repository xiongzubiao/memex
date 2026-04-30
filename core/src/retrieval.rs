//! Shared retrieval orchestration: BM25 (wiki + source) + vector search →
//! RRF fusion → MIN_SCORE filter. Used by the `memex search` command and by
//! the daemon's query path.

use crate::embed::{EmbeddingModel, Embedder, catch_unwind_silent, load_model};
use crate::error::Result;
use crate::search::{self, Bm25Search, MIN_SCORE, SearchResult};
use crate::vector::vector_search_collapsed;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf};

/// Disk-read cache shared between the vector probes and the BM25
/// snippet backfill. A wiki page that appears in primary, lex, vec,
/// and hyde lists is read from disk once per query.
type BodyCache = std::collections::HashMap<(String, String), String>;

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
    /// (wiki weighted 2x via RRF; source 1x).
    pub lex: Vec<String>,
    /// Vector expansion term embeddings — each runs as an additional
    /// chunk-level vector search contributing to RRF (1x weight).
    pub vec_embs: Vec<Vec<f32>>,
    /// HyDE term embeddings — same handling as `vec_embs`. Separate name
    /// for clarity; the retrieval treatment is identical.
    pub hyde_embs: Vec<Vec<f32>>,
}

/// Hybrid BM25 + vector retrieval with optional expansion.
///
/// `intent` (when `Some`) forces the weak-signal branch so the caller's
/// expansion+rerank pipeline always runs, and is plumbed into
/// `vector_search_as_results` so each `SearchResult.snippet` can be a
/// focused excerpt that prefers intent-bearing lines.
///
/// `memex_root` is the on-disk root used to materialize chunk text into
/// focused snippets (`crate::read_body_from_disk`).
pub fn hybrid_retrieve_expanded(
    search: &Bm25Search,
    question: &str,
    q_emb: &[f32],
    expansion: &Expansion,
    collections: &[String],
    intent: Option<&str>,
    memex_root: &std::path::Path,
) -> Result<HybridResult> {
    let collections = search::normalize_collections(collections);
    let wiki = search.search_by_doc_type_in_collections(question, "wiki", 20, &collections)?;
    let source = search.search_by_doc_type_in_collections(question, "raw", 20, &collections)?;

    // Strong-signal probe: an obvious BM25 winner in *either* list is
    // sufficient justification to skip LLM expansion. Each list gets
    // its own s1/s2 so the gap check stays within-list (avoids comparing
    // BM25 distributions across doc_types).
    let strong_in = |list: &[SearchResult]| -> bool {
        let s1 = list.first().map(|r| r.score as f64).unwrap_or(0.0);
        let s2 = list.get(1).map(|r| r.score as f64).unwrap_or(0.0);
        search::is_strong_signal(s1, s2, intent)
    };
    let signal = if strong_in(&wiki) || strong_in(&source) {
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

    let mut lists = vec![wiki, source];
    let mut weights: Vec<f32> = vec![W_WIKI_PRIMARY, W_SOURCE_PRIMARY];

    // Cache disk reads per (doc_type, path), shared across every vector
    // probe AND the BM25 snippet backfill — a wiki page that appears in
    // primary, lex, vec, hyde lists is read from disk once per query.
    let mut body_cache: BodyCache = std::collections::HashMap::new();

    let vec_ctx = VecQueryCtx {
        search,
        collections: &collections,
        primary_query: question,
        intent,
        memex_root,
    };
    // Run vector search per doc_type so each gets its own fair pool of
    // 20 candidates. A mixed query would let sources (typically 10× more
    // docs than wiki) crowd wiki out of the top-k.
    let push_vector_per_doc_type = |q: &[f32],
                                    wiki_weight: f32,
                                    source_weight: f32,
                                    lists: &mut Vec<Vec<SearchResult>>,
                                    weights: &mut Vec<f32>,
                                    body_cache: &mut BodyCache|
     -> Result<()> {
        let vec_wiki = vector_search_as_results(&vec_ctx, q, "wiki", body_cache)?;
        if !vec_wiki.is_empty() {
            lists.push(vec_wiki);
            weights.push(wiki_weight);
        }
        let vec_source = vector_search_as_results(&vec_ctx, q, "raw", body_cache)?;
        if !vec_source.is_empty() {
            lists.push(vec_source);
            weights.push(source_weight);
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
            &mut body_cache,
        )?;
    }

    // Lex expansion — one wiki + one source BM25 probe per term.
    for term in &expansion.lex {
        let lex_wiki = search.search_by_doc_type_in_collections(term, "wiki", 20, &collections)?;
        lists.push(lex_wiki);
        weights.push(W_WIKI_EXPANSION);
        let lex_source = search.search_by_doc_type_in_collections(term, "raw", 20, &collections)?;
        lists.push(lex_source);
        weights.push(W_SOURCE_EXPANSION);
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
            &mut body_cache,
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
            &mut body_cache,
        )?;
    }

    let fused = search::rrf_fuse(&lists, &weights, 60);
    let mut results: Vec<SearchResult> =
        fused.into_iter().filter(|r| r.score >= MIN_SCORE).collect();

    backfill_bm25_snippets(
        &mut results,
        search,
        question,
        intent,
        memex_root,
        &mut body_cache,
    );

    Ok(HybridResult { results, signal })
}

/// Stat-check + snippet-render pass for fused BM25/vector results.
///
/// Two responsibilities, deliberately fused into one pass so they
/// share `body_cache` and the per-result iteration:
///
/// 1. **Stat-check**: drop hits whose on-disk mtime/size disagree with
///    the indexed values. Otherwise the snippet would slice the wrong
///    bytes (chunk pos/len computed against a stale body). The vector
///    path already does this via `meta_by_id` at chunk-fanout time;
///    this is the BM25-side parallel.
///    `retain` (not `swap_remove`) preserves the RRF-fused order — a
///    middle-ranked stale entry shouldn't push a tail-ranked entry
///    above its honest peers.
///    Single batched meta lookup (`get_documents_meta_by_paths`)
///    instead of N per-row SELECTs: ~80 fused candidates used to mean
///    ~80 lock acquisitions on the query critical path.
///
/// 2. **Snippet rendering**: for each surviving BM25-only hit (vector
///    hits already brought their own snippet), score every chunk via
///    `score_chunk` against query+intent terms, render a focused
///    snippet from the best chunk's `(pos, len)`. Falls back to
///    whole-body when no chunks exist (doc indexed without an ONNX
///    model loaded).
fn backfill_bm25_snippets(
    results: &mut Vec<SearchResult>,
    search: &Bm25Search,
    question: &str,
    intent: Option<&str>,
    memex_root: &std::path::Path,
    body_cache: &mut BodyCache,
) {
    let pairs: Vec<(String, String)> = results
        .iter()
        .map(|r| (r.doc_type.clone(), r.path.to_string_lossy().to_string()))
        .collect();
    let meta_by_pair = match search.get_documents_meta_by_paths(&pairs) {
        Ok(m) => m,
        Err(e) => {
            // Lookup-as-a-batch failed: keep all entries (same
            // fallback semantics as the previous Err-arm). Log so a
            // recurring SQLite/IO failure surfaces in telemetry
            // instead of silently swallowing every search.
            tracing::warn!(
                error = %e,
                "BM25 stale-filter batched meta lookup failed; keeping all results"
            );
            std::collections::HashMap::new()
        }
    };
    results.retain(|r| {
        let path_str = r.path.to_string_lossy().to_string();
        match meta_by_pair.get(&(r.doc_type.clone(), path_str.clone())) {
            Some(meta) => !is_result_stale(memex_root, &path_str, &meta.mtime, meta.size),
            // Pair absent: either concurrent delete between fusion
            // and stat (correct to keep — snippet degrades gracefully)
            // or the batched lookup failed and the map is empty (also
            // correct to keep — same fallback semantics as the prior
            // per-row Err arm).
            None => true,
        }
    });

    let query_terms: Vec<String> = question
        .split_whitespace()
        .map(|s| s.to_lowercase())
        .collect();
    let intent_terms = intent
        .map(crate::snippet::extract_intent_terms)
        .unwrap_or_default();
    for r in results.iter_mut() {
        if !r.snippet.is_empty() {
            continue;
        }
        let path_str = r.path.to_string_lossy().to_string();
        let body = body_cache
            .entry((r.doc_type.clone(), path_str.clone()))
            .or_insert_with(|| {
                match crate::read_body_from_disk(memex_root, &r.doc_type, &path_str) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            doc_type = %r.doc_type,
                            path = %path_str,
                            error = %e,
                            "retrieval: read body failed; falling back to empty snippet"
                        );
                        String::new()
                    }
                }
            });
        if body.is_empty() {
            continue;
        }
        let chunks: Vec<(usize, usize)> = search
            .with_connection(|c| crate::vector::list_chunks_by_hash(c, &r.hash))
            .unwrap_or_default();
        let (best_pos, best_len) = if chunks.is_empty() {
            (0, body.len())
        } else {
            chunks
                .iter()
                .copied()
                .max_by(|a, b| {
                    let sa = crate::snippet::score_chunk(
                        body.get(a.0..a.0 + a.1).unwrap_or(""),
                        &query_terms,
                        &intent_terms,
                    );
                    let sb = crate::snippet::score_chunk(
                        body.get(b.0..b.0 + b.1).unwrap_or(""),
                        &query_terms,
                        &intent_terms,
                    );
                    sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or((0, body.len()))
        };
        let snippet_result = crate::snippet::extract_focused_snippet(
            body,
            best_pos,
            best_len,
            question,
            intent,
            crate::snippet::SNIPPET_MAX_LEN,
        );
        r.snippet = snippet_result;
    }
}

/// `~/.memex/models/`. Errors only when no home directory can be
/// resolved — broken-system territory, not a runtime case the rest of
/// the code needs to think about.
fn memex_models_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".memex/models"))
        .ok_or_else(|| {
            crate::error::MemexError::EmbeddingUnavailable {
                path: PathBuf::from("~/.memex/models"),
                reason: "cannot resolve home directory ($HOME unset?)".into(),
            }
        })
}

/// Default embedding model file path: `~/.memex/models/embedding-gemma-300m.onnx`.
pub fn default_model_path() -> Result<PathBuf> {
    Ok(memex_models_dir()?.join("embedding-gemma-300m.onnx"))
}

/// Default tokenizer file path:
/// `~/.memex/models/embedding-gemma-300m-tokenizer.json`.
pub fn default_tokenizer_path() -> Result<PathBuf> {
    Ok(memex_models_dir()?.join("embedding-gemma-300m-tokenizer.json"))
}

/// Load the default ONNX embedding model. The model, tokenizer, and
/// ONNX Runtime dylib must all be present and loadable; any missing
/// piece is a broken install and surfaces here as
/// `MemexError::EmbeddingUnavailable` so the CLI's remediation message
/// can fire.
pub fn load_default_model() -> Result<EmbeddingModel> {
    // Pre-init the ORT dylib via the explicit candidate search before
    // any code reaches `ort::Session::builder()`. The ort crate has a
    // re-entrant Once deadlock when dylib auto-discovery fails — its
    // error-construction path calls `ort::api()`, which is the same
    // Once that's mid-init. Pre-initializing here commits the API
    // pointer so subsequent calls are cache-hits.
    let model_path = default_model_path()?;
    crate::embed::init_runtime().map_err(|e| crate::error::MemexError::EmbeddingUnavailable {
        path: model_path.clone(),
        reason: format!("ONNX runtime init failed: {e}"),
    })?;
    if !model_path.exists() {
        return Err(crate::error::MemexError::EmbeddingUnavailable {
            path: model_path,
            reason: "model file not found (run `memex` plugin postinstall, \
                     or download embedding-gemma-300m.onnx into \
                     ~/.memex/models/)"
                .into(),
        });
    }

    let tokenizer_path = default_tokenizer_path()?;
    if !tokenizer_path.exists() {
        return Err(crate::error::MemexError::EmbeddingUnavailable {
            path: model_path,
            reason: format!(
                "tokenizer.json not found at {} (run the memex plugin postinstall, \
                 or download tokenizer.json into ~/.memex/models/)",
                tokenizer_path.display()
            ),
        });
    }
    let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path).map_err(|e| {
        crate::error::MemexError::EmbeddingUnavailable {
            path: model_path.clone(),
            reason: format!(
                "failed to load tokenizer from {}: {e}",
                tokenizer_path.display()
            ),
        }
    })?;

    let model_name = "embedding-gemma-300m".to_string();
    let model_path_for_load = model_path.clone();
    match catch_unwind_silent(move || load_model(&model_path_for_load, &model_name, tokenizer)) {
        Some(Ok(m)) => Ok(m),
        Some(Err(e)) => Err(crate::error::MemexError::EmbeddingUnavailable {
            path: model_path,
            reason: format!("ONNX loader failed: {e}"),
        }),
        None => Err(crate::error::MemexError::EmbeddingUnavailable {
            path: model_path,
            reason: "ONNX Runtime dylib load panicked (install libonnxruntime \
                     via `brew install onnxruntime`, your distro's package \
                     manager, or place the dylib in ~/.memex/lib/)"
                .into(),
        }),
    }
}

/// Format a query for embedding using EmbeddingGemma's task prompt.
/// Per the model card, retrieval queries should be wrapped as
/// `"task: search result | query: <text>"`. Skipping this prefix
/// produces vectors in the wrong region of the embedding space and
/// degrades retrieval relevance (~60% smaller separation gap measured
/// in `core/tests/prefix_impact_test.rs`).
pub fn format_query_for_embedding(text: &str) -> String {
    format!("task: search result | query: {text}")
}

/// Format a document chunk for embedding using EmbeddingGemma's task
/// prompt. Documents are wrapped as `"title: <title> | text: <body>"`,
/// or `"title: none | text: <body>"` when the document has no title.
/// Each chunk uses the parent document's title (chunks themselves are
/// untitled fragments).
pub fn format_passage_for_embedding(title: &str, body: &str) -> String {
    let t = if title.is_empty() { "none" } else { title };
    format!("title: {t} | text: {body}")
}

/// Embed a query string using the EmbeddingGemma query prefix.
/// Propagates any ONNX inference errors.
pub fn embed_query(model: &mut dyn Embedder, text: &str) -> Result<Vec<f32>> {
    model.embed_text(&format_query_for_embedding(text))
}

/// Batch size for `embed_document`. Eight chunks per ONNX call amortizes
/// per-call overhead well on CPU without risking OOM at the model's full
/// `EMBED_CONTEXT_SIZE` (peak activation ≈ 50 MB f32 for
/// `[8, EMBED_CONTEXT_SIZE, EMBEDDING_DIM]`).
pub const EMBED_BATCH_SIZE: usize = 8;

/// Chunk and embed a document body into the vector search index.
/// Each chunk is wrapped in EmbeddingGemma's `title: <title> | text: ...`
/// prompt before tokenization. Chunks are embedded in batches of
/// `EMBED_BATCH_SIZE` per ONNX call. Errors propagate from embedding or
/// storage failures.
pub fn embed_document(
    search: &Bm25Search,
    hash: &str,
    title: &str,
    body: &str,
    model: &mut dyn Embedder,
) -> Result<()> {
    let chunks = crate::chunking::chunk_text(body, 900, 0.15);

    // Build all prefixed inputs upfront, then embed in batches of
    // `EMBED_BATCH_SIZE` per ONNX call.
    let prompted: Vec<String> = chunks
        .iter()
        .map(|c| format_passage_for_embedding(title, &body[c.pos..c.pos + c.len]))
        .collect();
    let mut all_embeds: Vec<Vec<f32>> = Vec::with_capacity(chunks.len());
    for batch in prompted.chunks(EMBED_BATCH_SIZE) {
        let refs: Vec<&str> = batch.iter().map(|s| s.as_str()).collect();
        let embs = model.embed_batch(&refs)?;
        all_embeds.extend(embs);
    }

    let now = crate::search::now_rfc3339();
    search.with_transaction(|tx| {
        crate::vector::delete_chunks(tx, hash)?;
        for (seq, (chunk, emb)) in chunks.iter().zip(all_embeds.iter()).enumerate() {
            crate::vector::store_chunk(tx, hash, seq as i32, chunk.pos, chunk.len, emb)?;
        }
        // Stamp embed_model atomically with chunk write. Without this,
        // a tx-then-stamp split races: chunks land but the stamp lands
        // in a separate tx, so a crash, lock contention, or IO error
        // between them leaves embed_model NULL with chunks present —
        // and `outdated_chunk_hashes` filters NULL out, so a future
        // model bump won't see this doc as outdated. Stamping by hash
        // is correct because chunks are hash-keyed: every row pointing
        // at this hash shares the same chunks generated under the
        // current model.
        tx.execute(
            "UPDATE documents SET embed_model=?1, embedded_at=?2 WHERE hash=?3",
            rusqlite::params![crate::embed::CURRENT_MODEL_NAME, &now, hash],
        )?;
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
    model: &mut dyn Embedder,
    memex_root: &std::path::Path,
) -> Result<Option<String>> {
    let bm25 = search
        .search_title_only(title, "wiki", 5)
        .ok()
        .unwrap_or_default();
    if bm25.is_empty() {
        return Ok(None);
    }

    // Strong-BM25 short-circuit: if the top hit's score dominates the
    // runner-up by the standard signal threshold, the title query
    // unambiguously names that page. Skip the embed+rerank — vector
    // similarity wouldn't change the verdict and the embed cost
    // (~50ms warm, ~1.5s cold) is wasted. Mirrors the pattern
    // `hybrid_retrieve_expanded` uses for skipping LLM expansion on
    // strong primary BM25.
    let s1 = bm25.first().map(|r| r.score as f64).unwrap_or(0.0);
    let s2 = bm25.get(1).map(|r| r.score as f64).unwrap_or(0.0);
    if search::is_strong_signal(s1, s2, None) {
        return Ok(slug_from_search_result(&bm25[0]));
    }

    // Re-rank the BM25 candidates using vector similarity (fused RRF). This
    // promotes the semantically closest BM25 hit when there are several
    // (e.g. "Auth Migration" over "Auth Service").
    let title_emb = embed_query(model, title)?;
    let mut body_cache = std::collections::HashMap::new();
    // Dedup path: no user query/intent — the snippet is unused (caller
    // only inspects path/slug).
    let dedup_ctx = VecQueryCtx {
        search,
        collections: &[],
        primary_query: "",
        intent: None,
        memex_root,
    };
    let vec_wiki = vector_search_as_results(&dedup_ctx, &title_emb, "wiki", &mut body_cache)
        .ok()
        .unwrap_or_default();

    let fused = search::rrf_fuse(&[bm25, vec_wiki], &[2.0, 1.0], 60);
    let Some(best) = fused.into_iter().next() else {
        return Ok(None);
    };
    Ok(slug_from_search_result(&best))
}

/// Extract a wiki slug from a `SearchResult.path` of the form
/// `wiki/<slug>.md`. Lossy fallback is intentional — the only callers
/// are title-search paths where path comes from our own DB rows, and
/// returning the raw path on a malformed match still lets the agent
/// see something usable rather than panicking.
fn slug_from_search_result(r: &search::SearchResult) -> Option<String> {
    let path_str = r.path.to_string_lossy();
    let slug = path_str
        .strip_prefix("wiki/")
        .unwrap_or(&path_str)
        .strip_suffix(".md")
        .unwrap_or(&path_str);
    Some(slug.to_string())
}

/// Returns `true` if the file at `<memex_root>/<rel_path>` has drifted
/// from the indexed (mtime, size) — either it changed on disk or it's
/// gone entirely. A `true` result means chunk pos/len for this document
/// would slice into stale coordinates, so retrieval must drop the result
/// and let reconcile/watcher refresh the index. Default behavior on
/// detection is silent skip; the user surfaces drift via `memex lint`.
///
/// Uses `symlink_metadata` (not `metadata`) so a symlink doesn't
/// masquerade as the regular file it points to. Reconcile and watcher
/// already reject symlinks at index time, but defense-in-depth: if a
/// symlink ever does end up indexed (a regular file replaced with a
/// symlink whose target happens to have matching mtime/size), the
/// symlink's own metadata won't match the regular file's recorded
/// values, so the stat-check correctly flags it as stale.
fn is_result_stale(
    memex_root: &std::path::Path,
    rel_path: &str,
    indexed_mtime: &str,
    indexed_size: i64,
) -> bool {
    let abs = memex_root.join(rel_path);
    match std::fs::symlink_metadata(&abs) {
        Ok(meta) => {
            // Symlinks, directories, FIFOs, sockets, devices — anything
            // that isn't a plain regular file — are never legitimately
            // indexed. If one appears here, it's a swap-in by something
            // outside memex. Treat as stale rather than risk reading
            // unexpected content (e.g., a 0-byte FIFO or socket with
            // matching mtime/size happening to slip past a size/mtime
            // comparison alone).
            if !meta.file_type().is_file() {
                return true;
            }
            let on_disk_size = meta.len() as i64;
            let on_disk_mtime = crate::storage::file_mtime_iso(&abs);
            on_disk_size != indexed_size || on_disk_mtime != indexed_mtime
        }
        Err(_) => true, // file missing → always stale
    }
}

/// Map chunk-level vector search hits to document-level SearchResults.
///
/// Materializes a focused snippet for each result by reading the doc body
/// from disk and slicing around the matching chunk via `(pos, len)`. The
/// snippet is a diff-style header + numbered window picked by
/// `crate::snippet::extract_focused_snippet`, optionally biased toward
/// `intent`-bearing lines. Non-query callers can pass `intent = None`
/// and `primary_query = ""` — the line picker falls through to line 0
/// in that case.
/// Per-query context bundled for `vector_search_as_results`. Stable for
/// the lifetime of one user query — only the embedding + doc_type vary
/// across the wiki/raw fan-out, so they stay positional args.
pub struct VecQueryCtx<'a> {
    pub search: &'a Bm25Search,
    pub collections: &'a [String],
    pub primary_query: &'a str,
    pub intent: Option<&'a str>,
    pub memex_root: &'a std::path::Path,
}

pub fn vector_search_as_results(
    ctx: &VecQueryCtx<'_>,
    q_emb: &[f32],
    doc_type: &str,
    body_cache: &mut std::collections::HashMap<(String, String), String>,
) -> Result<Vec<SearchResult>> {
    let VecQueryCtx {
        search,
        collections,
        primary_query,
        intent,
        memex_root,
    } = *ctx;
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

    // Atomic batched lookup: docs by hash + memberships keyed by doc id,
    // both read under one connection-lock so a concurrent delete can't
    // resurrect a doc as "default" via the membership-fallback path.
    // Replaces the prior two-call flow (lookup_documents_by_hashes +
    // collections_by_document_ids) which had that race.
    let unique_hashes: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        vec_results
            .iter()
            .filter_map(|vr| {
                if seen.insert(vr.hash.clone()) {
                    Some(vr.hash.clone())
                } else {
                    None
                }
            })
            .collect()
    };
    let (docs_by_hash, memberships_by_id, meta_by_id) =
        search.lookup_documents_with_collections(&unique_hashes)?;

    for vr in &vec_results {
        let Some(docs) = docs_by_hash.get(&vr.hash) else {
            continue;
        };
        for doc in docs {
            if !selected.is_empty()
                && let Some(memberships) = memberships_by_id.get(&doc.id)
                && !memberships.iter().any(|name| selected.contains(name))
            {
                continue;
            }
            // Stat-check: drop results whose file changed since
            // indexing — chunk pos/len would slice into stale
            // coordinates. Skip silently; the watcher or next reconcile
            // refreshes the index, and `memex lint` surfaces the drift
            // if the user asks.
            if let Some((indexed_mtime, indexed_size)) = meta_by_id.get(&doc.id)
                && is_result_stale(memex_root, &doc.path, indexed_mtime, *indexed_size)
            {
                continue;
            }
            let cache_key = (doc.doc_type.clone(), doc.path.clone());
            let body = body_cache.entry(cache_key).or_insert_with(|| {
                match crate::read_body_from_disk(memex_root, &doc.doc_type, &doc.path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            doc_type = %doc.doc_type,
                            path = %doc.path,
                            error = %e,
                            "retrieval: read body failed; falling back to empty snippet"
                        );
                        String::new()
                    }
                }
            });
            let snippet_result = crate::snippet::extract_focused_snippet(
                body,
                vr.pos,
                vr.len,
                primary_query,
                intent,
                crate::snippet::SNIPPET_MAX_LEN,
            );
            out.push(SearchResult {
                path: PathBuf::from(&doc.path),
                title: doc.title.clone(),
                score: vr.score,
                snippet: snippet_result,
                doc_type: doc.doc_type.clone(),
                hash: vr.hash.clone(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::is_result_stale;
    use tempfile::TempDir;

    /// embed_document must commit chunks AND embed_model in a single
    /// transaction. Without this, a separate-tx stamp could fail (DB
    /// lock, IO) after chunks landed, leaving the row's embed_model
    /// at NULL — and outdated_chunk_hashes filters NULL out, so a
    /// future model bump never re-embeds the doc.
    #[test]
    fn embed_document_stamps_embed_model_atomically() {
        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let search = memex.search();

        // Insert a row with NULL embed_model (mirrors the state right
        // after commit_doc INSERT — the bug surface).
        let body = "atomic stamp test body";
        let hash = crate::storage::content_hash(body.as_bytes());
        search
            .with_transaction(|tx| {
                tx.execute(
                    "INSERT INTO documents (doc_type, path, title, hash, tags, source, mtime, size) \
                     VALUES ('wiki', 'wiki/atomic.md', 'Atomic', ?1, '', NULL, '2026-04-30T00:00:00Z', ?2)",
                    rusqlite::params![&hash, body.len() as i64],
                )?;
                Ok(())
            })
            .unwrap();

        // Pre-state: embed_model is NULL.
        let pre: Option<String> = search
            .with_connection(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT embed_model FROM documents WHERE path='wiki/atomic.md'",
                        [],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .unwrap())
            })
            .unwrap();
        assert!(pre.is_none(), "test setup: embed_model should be NULL");

        // Act: embed_document.
        let mut model = crate::embed::MockEmbedder;
        crate::retrieval::embed_document(search, &hash, "Atomic", body, &mut model).unwrap();

        // Post-state: embed_model is now CURRENT_MODEL_NAME.
        let post: Option<String> = search
            .with_connection(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT embed_model FROM documents WHERE path='wiki/atomic.md'",
                        [],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .unwrap())
            })
            .unwrap();
        assert_eq!(
            post.as_deref(),
            Some(crate::embed::CURRENT_MODEL_NAME),
            "embed_document must atomically stamp embed_model"
        );
    }

    #[test]
    fn stat_check_passes_when_file_matches_indexed() {
        let dir = TempDir::new().unwrap();
        let rel = "wiki/page.md";
        let abs = dir.path().join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "body content").unwrap();
        let mtime = crate::storage::file_mtime_iso(&abs);
        let size = std::fs::metadata(&abs).unwrap().len() as i64;
        assert!(
            !is_result_stale(dir.path(), rel, &mtime, size),
            "matching mtime+size => not stale"
        );
    }

    #[test]
    fn stat_check_flags_stale_when_size_changes() {
        let dir = TempDir::new().unwrap();
        let rel = "wiki/page.md";
        let abs = dir.path().join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "body content").unwrap();
        let mtime = crate::storage::file_mtime_iso(&abs);
        let indexed_size = 9999i64; // pretend index thinks it's a different size
        assert!(
            is_result_stale(dir.path(), rel, &mtime, indexed_size),
            "size mismatch => stale"
        );
    }

    /// Vector search returns a hit; the file is then modified externally;
    /// the next vector search drops the hit because the chunk pos/len
    /// no longer correspond to the on-disk body. Drives the full
    /// `vector_search_as_results` path.
    #[test]
    fn vector_search_drops_stale_results_after_external_edit() {
        use crate::retrieval::{VecQueryCtx, vector_search_as_results};
        use std::path::Path;

        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let body = "Bearer tokens authenticate API requests with short-lived credentials.";
        // Index a wiki document (the on-disk content + DB row + mtime/size).
        std::fs::write(
            memex.wiki_dir().join("auth.md"),
            format!("---\ntitle: Auth\ntags: []\nsources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\n{body}"),
        )
        .unwrap();
        crate::index_wiki::index_wiki_file(
            &memex,
            &memex.wiki_dir().join("auth.md"),
            None,
        )
        .unwrap();
        let body_hash = crate::storage::content_hash(body.as_bytes());

        // Manually wire a single chunk_vec row with a fixture vector.
        let fixture: Vec<f32> = vec![0.1; crate::embed::EMBEDDING_DIM];
        memex
            .search()
            .with_connection(|conn| {
                crate::vector::store_chunk(conn, &body_hash, 0, 0, body.len(), &fixture)?;
                Ok(())
            })
            .unwrap();

        let collections: [String; 0] = [];
        let mut body_cache = std::collections::HashMap::new();
        let ctx = VecQueryCtx {
            search: memex.search(),
            collections: &collections,
            primary_query: "tokens",
            intent: None,
            memex_root: memex.root(),
        };
        let pre = vector_search_as_results(&ctx, &fixture, "wiki", &mut body_cache).unwrap();
        assert_eq!(pre.len(), 1, "indexed file with matching vector should hit");
        assert_eq!(pre[0].path, Path::new("wiki/auth.md"));

        // Externally rewrite the body. Same path, different size — the
        // stat-check must drop the result rather than slice into stale
        // pos/len coordinates.
        std::fs::write(
            memex.wiki_dir().join("auth.md"),
            format!("---\ntitle: Auth\ntags: []\nsources: []\ncreated_at: 2026-04-26T00:00:00Z\nupdated_at: 2026-04-26T00:00:00Z\n---\n\n{body} ADDITIONAL EDITED CONTENT"),
        )
        .unwrap();
        let mut body_cache2 = std::collections::HashMap::new();
        let post = vector_search_as_results(&ctx, &fixture, "wiki", &mut body_cache2).unwrap();
        assert!(
            post.is_empty(),
            "stale-result stat-check should drop the hit; got {post:?}"
        );
    }

    /// Regression: the BM25 path also needs the stat-check (caught by
    /// strict e2e — kernel-notes was still surfacing in BM25 results
    /// after the indexed mtime was corrupted to a stale value, even
    /// though vector_search_as_results correctly dropped it). Drives
    /// `hybrid_retrieve_expanded` with an empty embedding so only the
    /// BM25 path runs.
    #[test]
    fn bm25_drops_stale_results_too() {
        use crate::retrieval::{Expansion, hybrid_retrieve_expanded};

        let dir = TempDir::new().unwrap();
        let memex = crate::Memex::open_writer(dir.path().to_path_buf()).unwrap();
        let body = "Working with the linux kernel internals requires deep knowledge.";
        std::fs::write(
            memex.wiki_dir().join("kernel-notes.md"),
            format!("---\ntitle: Kernel Notes\ntags: []\nsources: []\ncreated_at: 2026-04-29T00:00:00Z\nupdated_at: 2026-04-29T00:00:00Z\n---\n\n{body}"),
        )
        .unwrap();
        crate::index_wiki::index_wiki_file(
            &memex,
            &memex.wiki_dir().join("kernel-notes.md"),
            None,
        )
        .unwrap();

        let collections: [String; 0] = [];
        let exp = Expansion::default();
        // Sanity: BM25 finds the page before corruption.
        let pre = hybrid_retrieve_expanded(
            memex.search(),
            "kernel internals",
            &[],
            &exp,
            &collections,
            None,
            memex.root(),
        )
        .unwrap();
        assert_eq!(pre.results.len(), 1, "baseline: BM25 finds kernel-notes");

        // Corrupt the indexed mtime — file on disk is fresh, DB says ancient.
        memex
            .search()
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE documents SET mtime='1900-01-01T00:00:00Z' \
                     WHERE doc_type='wiki' AND path='wiki/kernel-notes.md'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        // After corruption: stat-check must drop the BM25 hit.
        let post = hybrid_retrieve_expanded(
            memex.search(),
            "kernel internals",
            &[],
            &exp,
            &collections,
            None,
            memex.root(),
        )
        .unwrap();
        assert!(
            post.results.is_empty(),
            "BM25 stat-check should drop the stale result; got {:?}",
            post.results.iter().map(|r| &r.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn stat_check_flags_stale_when_file_missing() {
        let dir = TempDir::new().unwrap();
        assert!(
            is_result_stale(dir.path(), "wiki/gone.md", "2026-04-30T00:00:00Z", 100),
            "missing file => stale"
        );
    }

    /// A symlink in the tree (typically a swap-in from outside memex)
    /// must be treated as stale even when the target's mtime/size
    /// matches the indexed values — otherwise an attacker who controls
    /// the target file could craft matching metadata to read its
    /// contents through `memex query`.
    #[cfg(unix)]
    #[test]
    fn stat_check_flags_stale_when_path_is_symlink() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.md");
        std::fs::write(&target, "regular body").unwrap();
        let target_meta = std::fs::metadata(&target).unwrap();
        let target_mtime = crate::storage::file_mtime_iso(&target);
        let target_size = target_meta.len() as i64;

        // Symlink wiki/link.md → target.md within the same dir.
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        std::os::unix::fs::symlink(&target, wiki.join("link.md")).unwrap();

        // Caller passes the SYMLINK target's mtime/size as if indexed.
        // Without symlink-aware checking, the function would follow the
        // symlink and report "not stale" because target metadata matches.
        assert!(
            is_result_stale(dir.path(), "wiki/link.md", &target_mtime, target_size),
            "symlink with target-matching metadata must still be flagged stale"
        );
    }
}

