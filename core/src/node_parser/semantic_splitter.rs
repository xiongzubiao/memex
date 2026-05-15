//! Faithful Rust port of LlamaIndex's `SemanticSplitterNodeParser`.
//!
//! Source:
//! https://github.com/run-llama/llama_index/blob/main/llama-index-core/llama_index/core/node_parser/text/semantic_splitter.py
//!
//! Algorithm (verbatim):
//!   1. `text_splits = sentence_splitter(text)` — sentence boundaries.
//!   2. `_build_sentence_groups`: for each i, `combined[i]` =
//!      concat of `sentences[i-buffer_size..i]` + `sentences[i]` +
//!      `sentences[i+1..i+1+buffer_size]` (clamped to bounds).
//!   3. `embed_model.get_text_embedding_batch(combined_sentences)`.
//!   4. `_calculate_distances_between_sentence_groups`:
//!      `distance[i] = 1 - similarity(combined[i], combined[i+1])`.
//!   5. `_build_node_chunks`:
//!        threshold = numpy.percentile(distances, percentile_threshold)
//!        breakpoints = [i for i, d in enumerate(distances) if d > threshold]
//!        slice sentences at each breakpoint.
//!
//! `embed_model.similarity` defaults to cosine similarity in LlamaIndex.

use crate::error::Result;

/// Mirror LlamaIndex's `buffer_size: int = Field(default=1)`. With
/// buffer=1, each combined sentence is `[prev, curr, next]`.
pub const DEFAULT_BUFFER_SIZE: usize = 1;

/// Mirror LlamaIndex's `breakpoint_percentile_threshold: int = Field(default=95)`.
/// Higher percentile = fewer breakpoints = larger chunks. Empirically
/// best for our corpus: larger chunks keep related facts together where
/// the answerer can use them coherently, while smaller chunks (P88-P90)
/// fragment relevant content and reduce retrieval recall.
pub const DEFAULT_BREAKPOINT_PERCENTILE_THRESHOLD: f32 = 95.0;

/// One chunk produced by the splitter, as `(pos, len)` byte offsets
/// into the input text. LlamaIndex returns `TextNode` objects with
/// the chunk's text copied; offsets are an addition for in-place
/// extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticChunk {
    pub pos: usize,
    pub len: usize,
}

/// Faithful port of
/// `SemanticSplitterNodeParser.build_semantic_nodes_from_documents`
/// for a single document.
///
/// `sentence_splitter` mirrors the `sentence_splitter: Callable` field
/// — it returns `(start, end)` byte spans into `text`. LlamaIndex's
/// default is `split_by_sentence_tokenizer()` (NLTK Punkt); pass any
/// substitute (memex uses `sentencex::get_sentence_boundaries`).
pub fn build_semantic_nodes_from_text<F>(
    text: &str,
    embed_model: &mut dyn crate::embed::Embedder,
    buffer_size: usize,
    breakpoint_percentile_threshold: f32,
    sentence_splitter: F,
) -> Result<Vec<SemanticChunk>>
where
    F: Fn(&str) -> Vec<(usize, usize)>,
{
    let spans = sentence_splitter(text);
    if spans.is_empty() {
        return Ok(Vec::new());
    }

    // Mirror LlamaIndex's `text_splits` (raw sentence strings).
    let sentences: Vec<&str> = spans.iter().map(|&(s, e)| &text[s..e]).collect();
    let combined = build_sentence_groups(&sentences, buffer_size);
    let combined_refs: Vec<&str> = combined.iter().map(String::as_str).collect();
    // `combined_sentence_embeddings = embed_model.get_text_embedding_batch(...)`.
    let embs = embed_model.embed_batch(&combined_refs)?;

    let distances = calculate_distances_between_sentence_groups(&embs);
    Ok(build_node_chunks(
        &spans,
        text.len(),
        &distances,
        breakpoint_percentile_threshold,
    ))
}

/// Mirror `_build_sentence_groups`. `sentences[i].combined_sentence` =
/// concat of `sentences[max(0, i-buffer_size) .. i]`, then
/// `sentences[i]`, then `sentences[i+1 .. min(n, i+1+buffer_size)]`.
fn build_sentence_groups(sentences: &[&str], buffer_size: usize) -> Vec<String> {
    let n = sentences.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut combined = String::new();
        let lo = i.saturating_sub(buffer_size);
        for j in lo..i {
            combined.push_str(sentences[j]);
        }
        combined.push_str(sentences[i]);
        let hi = (i + 1 + buffer_size).min(n);
        for j in (i + 1)..hi {
            combined.push_str(sentences[j]);
        }
        out.push(combined);
    }
    out
}

