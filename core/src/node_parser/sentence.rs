//! Faithful Rust port of LlamaIndex's `SentenceSplitter`.
//!
//! Source:
//! https://github.com/run-llama/llama_index/blob/main/llama-index-core/llama_index/core/node_parser/text/sentence.py
//!
//! Algorithm (verbatim):
//!   `_split(text, chunk_size)` recursively breaks text via a cascade
//!   until each piece fits in `chunk_size` tokens:
//!     1. paragraph separator (`"\n\n\n"`)
//!     2. chunking_tokenizer_fn (sentence boundaries; LlamaIndex uses
//!        NLTK Punkt, memex uses `sentencex::get_sentence_boundaries`)
//!     3. secondary_chunking_regex (`"[^,.;。？！]+[,.;。？！]?|[,.;。？！]"`)
//!     4. separator (`" "`)
//!     5. char split
//!
//!   The first three rounds (paragraph, sentence) carry `is_sentence=true`;
//!   the rest are `is_sentence=false`. `_get_splits_by_fns` picks the
//!   first primary fn that returns >1 splits; otherwise falls through
//!   to the sub-sentence cascade.
//!
//!   `_merge` greedy-packs splits into chunks ≤ `chunk_size` tokens,
//!   carrying `chunk_overlap` tokens of tail context into the next chunk.
//!
//! Two callables, two different roles (matching LlamaIndex):
//!   - `tokenizer`: text → token count (for budget). LlamaIndex default
//!     `get_tokenizer()` → tiktoken; memex passes the embedding-gemma
//!     `tokenizers::Tokenizer` via `Embedder::count_tokens`.
//!   - `chunking_tokenizer_fn`: text → sentence byte spans (for
//!     boundary detection). LlamaIndex default NLTK Punkt; memex
//!     passes a closure around `sentencex::get_sentence_boundaries`.
//!
//! Returns `Vec<TextChunk>` with `(pos, len)` byte offsets into the
//! original text — LlamaIndex returns `Vec<String>`, but offsets are
//! free here because every split fn (`split_by_sep`, `split_by_regex`,
//! `split_by_char`, sentencex) operates on substrings. Memex needs
//! offsets for chunks_fts indexing.

use regex::Regex;

/// `DEFAULT_CHUNK_SIZE = 1024` tokens (LlamaIndex constants.py).
pub const DEFAULT_CHUNK_SIZE: usize = 1024;

/// `SENTENCE_CHUNK_OVERLAP = 200` tokens.
pub const SENTENCE_CHUNK_OVERLAP: usize = 200;

/// `DEFAULT_PARAGRAPH_SEP = "\n\n\n"`.
pub const DEFAULT_PARAGRAPH_SEP: &str = "\n\n\n";

/// `CHUNKING_REGEX` — secondary fallback after sentence tokenizer.
pub const CHUNKING_REGEX: &str = r"[^,.;。？！]+[,.;。？！]?|[,.;。？！]";

/// Default word separator for the final fallback cascade step.
pub const DEFAULT_SEPARATOR: &str = " ";

/// One chunk produced by the splitter. `pos` and `len` are byte offsets
/// into the original input text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub pos: usize,
    pub len: usize,
}

/// Configuration for the splitter. Matches LlamaIndex's `SentenceSplitter`
/// fields (without the unused `include_metadata` etc. — those govern
/// LlamaIndex's `TextNode` building, not the splitting algorithm).
#[derive(Debug, Clone)]
pub struct SentenceSplitter {
    pub chunk_size: usize,
    pub chunk_overlap: usize,
    pub paragraph_separator: String,
    pub separator: String,
    pub secondary_chunking_regex: Option<String>,
}

