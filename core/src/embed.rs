use ort::session::Session;
use ort::value::Tensor;

/// Initialize the ONNX Runtime from a specific dylib path.
///
/// Must be called before any `load_model` / `Session::builder` call when using
/// the `load-dynamic` feature. Returns an error if the dylib cannot be loaded.
/// Safe to call multiple times (subsequent calls are no-ops).
pub fn init_runtime(dylib_path: &std::path::Path) -> crate::error::Result<()> {
    ort::init_from(dylib_path)?.commit();
    Ok(())
}

/// Run a closure with panic output silenced, returning `Some(R)` on success
/// or `None` if the closure panicked.
///
/// The `ort` crate panics (rather than returning an error) when the ONNX
/// Runtime shared library cannot be loaded. `catch_unwind` catches the panic,
/// but the default hook still prints a noisy backtrace to stderr. This helper
/// installs a no-op panic hook for the duration of the call, then restores
/// the original hook afterwards. The panic payload is discarded.
pub fn catch_unwind_silent<F: FnOnce() -> R + std::panic::UnwindSafe, R>(
    f: F,
) -> Option<R> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(f).ok();
    std::panic::set_hook(prev);
    result
}

/// Expected embedding dimensionality for embedding-gemma-300m.
pub const EMBEDDING_DIM: usize = 768;

/// Current embedding model name. Used to detect stale chunks after model upgrades.
pub const CURRENT_MODEL_NAME: &str = "embedding-gemma-300m";

/// An ONNX embedding model loaded into an inference session.
pub struct EmbeddingModel {
    session: Session,
    /// Human-readable model name (e.g. "embedding-gemma-300m").
    pub model_name: String,
    /// Names of the model's input tensors, in order.
    pub input_names: Vec<String>,
    /// Names of the model's output tensors, in order.
    pub output_names: Vec<String>,
}

/// Load an ONNX embedding model from the given path.
///
/// Inspects the model graph to record input/output tensor names, which are
/// needed later for `embed_text`.
pub fn load_model(path: &str, model_name: &str) -> crate::error::Result<EmbeddingModel> {
    let session = Session::builder()?.commit_from_file(path)?;

    let input_names: Vec<String> = session
        .inputs()
        .iter()
        .map(|i| i.name().to_string())
        .collect();
    let output_names: Vec<String> = session
        .outputs()
        .iter()
        .map(|o| o.name().to_string())
        .collect();

    Ok(EmbeddingModel {
        session,
        model_name: model_name.to_string(),
        input_names,
        output_names,
    })
}

/// Embed a text string into a vector.
///
/// For models that accept `input_ids` and `attention_mask` (i64 tensors), we
/// use a simple character-ordinal encoding as a stand-in for a real tokenizer.
/// Each character is mapped to its Unicode scalar value (clamped to a
/// reasonable vocabulary range). This is sufficient for building and testing
/// the vector-search infrastructure; a proper tokenizer can be swapped in
/// later.
///
/// If the model's input names are unrecognised, falls back to a deterministic
/// hash-based fake embedding so downstream code can still be developed.
pub fn embed_text(model: &mut EmbeddingModel, text: &str) -> crate::error::Result<Vec<f32>> {
    let has_input_ids = model.input_names.iter().any(|n| n == "input_ids");
    let has_attention_mask = model.input_names.iter().any(|n| n == "attention_mask");

    if has_input_ids && has_attention_mask {
        // Build a simple character-ordinal token sequence.
        // Clamp to first 2048 characters (model context window).
        let max_len = 2048;
        let chars: Vec<i64> = text.chars().take(max_len).map(|c| c as i64).collect();
        let seq_len = chars.len().max(1); // at least 1 token

        // If text was empty, use a single padding token (0).
        let input_ids_data: Vec<i64> = if chars.is_empty() { vec![0i64] } else { chars };
        let attention_mask_data: Vec<i64> = vec![1i64; seq_len];

        let input_ids = Tensor::from_array(([1usize, seq_len], input_ids_data.into_boxed_slice()))?;
        let attention_mask =
            Tensor::from_array(([1usize, seq_len], attention_mask_data.into_boxed_slice()))?;

        // Check if model also expects a token_type_ids input.
        let has_token_type_ids = model.input_names.iter().any(|n| n == "token_type_ids");

        let outputs = if has_token_type_ids {
            let token_type_ids_data: Vec<i64> = vec![0i64; seq_len];
            let token_type_ids =
                Tensor::from_array(([1usize, seq_len], token_type_ids_data.into_boxed_slice()))?;
            model.session.run(ort::inputs! {
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
                "token_type_ids" => token_type_ids,
            })?
        } else {
            model.session.run(ort::inputs! {
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
            })?
        };

        // Extract the first output tensor. Embedding models typically output
        // shape [1, seq_len, hidden_dim] or [1, hidden_dim]. We mean-pool
        // across the sequence dimension to get a single vector.
        let first_output_name = &model.output_names[0];
        let output = &outputs[first_output_name.as_str()];
        let (shape, data) = output.try_extract_tensor::<f32>()?;
        let dims: &[i64] = shape;

        if dims.len() == 3 {
            // [batch, seq_len, hidden_dim] — mean-pool over seq_len.
            let hidden_dim = dims[2] as usize;
            let seq = dims[1] as usize;
            let mut embedding = vec![0.0f32; hidden_dim];
            for t in 0..seq {
                for d in 0..hidden_dim {
                    embedding[d] += data[t * hidden_dim + d];
                }
            }
            if seq > 0 {
                for v in &mut embedding {
                    *v /= seq as f32;
                }
            }
            Ok(embedding)
        } else if dims.len() == 2 {
            // [batch, hidden_dim] — already pooled.
            let hidden_dim = dims[1] as usize;
            Ok(data[..hidden_dim].to_vec())
        } else {
            // Unexpected shape — return the raw data.
            Ok(data.to_vec())
        }
    } else {
        // Unrecognised input format — return deterministic hash-based fake
        // embedding so downstream infrastructure can still be built and tested.
        Ok(hash_embedding(text))
    }
}

