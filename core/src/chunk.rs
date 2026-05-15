//! Section-aware Markdown chunking. Used by document ingestion to split
//! long inputs into chunks that each fit through one Extract call.

/// Approximate token count for English Markdown. ~3 chars per token is
/// conservative; real tokenizers vary 2.5-4 chars/token. We err high so
/// chunks stay below model context limits.
pub fn estimate_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(3)
}

/// Chunk Markdown content. Returns one element if `content` is short
/// enough; otherwise splits at H1/H2 boundaries (preferred), paragraph
/// boundaries (fallback), or hard line splits (last resort). Never splits
/// inside a fenced code block.
///
/// `target_tokens` — desired chunk size; chunks may be slightly above to
/// reach a clean section boundary.
/// `hard_cap_tokens` — absolute maximum; force-split even mid-section.
/// `max_chunks` — caller errors if chunking would exceed this.
pub fn chunk_markdown(
    content: &str,
    target_tokens: usize,
    hard_cap_tokens: usize,
    max_chunks: usize,
) -> Result<Vec<String>, ChunkError> {
    if estimate_tokens(content) <= target_tokens {
        return Ok(vec![content.to_string()]);
    }
    let segments = split_into_segments(content);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for seg in segments {
        let combined_tokens = estimate_tokens(&current) + estimate_tokens(&seg);
        if combined_tokens > target_tokens && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if estimate_tokens(&seg) > hard_cap_tokens {
            // Single segment too big — recursively split at paragraph then line.
            for sub in split_oversize(&seg, hard_cap_tokens) {
                if estimate_tokens(&current) + estimate_tokens(&sub) > target_tokens
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

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error(
        "document would split into more than {0} chunks; raise max_chunks or shrink the source"
    )]
    TooManyChunks(usize),
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
        let chunks = chunk_markdown(c, 1000, 2000, 10).unwrap();
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
        // target = 1500 tokens (~4500 chars). Each section is 3000+ chars -> 1000+ tokens.
        let chunks = chunk_markdown(&c, 1500, 3000, 10).unwrap();
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
        let chunks = chunk_markdown(&c, 1500, 3000, 10).unwrap();
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
        let chunks = chunk_markdown(&c, 500, 5000, 20).unwrap();
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
        let err = chunk_markdown(&c, 500, 1000, 5).unwrap_err();
        assert!(matches!(err, ChunkError::TooManyChunks(5)));
    }

    #[test]
    fn estimate_tokens_is_chars_over_three() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcdef"), 2);
        assert_eq!(estimate_tokens("abcdefg"), 3);
    }
}