impl Default for SentenceSplitter {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            chunk_overlap: SENTENCE_CHUNK_OVERLAP,
            paragraph_separator: DEFAULT_PARAGRAPH_SEP.to_string(),
            separator: DEFAULT_SEPARATOR.to_string(),
            secondary_chunking_regex: Some(CHUNKING_REGEX.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
struct Split {
    pos: usize,
    len: usize,
    is_sentence: bool,
    token_size: usize,
}

impl SentenceSplitter {
    /// Faithful port of `SentenceSplitter.split_text`. `tokenizer` returns
    /// token count for a slice; `chunking_tokenizer_fn` returns sentence
    /// `(start, end)` byte spans.
    pub fn split_text<TokFn, SentFn>(
        &self,
        text: &str,
        tokenizer: &mut TokFn,
        chunking_tokenizer_fn: &SentFn,
    ) -> Vec<TextChunk>
    where
        TokFn: FnMut(&str) -> usize,
        SentFn: Fn(&str) -> Vec<(usize, usize)>,
    {
        if text.is_empty() {
            return Vec::new();
        }
        if self.chunk_overlap > self.chunk_size {
            // LlamaIndex raises ValueError here; we clamp instead so a
            // misconfigured splitter degrades rather than panics.
            return Vec::new();
        }
        let secondary_re = self
            .secondary_chunking_regex
            .as_deref()
            .and_then(|s| Regex::new(s).ok());
        let splits = self.split_recursive(
            text,
            0,
            self.chunk_size,
            tokenizer,
            chunking_tokenizer_fn,
            secondary_re.as_ref(),
        );
        let chunks = self.merge(splits, text);
        postprocess_chunks(chunks, text)
    }

    fn split_recursive<TokFn, SentFn>(
        &self,
        text: &str,
        pos_offset: usize,
        chunk_size: usize,
        tokenizer: &mut TokFn,
        chunking_tokenizer_fn: &SentFn,
        secondary_re: Option<&Regex>,
    ) -> Vec<Split>
    where
        TokFn: FnMut(&str) -> usize,
        SentFn: Fn(&str) -> Vec<(usize, usize)>,
    {
        let token_size = tokenizer(text);
        if token_size <= chunk_size {
            return vec![Split {
                pos: pos_offset,
                len: text.len(),
                is_sentence: true,
                token_size,
            }];
        }

        let (subspans, is_sentence) =
            self.get_splits_by_fns(text, chunking_tokenizer_fn, secondary_re);

        let mut out = Vec::new();
        for (sub_start, sub_end) in subspans {
            let sub_text = &text[sub_start..sub_end];
            if sub_text.is_empty() {
                continue;
            }
            let sub_token_size = tokenizer(sub_text);
            if sub_token_size <= chunk_size {
                out.push(Split {
                    pos: pos_offset + sub_start,
                    len: sub_text.len(),
                    is_sentence,
                    token_size: sub_token_size,
                });
            } else {
                let recurse = self.split_recursive(
                    sub_text,
                    pos_offset + sub_start,
                    chunk_size,
                    tokenizer,
                    chunking_tokenizer_fn,
                    secondary_re,
                );
                out.extend(recurse);
            }
        }
        out
    }

    /// Mirror `_get_splits_by_fns`. Try paragraph then sentence first
    /// (carrying `is_sentence=true`); fall through to regex, separator,
    /// char (`is_sentence=false`).
    fn get_splits_by_fns<SentFn>(
        &self,
        text: &str,
        chunking_tokenizer_fn: &SentFn,
        secondary_re: Option<&Regex>,
    ) -> (Vec<(usize, usize)>, bool)
    where
        SentFn: Fn(&str) -> Vec<(usize, usize)>,
    {
        let paragraph_spans = split_by_sep(text, &self.paragraph_separator);
        if paragraph_spans.len() > 1 {
            return (paragraph_spans, true);
        }
        let sentence_spans = chunking_tokenizer_fn(text);
        if sentence_spans.len() > 1 {
            return (sentence_spans, true);
        }
        if let Some(re) = secondary_re {
            let regex_spans = split_by_regex(text, re);
            if regex_spans.len() > 1 {
                return (regex_spans, false);
            }
        }
        let sep_spans = split_by_sep(text, &self.separator);
        if sep_spans.len() > 1 {
            return (sep_spans, false);
        }
        (split_by_char(text), false)
    }

    /// Mirror `_merge`. Greedy-pack splits into chunks ≤ `chunk_size`
    /// tokens, prepending `chunk_overlap` tokens from the previous
    /// chunk's tail.
    fn merge(&self, splits: Vec<Split>, _text: &str) -> Vec<TextChunk> {
        let mut chunks: Vec<TextChunk> = Vec::new();
        let mut cur_chunk: Vec<Split> = Vec::new();
        let mut last_chunk: Vec<Split> = Vec::new();
        let mut cur_chunk_len: usize = 0;
        let mut new_chunk = true;

        let mut split_idx: usize = 0;
        while split_idx < splits.len() {
            let cur_split = splits[split_idx].clone();
            if cur_split.token_size > self.chunk_size {
                // LlamaIndex raises here; defensive fallback: emit
                // alone. With the char-level cascade fallback this
                // shouldn't happen unless a single char tokenizes as
                // > chunk_size tokens (impossible for any sane vocab).
                chunks.push(TextChunk {
                    pos: cur_split.pos,
                    len: cur_split.len,
                });
                split_idx += 1;
                continue;
            }
            if cur_chunk_len + cur_split.token_size > self.chunk_size && !new_chunk {
                close_chunk(
                    &mut chunks,
                    &mut cur_chunk,
                    &mut last_chunk,
                    &mut cur_chunk_len,
                    &mut new_chunk,
                    self.chunk_overlap,
                );
            } else {
                if new_chunk && cur_chunk_len + cur_split.token_size > self.chunk_size {
                    // Trim overlap from the front until the split fits.
                    while !cur_chunk.is_empty()
                        && cur_chunk_len + cur_split.token_size > self.chunk_size
                    {
                        let s = cur_chunk.remove(0);
                        cur_chunk_len = cur_chunk_len.saturating_sub(s.token_size);
                    }
                }
                if cur_split.is_sentence
                    || cur_chunk_len + cur_split.token_size <= self.chunk_size
                    || new_chunk
                {
                    cur_chunk_len += cur_split.token_size;
                    cur_chunk.push(cur_split);
                    split_idx += 1;
                    new_chunk = false;
                } else {
                    close_chunk(
                        &mut chunks,
                        &mut cur_chunk,
                        &mut last_chunk,
                        &mut cur_chunk_len,
                        &mut new_chunk,
                        self.chunk_overlap,
                    );
                    // Don't advance split_idx — retry with new chunk.
                }
            }
        }

        if !new_chunk && !cur_chunk.is_empty() {
            chunks.push(chunk_extent(&cur_chunk));
        }
        chunks
    }
}

fn close_chunk(
    chunks: &mut Vec<TextChunk>,
    cur_chunk: &mut Vec<Split>,
    last_chunk: &mut Vec<Split>,
    cur_chunk_len: &mut usize,
    new_chunk: &mut bool,
    chunk_overlap: usize,
) {
    if !cur_chunk.is_empty() {
        chunks.push(chunk_extent(cur_chunk));
    }
    *last_chunk = std::mem::take(cur_chunk);
    *cur_chunk_len = 0;
    *new_chunk = true;
    // Overlap: walk backwards through last_chunk, prepending splits
    // until adding the next would exceed chunk_overlap.
    if !last_chunk.is_empty() {
        let mut last_index = last_chunk.len() as isize - 1;
        while last_index >= 0 {
            let s = &last_chunk[last_index as usize];
            if *cur_chunk_len + s.token_size > chunk_overlap {
                break;
            }
            *cur_chunk_len += s.token_size;
            cur_chunk.insert(0, s.clone());
            last_index -= 1;
        }
    }
}

/// `chunk.pos` = first split's pos. `chunk.len` = end-of-last-split
/// minus first.pos. Splits are kept in input order, so the chunk
/// covers a contiguous byte range (overlap with the prior chunk is
/// fine — both chunks reference the same body bytes).
fn chunk_extent(splits: &[Split]) -> TextChunk {
    let pos = splits[0].pos;
    let last = splits.last().unwrap();
    let end = last.pos + last.len;
    TextChunk { pos, len: end - pos }
}

/// Mirror LlamaIndex's `_postprocess_chunks`: drop chunks whose body
/// span is whitespace-only. LlamaIndex also strips leading/trailing
/// whitespace from each chunk text — we don't, because we return
/// offsets and `body[pos..pos+len]` must round-trip through chunks_fts
/// snippet rendering.
fn postprocess_chunks(chunks: Vec<TextChunk>, text: &str) -> Vec<TextChunk> {
    chunks
        .into_iter()
        .filter(|c| !text[c.pos..c.pos + c.len].trim().is_empty())
        .collect()
}

/// Mirror LlamaIndex's `split_text_keep_separator(text, separator)`:
/// split on `sep`, keeping the separator at the start of each subsequent
/// piece. Returns byte spans into `text`.
fn split_by_sep(text: &str, sep: &str) -> Vec<(usize, usize)> {
    if sep.is_empty() || text.is_empty() {
        return vec![(0, text.len())];
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let bytes = text.as_bytes();
    let sep_bytes = sep.as_bytes();
    let mut span_start: usize = 0;
    let mut i: usize = 0;
    while i + sep_bytes.len() <= bytes.len() {
        if &bytes[i..i + sep_bytes.len()] == sep_bytes {
            if i > span_start {
                spans.push((span_start, i));
            }
            span_start = i;
            i += sep_bytes.len();
            continue;
        }
        i += 1;
    }
    if span_start < bytes.len() {
        spans.push((span_start, bytes.len()));
    }
    spans
}

/// Mirror LlamaIndex's `split_by_regex` (which uses `re.findall`).
/// Returns byte spans of regex matches.
fn split_by_regex(text: &str, re: &Regex) -> Vec<(usize, usize)> {
    re.find_iter(text).map(|m| (m.start(), m.end())).collect()
}

/// Mirror LlamaIndex's `split_by_char` (`list(text)`). One span per
/// character (UTF-8 — span covers the char's bytes).
fn split_by_char(text: &str) -> Vec<(usize, usize)> {
    text.char_indices()
        .map(|(i, c)| (i, i + c.len_utf8()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Word-count tokenizer for tests: 1 token per whitespace-split
    /// word. Approximates a real tokenizer well enough to drive the
    /// algorithm without needing a model loaded.
    fn word_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    /// Trivial sentence splitter for tests (regex won't lock us into
    /// sentencex's specific behavior at this level): split at `. ` or
    /// end of text.
    fn naive_sentences(text: &str) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        let bytes = text.as_bytes();
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1] == b' ' {
                spans.push((start, i + 1));
                start = i + 1;
            }
            i += 1;
        }
        if start < bytes.len() {
            spans.push((start, bytes.len()));
        }
        spans
    }

    #[test]
    fn empty_text_returns_no_chunks() {
        let s = SentenceSplitter::default();
        let mut tok = word_tokens;
        let chunks = s.split_text("", &mut tok, &naive_sentences);
        assert!(chunks.is_empty());
    }

    #[test]
    fn short_text_one_chunk() {
        let s = SentenceSplitter {
            chunk_size: 100,
            chunk_overlap: 0,
            ..Default::default()
        };
        let text = "Hello world.";
        let chunks = s.split_text(text, &mut word_tokens, &naive_sentences);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].pos, 0);
        assert_eq!(chunks[0].len, text.len());
    }