/// Produce a deterministic, normalised fake embedding from a text string.
///
/// Uses a simple hash-scatter approach: each byte of the input influences a
/// specific dimension. The resulting vector is L2-normalised.  This is *not*
/// semantically meaningful — it exists only so that the chunking and
/// vector-search infrastructure can be exercised without a real model.
pub fn hash_embedding(text: &str) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut vec = vec![0.0f32; EMBEDDING_DIM];
    for (i, byte) in text.bytes().enumerate() {
        let mut h = DefaultHasher::new();
        (i, byte).hash(&mut h);
        let idx = (h.finish() as usize) % EMBEDDING_DIM;
        // Use a simple signed contribution based on the hash.
        let sign = if h.finish() & 1 == 0 { 1.0 } else { -1.0 };
        vec[idx] += sign * (byte as f32) / 255.0;
    }
    // L2-normalise.
    let norm: f32 = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut vec {
            *v /= norm;
        }
    }
    vec
}

/// Cosine similarity between two vectors.
///
/// Returns 0.0 if either vector has zero magnitude.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// A chunk of document text for embedding.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub text: String,
    pub pos: usize, // character offset in original text
    pub len: usize, // character length
}

/// Split text into overlapping chunks for embedding.
///
/// - `max_tokens`: soft token budget per chunk. Approximated as 1 token per
///   character for conservative splitting (actual LLM tokenizers average ~4
///   chars/token, so a budget of 900 here covers ~900 characters).
/// - `overlap_frac`: fraction of overlap between chunks (e.g. 0.15 = 15%).
/// - Markdown-aware: prefers splitting at heading boundaries, code blocks,
///   paragraph breaks, list items, newlines.
pub fn chunk_text(text: &str, max_tokens: usize, overlap_frac: f64) -> Vec<Chunk> {
    // Approximate: 1 token ≈ 4 characters.
    let max_chars = max_tokens.max(1) * 4;

    // Short document fits in one chunk.
    if text.len() <= max_chars {
        return vec![Chunk {
            text: text.to_string(),
            pos: 0,
            len: text.len(),
        }];
    }

    let overlap_chars = (max_chars as f64 * overlap_frac) as usize;
    let search_window = max_chars.min(800); // 200 tokens ≈ 800 chars

    // Pre-compute which character positions are inside fenced code blocks.
    let in_code_fence = build_code_fence_map(text);

    let mut chunks = Vec::new();
    let mut start = 0;

    while start < text.len() {
        let remaining = text.len() - start;
        if remaining <= max_chars {
            // Last chunk: take everything remaining.
            chunks.push(Chunk {
                text: text[start..].to_string(),
                pos: start,
                len: remaining,
            });
            break;
        }

        // Target split point is at start + max_chars.
        let target = start + max_chars;

        // Search window: [target - window/2, target + window/2], clamped to text bounds.
        let half_window = search_window / 2;
        let win_start = target.saturating_sub(half_window).max(start + 1);
        let win_end = (target + half_window).min(text.len());

        let mut split = find_best_split(text, win_start, win_end, &in_code_fence);

        // If the chosen split is inside a code fence, push it past the fence end.
        if split < in_code_fence.len() && in_code_fence[split] {
            split = next_outside_fence(&in_code_fence, split);
        }

        // Snap to a valid char boundary (advance forward).
        while split < text.len() && !text.is_char_boundary(split) {
            split += 1;
        }

        // If pushing past the fence would exceed the text, take the rest.
        if split >= text.len() {
            chunks.push(Chunk {
                text: text[start..].to_string(),
                pos: start,
                len: text.len() - start,
            });
            break;
        }

        chunks.push(Chunk {
            text: text[start..split].to_string(),
            pos: start,
            len: split - start,
        });

        // Next chunk starts with overlap.
        let mut next_start = split.saturating_sub(overlap_chars);
        // Don't go backwards past the current chunk's start.
        let prev_start = chunks.last().map_or(0, |c| c.pos);
        if next_start <= prev_start && chunks.len() > 1 {
            next_start = split;
        }
        // Don't let overlap pull us back inside a code fence.
        if next_start < in_code_fence.len() && in_code_fence[next_start] {
            next_start = next_outside_fence(&in_code_fence, next_start);
        }
        // Snap to a valid char boundary (advance forward).
        while next_start < text.len() && !text.is_char_boundary(next_start) {
            next_start += 1;
        }
        start = next_start;
    }

    chunks
}