/// Mirror `_calculate_distances_between_sentence_groups`:
/// `distance[i] = 1 - similarity(emb[i], emb[i+1])` for `i in 0..n-1`.
/// `embed_model.similarity` defaults to cosine in LlamaIndex.
fn calculate_distances_between_sentence_groups(embs: &[Vec<f32>]) -> Vec<f32> {
    if embs.len() < 2 {
        return Vec::new();
    }
    (0..embs.len() - 1)
        .map(|i| 1.0 - cosine_similarity(&embs[i], &embs[i + 1]))
        .collect()
}

/// Mirror `_build_node_chunks`. Returns chunks as `(pos, len)` byte
/// ranges into the original text (LlamaIndex returns concatenated
/// sentence strings; here we return offsets so the caller can extract
/// without copying).
fn build_node_chunks(
    spans: &[(usize, usize)],
    text_len: usize,
    distances: &[f32],
    percentile_threshold: f32,
) -> Vec<SemanticChunk> {
    let n = spans.len();
    if n == 0 {
        return Vec::new();
    }
    if distances.is_empty() {
        // LlamaIndex fallback: `chunks = [" ".join([s["sentence"] ...])]`.
        // Single chunk covering the whole text (the join is a no-op for
        // a single sentence, and offset-wise we just use [0, text_len)).
        return vec![SemanticChunk {
            pos: 0,
            len: text_len,
        }];
    }
    let threshold = numpy_percentile(distances, percentile_threshold);
    // `[i for i, x in enumerate(distances) if x > threshold]` — strict `>`.
    let breakpoints: Vec<usize> = distances
        .iter()
        .enumerate()
        .filter(|&(_, &d)| d > threshold)
        .map(|(i, _)| i)
        .collect();

    let mut chunks: Vec<SemanticChunk> = Vec::new();
    let mut start_idx: usize = 0;
    for bp in breakpoints {
        // Group: sentences[start_idx ..= bp]. Pos = first sentence's
        // start; end = next sentence's start (or text_len if last).
        let pos = spans[start_idx].0;
        let end = if bp + 1 < n {
            spans[bp + 1].0
        } else {
            text_len
        };
        if end > pos {
            chunks.push(SemanticChunk {
                pos,
                len: end - pos,
            });
        }
        start_idx = bp + 1;
    }
    // Trailing group.
    if start_idx < n {
        let pos = spans[start_idx].0;
        if text_len > pos {
            chunks.push(SemanticChunk {
                pos,
                len: text_len - pos,
            });
        }
    }
    if chunks.is_empty() {
        chunks.push(SemanticChunk {
            pos: 0,
            len: text_len,
        });
    }
    chunks
}