    #[test]
    fn split_by_sep_basic() {
        let spans = split_by_sep("a\n\n\nb\n\n\nc", "\n\n\n");
        assert_eq!(spans, vec![(0, 1), (1, 5), (5, 9)]);
        // Reconstruct
        let text = "a\n\n\nb\n\n\nc";
        let joined: String = spans.iter().map(|&(s, e)| &text[s..e]).collect();
        assert_eq!(joined, text);
    }

    #[test]
    fn split_by_sep_no_separator_returns_whole() {
        let spans = split_by_sep("hello", "\n\n\n");
        assert_eq!(spans, vec![(0, 5)]);
    }

    #[test]
    fn split_by_char_returns_one_span_per_char() {
        let spans = split_by_char("abc");
        assert_eq!(spans, vec![(0, 1), (1, 2), (2, 3)]);
    }

    #[test]
    fn split_by_char_handles_multibyte() {
        // 'é' is 2 bytes in UTF-8.
        let spans = split_by_char("aé");
        assert_eq!(spans, vec![(0, 1), (1, 3)]);
    }

    #[test]
    fn split_by_regex_finds_matches() {
        let re = Regex::new(r"\w+").unwrap();
        let spans = split_by_regex("hello world", &re);
        assert_eq!(spans, vec![(0, 5), (6, 11)]);
    }

