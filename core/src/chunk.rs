//! Section-aware Markdown chunking. Used by document ingestion to split
//! long inputs into chunks that each fit through one Extract call.

/// Approximate token count for English Markdown. ~3 chars per token is
/// conservative; real tokenizers vary 2.5-4 chars/token. We err high so
/// chunks stay below model context limits.
pub fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(3)
}

/// A single addressable unit of source content for the EXTRACT worker.
///
/// Transcripts produce one ExtractSegment per turn (role, timestamp, index
/// populated). Documents produce one ExtractSegment per markdown chunk
/// (all three optional fields = None — preserves Mode B detection in the
/// EXTRACT system prompt).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExtractSegment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub text: String,
}

/// Chunk Markdown content. Returns one element if `content` is short
/// enough; otherwise splits at H1/H2 boundaries (preferred), paragraph
/// boundaries (fallback), or hard line splits (last resort). Never splits
/// inside a fenced code block.
///
/// `chunk_max_tokens` — packing cap; chunks pack up to this size, and any
/// single segment larger than this is force-split at the same threshold.
/// `max_chunks` — caller errors if chunking would exceed this.
pub fn chunk_markdown(
    content: &str,
    chunk_max_tokens: usize,
    max_chunks: usize,
) -> Result<Vec<String>, ChunkError> {
    if estimate_tokens(content) <= chunk_max_tokens {
        return Ok(vec![content.to_string()]);
    }
    let segments = split_into_segments(content);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for seg in segments {
        let combined_tokens = estimate_tokens(&current) + estimate_tokens(&seg);
        if combined_tokens > chunk_max_tokens && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if estimate_tokens(&seg) > chunk_max_tokens {
            // Single segment too big — recursively split at paragraph then line.
            for sub in split_oversize(&seg, chunk_max_tokens) {
                if estimate_tokens(&current) + estimate_tokens(&sub) > chunk_max_tokens
                    && !current.is_empty()
                {
                    chunks.push(std::mem::take(&mut current));
                }
                push_with_newline(&mut current, &sub);
            }
        } else {
            push_with_newline(&mut current, &seg);
        }
        if chunks.len() >= max_chunks {
            return Err(ChunkError::TooManyChunks(max_chunks));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.len() > max_chunks {
        return Err(ChunkError::TooManyChunks(max_chunks));
    }
    Ok(chunks)
}

/// Pack a sequence of transcript segments into chunks under the size cap.
///
/// Each chunk is a `Vec<ExtractSegment>`. Segments are never split mid-turn;
/// a single segment exceeding `chunk_max_tokens` errors with
/// `ChunkError::TooLargeSegment`. After packing, the last `overlap_turns`
/// segments of each chunk `i-1` are prepended to chunk `i` (anchor context).
///
/// Token estimation uses the same `estimate_tokens` as `chunk_markdown`.
pub fn chunk_transcript_segments(
    segments: &[ExtractSegment],
    chunk_max_tokens: usize,
    max_chunks: usize,
    overlap_turns: usize,
) -> Result<Vec<Vec<ExtractSegment>>, ChunkError> {
    if segments.is_empty() {
        return Ok(Vec::new());
    }

    // Estimate the average per-turn tokens from the input so we can
    // reserve budget for the `overlap_turns` we'll prepend to chunks[1..].
    // Best-effort: based on the average, not pessimistic. If the average
    // misjudges and a post-overlap chunk exceeds cap, the worker's
    // fit_miss restart trigger is the safety net.
    let avg_seg_tokens = {
        let total: usize = segments.iter().map(|s| estimate_tokens(&s.text)).sum();
        (total / segments.len()).max(1)
    };
    let overlap_budget = overlap_turns.saturating_mul(avg_seg_tokens);

    let mut chunks: Vec<Vec<ExtractSegment>> = Vec::new();
    let mut current: Vec<ExtractSegment> = Vec::new();
    let mut current_tokens: usize = 0;

    for (idx, seg) in segments.iter().enumerate() {
        let seg_tokens = estimate_tokens(&seg.text);
        if seg_tokens > chunk_max_tokens {
            return Err(ChunkError::TooLargeSegment(idx, seg_tokens));
        }
        // After chunk 0, reserve overlap_budget tokens for the prepended
        // turns from the prior chunk. So the effective per-chunk cap is
        // (chunk_max_tokens - overlap_budget); chunk 0 uses the full cap
        // since there's no overlap to reserve.
        let effective_cap = if chunks.is_empty() {
            chunk_max_tokens
        } else {
            chunk_max_tokens.saturating_sub(overlap_budget)
        };
        if current_tokens + seg_tokens > effective_cap && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_tokens = 0;
            if chunks.len() >= max_chunks {
                return Err(ChunkError::TooManyChunks(max_chunks));
            }
        }
        current.push(seg.clone());
        current_tokens += seg_tokens;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.len() > max_chunks {
        return Err(ChunkError::TooManyChunks(max_chunks));
    }

    // Overlap: chunks[1..] start with the last `overlap_turns` segments of
    // chunks[i-1]. Take min(overlap_turns, prev.len()) to handle the case
    // where overlap_budget saturated effective_cap and chunks[i-1] ended up
    // with fewer segments than overlap_turns (e.g., overlap_budget >=
    // chunk_max_tokens forces 1-seg chunks). Best-effort: prepend what's
    // available.
    if overlap_turns > 0 && chunks.len() > 1 {
        for i in 1..chunks.len() {
            let prev_len = chunks[i - 1].len();
            let take = overlap_turns.min(prev_len);
            let overlap: Vec<ExtractSegment> = chunks[i - 1][prev_len - take..].to_vec();
            let mut new_chunk = Vec::with_capacity(overlap.len() + chunks[i].len());
            new_chunk.extend(overlap);
            new_chunk.extend(std::mem::take(&mut chunks[i]));
            chunks[i] = new_chunk;
        }
    }

    Ok(chunks)
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error(
        "document would split into more than {0} chunks; raise max_chunks or shrink the source"
    )]
    TooManyChunks(usize),
    #[error(
        "segment {0} is {1} tokens, exceeds chunk_max_tokens (transcripts can't be split mid-segment)"
    )]
    TooLargeSegment(usize, usize),
}