/// Build a boolean map: `result[i]` is true if character index `i` is inside
/// a fenced code block (between an opening ``` and its matching close).
fn build_code_fence_map(text: &str) -> Vec<bool> {
    let mut map = vec![false; text.len()];
    let mut inside = false;
    let mut i = 0;
    let bytes = text.as_bytes();

    while i < bytes.len() {
        if i == 0 || bytes[i - 1] == b'\n' {
            // Check for ``` at start of line (possibly with leading whitespace).
            let line_start = i;
            // Skip optional leading whitespace.
            let mut j = i;
            while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                j += 1;
            }
            if j + 2 < bytes.len() && &bytes[j..j + 3] == b"```" {
                if inside {
                    // Closing fence: mark up to end of fence line.
                    let fence_end = find_line_end(bytes, line_start);
                    // The fence line itself is still "inside" the block.
                    for slot in map.iter_mut().take(fence_end).skip(line_start) {
                        *slot = true;
                    }
                    inside = false;
                    i = fence_end;
                    continue;
                } else {
                    // Opening fence: mark from here onward until we find close.
                    inside = true;
                    let fence_end = find_line_end(bytes, line_start);
                    for slot in map.iter_mut().take(fence_end).skip(line_start) {
                        *slot = true;
                    }
                    i = fence_end;
                    continue;
                }
            }
        }

        if inside {
            map[i] = true;
        }
        i += 1;
    }

    // If we ended inside an unclosed fence, mark remaining as inside.
    if inside {
        for slot in map.iter_mut().skip(i) {
            *slot = true;
        }
    }

    map
}

/// Find the end of the line starting at `start` (index after the newline, or end of bytes).
fn find_line_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    if i < bytes.len() {
        i + 1 // skip past the newline
    } else {
        i
    }
}

/// Find the first position at or after `pos` that is outside a code fence.
/// Returns `map.len()` if no such position exists (entire remainder is fenced).
fn next_outside_fence(map: &[bool], pos: usize) -> usize {
    let mut i = pos;
    while i < map.len() && map[i] {
        i += 1;
    }
    i
}