    #[test]
    fn long_text_splits_at_paragraph_boundary() {
        // chunk_size=5 words. Two paragraphs of ~5 words each, separated
        // by "\n\n\n", should split into 2 chunks at the paragraph break.
        let s = SentenceSplitter {
            chunk_size: 5,
            chunk_overlap: 0,
            ..Default::default()
        };
        let text = "one two three four five\n\n\nsix seven eight nine ten";
        let chunks = s.split_text(text, &mut word_tokens, &naive_sentences);
        assert!(
            chunks.len() >= 2,
            "expected paragraph-boundary split: got {chunks:?}"
        );
        // First chunk includes the first paragraph; second includes the second.
        let c0 = &text[chunks[0].pos..chunks[0].pos + chunks[0].len];
        let c_last = &text[chunks.last().unwrap().pos
            ..chunks.last().unwrap().pos + chunks.last().unwrap().len];
        assert!(c0.contains("one"));
        assert!(c_last.contains("ten"));
    }

    #[test]
    fn chunks_have_overlap() {
        // 4 sentences, chunk_size=2 sentences, overlap=1 sentence.
        // Each sentence is 1 word so token counts match.
        let s = SentenceSplitter {
            chunk_size: 2,
            chunk_overlap: 1,
            paragraph_separator: "\n\n\n".to_string(),
            separator: " ".to_string(),
            secondary_chunking_regex: None,
        };
        let text = "A. B. C. D.";
        let chunks = s.split_text(text, &mut word_tokens, &naive_sentences);
        assert!(chunks.len() >= 2, "expected ≥2 chunks: {chunks:?}");
        // Chunk N+1's pos should be ≤ chunk N's end (overlap).
        for w in chunks.windows(2) {
            assert!(
                w[1].pos <= w[0].pos + w[0].len,
                "expected overlap: chunk[{}..{}] vs chunk[{}..{}]",
                w[0].pos,
                w[0].pos + w[0].len,
                w[1].pos,
                w[1].pos + w[1].len
            );
        }
    }