/// Cosine similarity. LlamaIndex's `embed_model.similarity` defaults to
/// cosine.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    let len = a.len().min(b.len());
    for i in 0..len {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Mirror `numpy.percentile(values, p)` with default linear
/// interpolation (`method="linear"`).
fn numpy_percentile(values: &[f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f32> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if sorted.len() == 1 {
        return sorted[0];
    }
    let h = (p / 100.0) * (sorted.len() - 1) as f32;
    let lo = h.floor() as usize;
    let hi = h.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        sorted[lo] + (h - lo as f32) * (sorted[hi] - sorted[lo])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct StubEmbedder {
        map: HashMap<String, Vec<f32>>,
    }

    impl crate::embed::Embedder for StubEmbedder {
        fn model_name(&self) -> &str {
            "stub"
        }
        fn embed_text(&mut self, text: &str) -> Result<Vec<f32>> {
            Ok(self.map.get(text).cloned().unwrap_or_else(|| vec![1.0]))
        }
        fn embed_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| self.map.get(*t).cloned().unwrap_or_else(|| vec![1.0]))
                .collect())
        }
        fn count_tokens(&mut self, text: &str) -> Result<usize> {
            Ok(text.split_whitespace().count())
        }
        fn max_input_tokens(&self) -> usize {
            usize::MAX
        }
    }

    fn whole_text_splitter(text: &str) -> Vec<(usize, usize)> {
        // Trivial splitter: returns one span per non-empty paragraph.
        let mut spans = Vec::new();
        let mut start = 0;
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\n' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                if i > start {
                    spans.push((start, i));
                }
                i += 2;
                start = i;
                continue;
            }
            i += 1;
        }
        if start < bytes.len() {
            spans.push((start, bytes.len()));
        }
        spans
    }

    #[test]
    fn build_sentence_groups_buffer_one() {
        let s = vec!["A. ", "B. ", "C. ", "D. "];
        let combined = build_sentence_groups(&s, 1);
        assert_eq!(combined[0], "A. B. ");
        assert_eq!(combined[1], "A. B. C. ");
        assert_eq!(combined[2], "B. C. D. ");
        assert_eq!(combined[3], "C. D. ");
    }

    #[test]
    fn build_sentence_groups_buffer_zero_is_identity() {
        let s = vec!["A", "B", "C"];
        let combined = build_sentence_groups(&s, 0);
        assert_eq!(combined, vec!["A".to_string(), "B".into(), "C".into()]);
    }

    #[test]
    fn numpy_percentile_linear_interpolation() {
        let vals = vec![1.0, 2.0, 3.0, 4.0];
        // 50th: h = 0.5 * 3 = 1.5 → 2 + 0.5*(3-2) = 2.5
        assert!((numpy_percentile(&vals, 50.0) - 2.5).abs() < 1e-5);
        // 95th: h = 0.95 * 3 = 2.85 → 3 + 0.85*(4-3) = 3.85
        assert!((numpy_percentile(&vals, 95.0) - 3.85).abs() < 1e-5);
        // 100th: h = 3.0 → 4.0
        assert!((numpy_percentile(&vals, 100.0) - 4.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_orthogonal_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_parallel_is_one() {
        let a = vec![1.0, 0.0];
        let b = vec![2.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn build_node_chunks_no_breakpoints_returns_single_chunk() {
        let spans = vec![(0, 10), (10, 20), (20, 30)];
        let dists = vec![0.1, 0.1];
        // 95th percentile of [0.1, 0.1] = 0.1; nothing strictly > 0.1.
        let chunks = build_node_chunks(&spans, 30, &dists, 95.0);
        assert_eq!(chunks, vec![SemanticChunk { pos: 0, len: 30 }]);
    }

    #[test]
    fn build_node_chunks_breakpoint_splits_at_sentence_boundary() {
        let spans = vec![(0, 10), (10, 20), (20, 30), (30, 40)];
        // distances[1] = 0.9 will be > 95th-percentile.
        let dists = vec![0.1, 0.9, 0.1];
        let chunks = build_node_chunks(&spans, 40, &dists, 95.0);
        // 95th of [0.1, 0.9, 0.1] sorted [0.1, 0.1, 0.9]:
        // h = 0.95 * 2 = 1.9 → between 0.1 and 0.9: 0.1 + 0.9*(0.9-0.1) = 0.82.
        // 0.9 > 0.82 → break at distance index 1.
        // Chunk 1: sentences 0..=1 → pos=0, end=spans[2].0=20 → (0, 20).
        // Chunk 2: sentences 2..=3 → pos=20, end=40 → (20, 20).
        assert_eq!(
            chunks,
            vec![
                SemanticChunk { pos: 0, len: 20 },
                SemanticChunk { pos: 20, len: 20 },
            ]
        );
    }

    #[test]
    fn end_to_end_splits_at_topic_change() {
        let text = "Cats are soft.\n\nCats purr loudly.\n\nCars are fast.\n\nCars need fuel.";
        // whole_text_splitter splits on \n\n → 4 sentences (paragraphs).
        let mut map = HashMap::new();
        // buffer=1 combined sentences — order matters; concat with no sep.
        // s0="Cats are soft.", s1="Cats purr loudly.", s2="Cars are fast.", s3="Cars need fuel."
        map.insert(
            "Cats are soft.Cats purr loudly.".to_string(),
            vec![1.0, 0.0],
        );
        map.insert(
            "Cats are soft.Cats purr loudly.Cars are fast.".to_string(),
            vec![1.0, 0.0],
        );
        map.insert(
            "Cats purr loudly.Cars are fast.Cars need fuel.".to_string(),
            vec![0.0, 1.0],
        );
        map.insert("Cars are fast.Cars need fuel.".to_string(), vec![0.0, 1.0]);
        let mut emb = StubEmbedder { map };
        let chunks =
            build_semantic_nodes_from_text(text, &mut emb, 1, 95.0, whole_text_splitter).unwrap();
        assert_eq!(chunks.len(), 2, "expected 2 chunks: {chunks:?}");
        let c0 = &text[chunks[0].pos..chunks[0].pos + chunks[0].len];
        let c1 = &text[chunks[1].pos..chunks[1].pos + chunks[1].len];
        assert!(c0.contains("Cats"));
        assert!(c1.contains("Cars"));
        assert!(!c0.contains("Cars"));
    }

    #[test]
    fn empty_text_returns_no_chunks() {
        let mut emb = StubEmbedder {
            map: HashMap::new(),
        };
        let chunks =
            build_semantic_nodes_from_text("", &mut emb, 1, 95.0, whole_text_splitter).unwrap();
        assert_eq!(chunks.len(), 0);
    }

    #[test]
    fn single_sentence_returns_one_chunk_no_embedding_needed() {
        let mut emb = StubEmbedder {
            map: HashMap::new(),
        };
        let text = "Just one paragraph.";
        let chunks =
            build_semantic_nodes_from_text(text, &mut emb, 1, 95.0, whole_text_splitter).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].pos, 0);
        assert_eq!(chunks[0].len, text.len());
    }
}