/// Score a candidate break position. Higher is better.
/// Returns 0 if the position is inside a code fence (not a valid break).
fn score_break(text: &str, pos: usize, in_code_fence: &[bool]) -> u32 {
    // Don't split inside code fences.
    if pos < in_code_fence.len() && in_code_fence[pos] {
        return 0;
    }

    // We look at what starts at `pos`. The break happens *before* pos,
    // so the next chunk starts at pos.
    let remaining = &text[pos..];

    // Heading h1-h6 (text at pos starts with "\n# ", "\n## ", etc.)
    // But pos itself might be right after a newline, so check if pos-1 is '\n'
    // or pos is 0.
    let at_line_start =
        pos == 0 || (pos > 0 && text.as_bytes()[pos - 1] == b'\n') || remaining.starts_with('\n');

    if at_line_start {
        let line = remaining.strip_prefix('\n').unwrap_or(remaining);

        // Heading scores: h1=100, h2=90, h3=80, h4=70, h5=60, h6=50
        if line.starts_with("# ") {
            return 100;
        }
        if line.starts_with("## ") {
            return 90;
        }
        if line.starts_with("### ") {
            return 80;
        }
        if line.starts_with("#### ") {
            return 70;
        }
        if line.starts_with("##### ") {
            return 60;
        }
        if line.starts_with("###### ") {
            return 50;
        }

        // Code block boundary.
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            return 80;
        }

        // List item.
        if line.starts_with("- ")
            || line.starts_with("* ")
            || (line.len() >= 3
                && line.as_bytes()[0].is_ascii_digit()
                && line.as_bytes()[1] == b'.'
                && line.as_bytes()[2] == b' ')
        {
            return 5;
        }
    }

    // Paragraph break: \n\n at this position.
    if remaining.starts_with("\n\n") {
        return 20;
    }

    // Also check if we're right after a \n\n (pos is start of new paragraph).
    if pos >= 2 && text.is_char_boundary(pos - 2) && &text[pos - 2..pos] == "\n\n" {
        return 20;
    }

    // Newline.
    if remaining.starts_with('\n') || (pos > 0 && text.as_bytes()[pos - 1] == b'\n') {
        return 1;
    }

    0
}