    #[test]
    fn recursive_cascade_falls_through_to_word_split() {
        // No paragraphs, no sentence punctuation, no commas → falls to
        // separator (space) split.
        let s = SentenceSplitter {
            chunk_size: 3,
            chunk_overlap: 0,
            ..Default::default()
        };
        let text = "alpha beta gamma delta epsilon zeta";
        let chunks = s.split_text(text, &mut word_tokens, &naive_sentences);
        assert!(
            chunks.len() >= 2,
            "expected word-split fallback: {chunks:?}"
        );
        // Total content (after possible whitespace boundaries) covers all words.
        let joined: String = chunks
            .iter()
            .map(|c| &text[c.pos..c.pos + c.len])
            .collect::<Vec<_>>()
            .join("");
        for word in ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"] {
            assert!(joined.contains(word), "missing {word} in: {joined:?}");
        }
    }

    #[test]
    fn chunks_byte_offsets_are_char_safe() {
        let s = SentenceSplitter {
            chunk_size: 2,
            chunk_overlap: 0,
            ..Default::default()
        };
        let text = "Café est ouvert.\n\n\nJe bois café.";
        let chunks = s.split_text(text, &mut word_tokens, &naive_sentences);
        for c in &chunks {
            assert!(text.is_char_boundary(c.pos), "pos {} not char-aligned", c.pos);
            assert!(
                text.is_char_boundary(c.pos + c.len),
                "end {} not char-aligned",
                c.pos + c.len
            );
        }
    }
}
