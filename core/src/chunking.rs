//! Composition layer over the LlamaIndex node-parser ports
//! (`node_parser::markdown`, `node_parser::semantic_splitter`,
//! `node_parser::sentence`). Owns the `(pos, len)` `Chunk` type and the
//! pipeline used by `commit_doc` / `embed_document`:
//!
//!   1. **MarkdownNodeParser** — split on headers (no embedder needed).
//!   2. **SemanticSplitterNodeParser** — split each section at
//!      semantic-distance breakpoints (needs embedder).
//!   3. **SentenceSplitter token-budget pass** — re-split any chunk
//!      whose token count exceeds `embedder.max_input_tokens()`. Without
//!      this, `embed_batch` silently truncates oversized chunks at the
//!      embedder limit, dropping the tail content from the vector —
//!      invisible to vector retrieval.

use crate::error::Result;
use crate::node_parser::{markdown, semantic_splitter, sentence};

#[derive(Debug, Clone, Copy)]
pub struct Chunk {
    pub pos: usize,
    pub len: usize,
}

/// Sentencex-backed sentence splitter; the substitute for LlamaIndex's
/// NLTK Punkt slot. Used by both the semantic splitter and the
/// SentenceSplitter cascade.
///
/// Drops empty + paragraph-break-only spans: sentencex is
/// non-destructive and emits each `\n\n` gap as its own span. Those
/// spans are pure whitespace and would become trivial chunks (BM25
/// noise) if kept.
fn sentence_spans(text: &str) -> Vec<(usize, usize)> {
    sentencex::get_sentence_boundaries("en", text)
        .into_iter()
        .filter(|b| !b.is_paragraph_break)
        .map(|b| (b.start_byte, b.end_byte))
        .filter(|&(s, e)| e > s && !text[s..e].trim().is_empty())
        .collect()
}

/// Markdown-only chunking: one `Chunk` per `MarkdownNode`. Used by
/// `commit_doc`, which writes chunks_fts before any embedder is
/// available.
pub fn chunk_markdown(body: &str) -> Vec<Chunk> {
    markdown::get_nodes_from_text(body, markdown::DEFAULT_HEADER_PATH_SEPARATOR)
        .into_iter()
        .map(|n| Chunk {
            pos: n.pos,
            len: n.len,
        })
        .collect()
}

/// Full pipeline: markdown → semantic → SentenceSplitter token-budget.
/// The canonical chunk set stored for retrieval. Every chunk's tokens
/// plus `prompt_prefix`'s tokens fit within the embedder's context
/// window, so callers that prepend the prefix at embed time never
/// overflow. Pass `""` if no prefix will be added.
pub fn chunk_full_pipeline(
    body: &str,
    embedder: &mut dyn crate::embed::Embedder,
    prompt_prefix: &str,
) -> Result<Vec<Chunk>> {
    let nodes = markdown::get_nodes_from_text(body, markdown::DEFAULT_HEADER_PATH_SEPARATOR);
    let mut semantic_chunks: Vec<Chunk> = Vec::new();
    for node in nodes {
        let section = &body[node.pos..node.pos + node.len];
        let sub = semantic_splitter::build_semantic_nodes_from_text(
            section,
            embedder,
            semantic_splitter::DEFAULT_BUFFER_SIZE,
            semantic_splitter::DEFAULT_BREAKPOINT_PERCENTILE_THRESHOLD,
            sentence_spans,
        )?;
        for sc in sub {
            semantic_chunks.push(Chunk {
                pos: node.pos + sc.pos,
                len: sc.len,
            });
        }
    }
    enforce_token_budget(body, semantic_chunks, embedder, prompt_prefix)
}

/// Re-split chunks whose token count exceeds the effective budget
/// (`max_input_tokens() - prefix_tokens`). Splits use `SentenceSplitter`
/// (paragraph → sentence → regex → word → char cascade) so cuts prefer
/// natural boundaries.
fn enforce_token_budget(
    body: &str,
    chunks: Vec<Chunk>,
    embedder: &mut dyn crate::embed::Embedder,
    prompt_prefix: &str,
) -> Result<Vec<Chunk>> {
    let raw_budget = embedder.max_input_tokens();
    let prefix_tokens = if prompt_prefix.is_empty() {
        0
    } else {
        embedder.count_tokens(prompt_prefix)?
    };
    let budget = raw_budget.saturating_sub(prefix_tokens);
    let splitter = sentence::SentenceSplitter {
        chunk_size: budget,
        ..Default::default()
    };
    let mut out: Vec<Chunk> = Vec::with_capacity(chunks.len());
    for ch in chunks {
        let text = &body[ch.pos..ch.pos + ch.len];
        let token_count = embedder.count_tokens(text)?;
        if token_count <= budget {
            out.push(ch);
            continue;
        }
        let mut count_fn = |t: &str| -> usize {
            embedder
                .count_tokens(t)
                .unwrap_or_else(|_| t.split_whitespace().count())
        };
        let sub_chunks = splitter.split_text(text, &mut count_fn, &sentence_spans);
        if sub_chunks.is_empty() {
            out.push(ch);
            continue;
        }
        for sc in sub_chunks {
            out.push(Chunk {
                pos: ch.pos + sc.pos,
                len: sc.len,
            });
        }
    }
    Ok(out)
}