fn push_with_newline(buf: &mut String, s: &str) {
    if !buf.is_empty() && !buf.ends_with("\n\n") {
        if buf.ends_with('\n') {
            buf.push('\n');
        } else {
            buf.push_str("\n\n");
        }
    }
    buf.push_str(s);
}

/// Split content into top-level segments at H1/H2 headings. If no
/// headings, fall back to paragraph splits (\n\n+). If no paragraph
/// breaks, fall back to single-line splits.
fn split_into_segments(content: &str) -> Vec<String> {
    let in_code_fence = build_code_fence_mask(content);
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut byte_pos = 0;
    for line in content.split_inclusive('\n') {
        let line_start = byte_pos;
        byte_pos += line.len();
        let in_fence = in_code_fence.get(line_start).copied().unwrap_or(false);
        let trimmed = line.trim_start();
        let is_heading = !in_fence && (trimmed.starts_with("# ") || trimmed.starts_with("## "));
        if is_heading && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    if segments.len() == 1 {
        // No H1/H2 splits found; fall back to paragraph splits.
        return split_paragraphs(&segments[0]);
    }
    segments
}

fn split_paragraphs(content: &str) -> Vec<String> {
    let parts: Vec<String> = content
        .split("\n\n")
        .map(|p| {
            if p.is_empty() {
                String::new()
            } else {
                format!("{p}\n\n")
            }
        })
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() == 1 {
        // No paragraph breaks; fall back to line splits.
        return content.lines().map(|l| format!("{l}\n")).collect();
    }
    parts
}

/// Recursively split a too-large segment at paragraph then line boundaries
/// until each piece is under `cap_tokens`.
fn split_oversize(seg: &str, cap_tokens: usize) -> Vec<String> {
    let paras = split_paragraphs(seg);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for p in paras {
        if estimate_tokens(&p) > cap_tokens {
            // Even paragraph too big — split lines.
            for line in p.lines() {
                let line_with_nl = format!("{line}\n");
                if estimate_tokens(&current) + estimate_tokens(&line_with_nl) > cap_tokens
                    && !current.is_empty()
                {
                    out.push(std::mem::take(&mut current));
                }
                current.push_str(&line_with_nl);
            }
        } else if estimate_tokens(&current) + estimate_tokens(&p) > cap_tokens
            && !current.is_empty()
        {
            out.push(std::mem::take(&mut current));
            current.push_str(&p);
        } else {
            current.push_str(&p);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Build a byte-indexed mask: for each byte position, true if that byte is
/// inside a fenced code block (between ``` and the matching closing ```).
/// Used so heading-detection skips ATX-headings that appear inside code.
fn build_code_fence_mask(content: &str) -> Vec<bool> {
    let mut mask = vec![false; content.len()];
    let mut in_fence = false;
    let mut byte_pos = 0;
    for line in content.split_inclusive('\n') {
        let line_start = byte_pos;
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
        }
        let line_end = byte_pos + line.len();
        for i in line_start..line_end.min(mask.len()) {
            mask[i] = in_fence;
        }
        byte_pos += line.len();
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_one_chunk() {
        let c = "# Title\n\nSome paragraph.\n";
        let chunks = chunk_markdown(c, 2000, 10).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], c);
    }

    #[test]
    fn splits_at_h1_boundaries() {
        let c = format!(
            "# Section A\n\n{}\n\n# Section B\n\n{}\n\n# Section C\n\n{}\n",
            "x".repeat(3000),
            "y".repeat(3000),
            "z".repeat(3000),
        );
        // cap = 3000 tokens (~9000 chars). Each section is 3000+ chars -> 1000+ tokens.
        let chunks = chunk_markdown(&c, 3000, 10).unwrap();
        assert!(
            chunks.len() >= 2,
            "expected ≥2 chunks, got {}",
            chunks.len()
        );
        for ch in &chunks {
            assert!(
                ch.contains("# Section "),
                "chunk should contain a heading: {ch}"
            );
        }
    }

    #[test]
    fn splits_at_paragraphs_when_no_headings() {
        let c = format!(
            "{}\n\n{}\n\n{}\n",
            "x".repeat(3000),
            "y".repeat(3000),
            "z".repeat(3000)
        );
        let chunks = chunk_markdown(&c, 3000, 10).unwrap();
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn never_splits_inside_code_fence() {
        let mut c = String::from("# Intro\n\nIntro text.\n\n# Code\n\n```rust\n");
        for _ in 0..200 {
            c.push_str("// not a heading even though it has # marks\n");
            c.push_str("# fake heading inside code\n");
        }
        c.push_str("```\n\n# Outro\n\nClosing text.\n");
        let chunks = chunk_markdown(&c, 5000, 20).unwrap();
        // The chunk containing the code fence must contain BOTH ``` markers.
        let fenced_chunk = chunks
            .iter()
            .find(|ch| ch.contains("```rust"))
            .expect("a chunk should contain the opening fence");
        assert!(
            fenced_chunk.matches("```").count() >= 2,
            "code fence must be balanced inside one chunk; got {} backtick lines in:\n{fenced_chunk}",
            fenced_chunk.matches("```").count()
        );
    }

    #[test]
    fn errors_on_too_many_chunks() {
        let c: String = (0..50)
            .map(|i| format!("# H{i}\n\n{}\n\n", "x".repeat(2000)))
            .collect();
        let err = chunk_markdown(&c, 1000, 5).unwrap_err();
        assert!(matches!(err, ChunkError::TooManyChunks(5)));
    }

    #[test]
    fn estimate_tokens_is_chars_over_three() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcdef"), 2);
        assert_eq!(estimate_tokens("abcdefg"), 3);
    }

    #[test]
    fn chunk_markdown_collapsed_signature_takes_single_cap() {
        // Documents and transcripts share one knob now: chunk_max_tokens.
        // Pack target = cap; force-split also at cap.
        let body = "## A\n".repeat(500); // ~3000 chars / ~750 tokens at 4 bytes/token
        let chunks = chunk_markdown(&body, 1_000, 10).unwrap();
        assert!(!chunks.is_empty(), "should produce at least one chunk");
        for (i, c) in chunks.iter().enumerate() {
            assert!(estimate_tokens(c) <= 1_000, "chunk {i} = {} tokens > cap", estimate_tokens(c));
        }
    }

    #[test]
    fn chunk_error_too_large_segment_message_includes_idx_and_tokens() {
        let e = ChunkError::TooLargeSegment(7, 192_345);
        let s = format!("{e}");
        assert!(s.contains("7"), "error message should include segment index: {s}");
        assert!(s.contains("192345") || s.contains("192,345"), "error message should include token count: {s}");
    }

    #[test]
    fn chunk_transcript_segments_packs_under_chunk_max() {
        // 10 segments, each ~200 tokens (800 chars). Cap at 600 tokens →
        // each chunk holds ~3 segments (~600 tokens, just under cap).
        let segs: Vec<ExtractSegment> = (0..10)
            .map(|i| ExtractSegment {
                index: Some(i),
                role: Some("user".into()),
                timestamp: None,
                text: "x".repeat(800),
            })
            .collect();
        let chunks = chunk_transcript_segments(&segs, 600, 100, 0).unwrap();
        assert!(chunks.len() >= 3, "expected >=3 chunks at 600-token cap, got {}", chunks.len());
        for (i, c) in chunks.iter().enumerate() {
            let total = c.iter().map(|s| estimate_tokens(&s.text)).sum::<usize>();
            assert!(total <= 600, "chunk {i} total tokens {total} > cap 600");
        }
    }

    #[test]
    fn chunk_transcript_segments_never_splits_a_segment() {
        // 2000 chars = ~667 tokens (chars/3). Cap=1000 leaves room for the
        // whole segment without triggering TooLargeSegment, so we can verify
        // it lands in a single chunk untouched.
        let segs = vec![ExtractSegment {
            index: Some(0),
            role: Some("user".into()),
            timestamp: None,
            text: "x".repeat(2000),
        }];
        let chunks = chunk_transcript_segments(&segs, 1000, 100, 0).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1, "segment must not be split");
        assert_eq!(chunks[0][0].text.len(), 2000);
    }

    #[test]
    fn chunk_transcript_segments_too_large_segment_errors() {
        // Pick a size that's definitely larger than the cap regardless of
        // whether estimate_tokens is chars/3 or chars/4 or bytes/4.
        let segs = vec![ExtractSegment {
            index: Some(0),
            role: Some("user".into()),
            timestamp: None,
            text: "x".repeat(5000),
        }];
        let err = chunk_transcript_segments(&segs, 600, 100, 0).unwrap_err();
        match err {
            ChunkError::TooLargeSegment(idx, _tokens) => {
                assert_eq!(idx, 0);
            }
            other => panic!("expected TooLargeSegment, got {other:?}"),
        }
    }

    #[test]
    fn chunk_transcript_segments_empty_input() {
        let chunks = chunk_transcript_segments(&[], 600, 100, 0).unwrap();
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunk_transcript_segments_too_many_chunks() {
        // Each segment ~200 tokens at cap=180 → each is alone in its chunk.
        let segs: Vec<ExtractSegment> = (0..20)
            .map(|i| ExtractSegment {
                index: Some(i),
                role: Some("user".into()),
                timestamp: None,
                text: "x".repeat(800),
            })
            .collect();
        let err = chunk_transcript_segments(&segs, 800, 5, 0).unwrap_err();
        assert!(matches!(err, ChunkError::TooManyChunks(5)));
    }

    #[test]
    fn chunk_transcript_segments_overlap_prepends_last_k_turns() {
        let segs: Vec<ExtractSegment> = (0..10)
            .map(|i| ExtractSegment {
                index: Some(i),
                role: Some("user".into()),
                timestamp: None,
                text: format!("seg-{i}"),
            })
            .collect();
        // Force two chunks. Each segment is ~5 chars / ~2 tokens. Cap = 8 tokens
        // → ~4 segments per chunk.
        let chunks = chunk_transcript_segments(&segs, 8, 100, 3).unwrap();
        assert!(chunks.len() >= 2, "expected >=2 chunks, got {}", chunks.len());
        // Chunk 1 should start with the last 3 segments of chunk 0.
        let prev_tail: Vec<usize> = chunks[0]
            .iter()
            .rev()
            .take(3)
            .map(|s| s.index.unwrap())
            .collect();
        let next_head: Vec<usize> = chunks[1].iter().take(3).map(|s| s.index.unwrap()).collect();
        let mut prev_tail_ordered = prev_tail.clone();
        prev_tail_ordered.reverse();
        assert_eq!(
            next_head, prev_tail_ordered,
            "chunk[1] should start with last 3 of chunk[0]: head={next_head:?}, prev_tail_ordered={prev_tail_ordered:?}"
        );
    }

    #[test]
    fn chunk_transcript_segments_overlap_chunk_0_unchanged() {
        // Geometry must produce >=2 chunks AND satisfy the post-overlap
        // invariant (overlap_turns * avg_tokens <= cap). Cap=8, 10 segs of
        // ~2 tokens each: chunks[0] packs 4 segs at full cap; chunks[1+]
        // pack 1 seg at effective_cap=2; after overlap each is 4 segs / 8 tokens.
        let segs: Vec<ExtractSegment> = (0..10)
            .map(|i| ExtractSegment {
                index: Some(i),
                role: Some("user".into()),
                timestamp: None,
                text: format!("seg-{i}"),
            })
            .collect();
        let chunks = chunk_transcript_segments(&segs, 8, 100, 3).unwrap();
        assert!(chunks.len() >= 2, "expected >=2 chunks, got {}", chunks.len());
        // Chunk 0 must start with segment 0 (no prepended overlap).
        assert_eq!(chunks[0][0].index, Some(0), "chunk 0 should start with segment 0");
    }

    #[test]
    fn chunk_transcript_segments_chunks_1plus_smaller_than_chunk_0() {
        // Verifies the overlap-budget reservation: chunks[0] uses the
        // full cap, but chunks 1..N use a smaller effective_cap to leave
        // room for prepended overlap. With homogeneous segs of S tokens,
        // chunks[0] packs floor(cap/S) segs and chunks[1+] pack fewer.
        // 20 segs of 40 chars = 14 tokens each. Cap=60, overlap=3.
        // avg=14, overlap_budget=42, effective_cap=18. Chunks[0] holds
        // 4 segs (56 tokens); chunks 1..N hold 1 seg each (14 tokens).
        let segs: Vec<ExtractSegment> = (0..20)
            .map(|i| ExtractSegment {
                index: Some(i),
                role: Some("user".into()),
                timestamp: None,
                text: "x".repeat(40),
            })
            .collect();
        let chunks = chunk_transcript_segments(&segs, 60, 100, 3).unwrap();
        assert!(chunks.len() >= 2, "expected >=2 chunks, got {}", chunks.len());
        // Compare PRE-overlap sizes. After overlap is applied, all chunks
        // include the prepended turns — so to verify the reservation we
        // count chunks[1]'s original (non-overlap) tail: total - overlap.
        // chunks[0] (unchanged by overlap) gets 4 segs of original input.
        // chunks[1] gets 3 overlap + originals; originals should be < 4.
        let chunk0_originals = chunks[0].len();
        let chunk1_originals = chunks[1].len() - 3; // subtract overlap_turns
        assert!(
            chunk1_originals < chunk0_originals,
            "chunks[1] originals ({chunk1_originals}) should be < chunks[0] ({chunk0_originals}) due to reservation"
        );
    }

    #[test]
    fn chunk_transcript_segments_post_overlap_stays_under_cap() {
        // Pack tight, then overlap should NOT push chunks 1..N over cap.
        // 15 segments of 40 chars = 14 tokens each (chars/3 ceil_div).
        // Cap = 60 tokens, overlap_turns = 3. Without budget reservation,
        // chunks pack to ~56 tokens, then overlap adds 3 × 14 = 42 tokens
        // → chunk 1 = ~98 tokens (over cap). With reservation:
        // overlap_budget = 3 × 14 = 42, effective_cap for chunks 1..N
        // = 60 - 42 = 18, so they hold 1 original segment each (14 tokens),
        // and post-overlap totals = 3 × 14 + 14 = 56 ≤ 60.
        let segs: Vec<ExtractSegment> = (0..15)
            .map(|i| ExtractSegment {
                index: Some(i),
                role: Some("user".into()),
                timestamp: None,
                text: "x".repeat(40),
            })
            .collect();
        let chunks = chunk_transcript_segments(&segs, 60, 100, 3).unwrap();
        for (i, c) in chunks.iter().enumerate() {
            let total: usize = c.iter().map(|s| estimate_tokens(&s.text)).sum();
            assert!(
                total <= 60,
                "post-overlap chunk {i} total = {total} tokens > cap 60 (chunks.len()={})",
                chunks.len()
            );
        }
    }

    #[test]
    fn chunk_transcript_segments_overlap_safe_when_budget_saturates() {
        // Regression: when overlap_budget >= chunk_max_tokens, effective_cap
        // saturates to 0 → chunks 1..N end up with fewer than overlap_turns
        // segments. The overlap loop must use .min(prev.len()) to avoid
        // panic on usize underflow.
        //
        // Geometry: cap=100, overlap_turns=3, segs of 30 tokens each.
        // avg=30, overlap_budget=90, effective_cap=10. Packing produces
        // chunks[0]=3 segs (30+30+30=90 ≤ 100), then chunks[1..N]=1 seg each
        // (since 30 > effective_cap=10, packer pushes 1-seg chunks).
        // At i=2, chunks[1].len()=1 < overlap_turns=3 — the .min() guard fires.
        let segs: Vec<ExtractSegment> = (0..6)
            .map(|i| ExtractSegment {
                index: Some(i),
                role: Some("user".into()),
                timestamp: None,
                text: "x".repeat(90), // ~30 tokens at chars/3
            })
            .collect();
        // Should not panic.
        let chunks = chunk_transcript_segments(&segs, 100, 100, 3).unwrap();
        assert!(chunks.len() >= 2, "expected multiple chunks");
        // No further behavioral assertion — the test is "doesn't panic".
    }

    /// Load-bearing contract test: the EXTRACT system prompt's Mode A
    /// (TRANSCRIPT) vs Mode B (DOCUMENT) detection keys on whether segments
    /// carry `role`. Documents wrap markdown chunks in ExtractSegment with
    /// role=None and rely on serde's `skip_serializing_if = "Option::is_none"`
    /// to omit the `role` key entirely. A refactor that breaks this shape
    /// silently switches documents into Mode A and produces one-page-per-
    /// speaker output for content that has no speakers — this test pins
    /// the JSON shape so that regression is loud.
    #[test]
    fn extract_segment_doc_mode_omits_optional_fields() {
        let seg = ExtractSegment {
            index: None,
            role: None,
            timestamp: None,
            text: "x".into(),
        };
        let json = serde_json::to_string(&seg).unwrap();
        assert_eq!(
            json, r#"{"text":"x"}"#,
            "doc-mode segment must serialize without role/timestamp/index keys"
        );
    }

    #[test]
    fn extract_segment_transcript_mode_includes_role() {
        let seg = ExtractSegment {
            index: Some(1),
            role: Some("user".into()),
            timestamp: Some("2026-05-22T12:00:00Z".into()),
            text: "hi".into(),
        };
        let json = serde_json::to_string(&seg).unwrap();
        assert!(json.contains(r#""role":"user""#), "transcript mode should include role: {json}");
        assert!(json.contains(r#""index":1"#), "transcript mode should include index: {json}");
        assert!(
            json.contains(r#""timestamp":"2026-05-22T12:00:00Z""#),
            "transcript mode should include timestamp: {json}"
        );
    }
}