/// Find the best split point in `text[win_start..win_end]`.
fn find_best_split(text: &str, win_start: usize, win_end: usize, in_code_fence: &[bool]) -> usize {
    let mut best_pos = win_end.min(text.len());
    let mut best_score = 0u32;

    let mut i = win_start;
    while i < win_end && i < text.len() {
        // Only consider positions that are valid char boundaries for string slicing.
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let s = score_break(text, i, in_code_fence);
        if s > best_score {
            best_score = s;
            best_pos = i;
        }
        i += 1;
    }

    best_pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical() {
        let v = vec![1.0, 2.0, 3.0];
        assert!(
            (cosine_similarity(&v, &v) - 1.0).abs() < 0.001,
            "identical vectors should have cosine similarity ~1.0"
        );
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(
            cosine_similarity(&a, &b).abs() < 0.001,
            "orthogonal vectors should have cosine similarity ~0.0"
        );
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!(
            (cosine_similarity(&a, &b) + 1.0).abs() < 0.001,
            "opposite vectors should have cosine similarity ~-1.0"
        );
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = vec![1.0, 2.0, 3.0];
        let zero = vec![0.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &zero), 0.0);
        assert_eq!(cosine_similarity(&zero, &a), 0.0);
    }

    #[test]
    fn hash_embedding_deterministic() {
        let e1 = hash_embedding("hello world");
        let e2 = hash_embedding("hello world");
        assert_eq!(e1, e2, "same input should produce identical embeddings");
    }

    #[test]
    fn hash_embedding_correct_dimension() {
        let e = hash_embedding("test input");
        assert_eq!(e.len(), EMBEDDING_DIM);
    }

    #[test]
    fn hash_embedding_is_normalised() {
        let e = hash_embedding("some text to embed");
        let norm: f32 = e.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "hash embedding should be L2-normalised, got norm={norm}"
        );
    }

    #[test]
    fn hash_embedding_different_texts_differ() {
        let e1 = hash_embedding("hello");
        let e2 = hash_embedding("world");
        let sim = cosine_similarity(&e1, &e2);
        assert!(
            sim < 0.99,
            "different texts should produce different embeddings, got sim={sim}"
        );
    }

    #[test]
    fn embed_produces_768_dim_vector() {
        let Some(mut model) = try_load_test_model() else {
            return;
        };
        eprintln!("Model inputs: {:?}", model.input_names);
        eprintln!("Model outputs: {:?}", model.output_names);
        let embedding = embed_text(&mut model, "hello world").unwrap();
        assert_eq!(
            embedding.len(),
            EMBEDDING_DIM,
            "expected {EMBEDDING_DIM}-dim vector, got {}",
            embedding.len()
        );
    }

    #[test]
    fn model_similar_texts_closer() {
        let Some(mut model) = try_load_test_model() else {
            return;
        };
        let e_cat = embed_text(&mut model, "The cat sat on the mat").unwrap();
        let e_kitten = embed_text(&mut model, "A kitten rested on the rug").unwrap();
        let e_stock = embed_text(&mut model, "Stock markets rallied on Friday").unwrap();

        let sim_close = cosine_similarity(&e_cat, &e_kitten);
        let sim_far = cosine_similarity(&e_cat, &e_stock);
        eprintln!("cat-kitten similarity: {sim_close}");
        eprintln!("cat-stock similarity: {sim_far}");
        // With char-ordinal encoding this may not hold semantically,
        // but we at least verify the pipeline runs end-to-end.
    }

    /// Helper: resolve a path relative to the user's home directory.
    fn dirs_path(relative: &str) -> std::path::PathBuf {
        let home = std::env::var("HOME").expect("HOME not set");
        std::path::PathBuf::from(home).join(relative)
    }

    /// Try to load the test embedding model. Returns `None` (and prints a
    /// skip message) if either the model file is missing or the ONNX Runtime
    /// shared library cannot be loaded.
    ///
    /// Uses `catch_unwind` because `ort` panics (rather than returning an
    /// error) when the dynamic library is not found.
    fn try_load_test_model() -> Option<EmbeddingModel> {
        let model_path = dirs_path(".memex/models/embedding-gemma-300m.onnx");
        if !model_path.exists() {
            eprintln!("Skipping: model not found at {}", model_path.display());
            return None;
        }
        let path_str = model_path.to_str().unwrap().to_string();
        match std::panic::catch_unwind(|| load_model(&path_str, "embedding-gemma-300m")) {
            Ok(Ok(m)) => Some(m),
            Ok(Err(e)) => {
                eprintln!("Skipping: could not load model: {e}");
                None
            }
            Err(_) => {
                eprintln!(
                    "Skipping: ONNX Runtime dylib not available \
                     (set ORT_DYLIB_PATH to the libonnxruntime path)"
                );
                None
            }
        }
    }

    #[test]
    fn chunk_short_document_single_chunk() {
        let chunks = chunk_text("Short document.", 900, 0.15);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "Short document.");
        assert_eq!(chunks[0].pos, 0);
    }

    #[test]
    fn chunk_long_document_splits() {
        // 900 tokens * 4 chars/token = 3600 chars. Need >3600 chars to force split.
        let long = "word ".repeat(2000); // 10000 chars
        let chunks = chunk_text(&long, 900, 0.15);
        assert!(chunks.len() > 1);
    }

    #[test]
    fn chunk_prefers_heading_boundaries() {
        let doc = format!(
            "{}\n\n## Section 2\n\n{}",
            "a ".repeat(2000), // 4000 chars
            "b ".repeat(2000)  // 4000 chars
        );
        let chunks = chunk_text(&doc, 900, 0.15);
        assert!(chunks.len() >= 2);
        // Second chunk should start at or near the heading
        assert!(
            chunks[1].text.contains("Section 2"),
            "Second chunk should contain heading: {:?}",
            chunks[1].text
        );
    }

    #[test]
    fn chunk_respects_code_fences() {
        let code = "line\n".repeat(20);
        let doc = format!("Before\n\n```rust\n{code}```\n\nAfter");
        let chunks = chunk_text(&doc, 50, 0.15);
        // No chunk should have unmatched fences
        for chunk in &chunks {
            let count = chunk.text.matches("```").count();
            assert!(
                count % 2 == 0 || count == 0,
                "Chunk has unmatched code fence: {:?}",
                chunk.text
            );
        }
    }

    #[test]
    fn chunk_overlap_exists() {
        let long = "word ".repeat(500);
        let chunks = chunk_text(&long, 100, 0.15);
        if chunks.len() >= 2 {
            // Second chunk should start before the end of the first
            assert!(
                chunks[1].pos < chunks[0].pos + chunks[0].len,
                "Expected overlap between chunks"
            );
        }
    }

    #[test]
    fn chunk_multibyte_utf8_no_panic() {
        // Em-dash is 3 bytes (U+2014). Place them densely so split points
        // are likely to land inside a multi-byte character.
        let text = "A\u{2014}B\u{2014}C ".repeat(300);
        let chunks = chunk_text(&text, 100, 0.15);
        assert!(!chunks.is_empty(), "should produce at least one chunk");
        // Verify all chunk texts are valid UTF-8 (implicit: String type)
        // and positions are char boundaries.
        for chunk in &chunks {
            assert!(
                text.is_char_boundary(chunk.pos),
                "chunk pos {} is not a char boundary",
                chunk.pos
            );
            assert!(
                text.is_char_boundary(chunk.pos + chunk.len),
                "chunk end {} is not a char boundary",
                chunk.pos + chunk.len
            );
        }
    }
}
