use ort::session::Session;
use ort::value::Tensor;

use std::path::PathBuf;
use std::sync::OnceLock;

/// Keep the dylib Library handle alive for the process lifetime so the
/// subsequent `ort::Session::builder` dlopen is a refcount bump, not a
/// second full load.
static LOADED_DYLIB: OnceLock<libloading::Library> = OnceLock::new();

/// Candidate dylib paths, in priority order. First `~/.memex/lib/` (where
/// the postinstall places it), then platform-standard install locations.
fn candidate_dylib_paths() -> Vec<PathBuf> {
    let lib_name = if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".memex/lib").join(lib_name));
    }
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from("/opt/homebrew/lib").join(lib_name));
        candidates.push(PathBuf::from("/usr/local/lib").join(lib_name));
    } else if cfg!(target_os = "linux") {
        candidates.push(PathBuf::from("/usr/lib").join(lib_name));
        candidates.push(PathBuf::from("/usr/lib/x86_64-linux-gnu").join(lib_name));
    } else if cfg!(target_os = "windows") {
        if let Ok(pf) = std::env::var("ProgramFiles") {
            candidates.push(PathBuf::from(pf).join("onnxruntime/lib").join(lib_name));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local).join("onnxruntime/lib").join(lib_name));
        }
    }
    candidates
}

/// Discover and initialize the ONNX Runtime dylib from known install
/// locations. `ort::init_from` alone is lazy and stores the path without
/// loading, so bad or missing dylibs surface much later as a deadlock
/// inside `Session::builder`. We pre-validate via a real `dlopen` and
/// keep the Library handle alive for the process so ort's subsequent
/// dlopen is a refcount bump rather than a second full load.
///
/// Returns `Err` listing checked paths if no dylib is found.
pub fn init_runtime() -> crate::error::Result<()> {
    if LOADED_DYLIB.get().is_some() {
        return Ok(());
    }
    let candidates = candidate_dylib_paths();
    for path in &candidates {
        if !path.exists() {
            continue;
        }
        let lib = unsafe { libloading::Library::new(path) }.map_err(|e| {
            crate::error::MemexError::Io(std::io::Error::other(format!("dlopen failed: {e}")))
        })?;
        let _ = LOADED_DYLIB.set(lib);
        ort::init_from(path)?.commit();
        return Ok(());
    }
    let checked = candidates
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n  ");
    Err(crate::error::MemexError::Io(std::io::Error::other(
        format!(
            "libonnxruntime not found. Checked:\n  {checked}\n\
             Install via `brew install onnxruntime` (macOS), your distro's \
             package manager (Linux), or download from \
             https://github.com/microsoft/onnxruntime/releases and place \
             the dylib at ~/.memex/lib/."
        ),
    )))
}

/// Run a closure with panic output silenced, returning `Some(R)` on success
/// or `None` if the closure panicked.
///
/// The `ort` crate panics (rather than returning an error) when the ONNX
/// Runtime shared library cannot be loaded. `catch_unwind` catches the panic,
/// but the default hook still prints a noisy backtrace to stderr. This helper
/// installs a no-op panic hook for the duration of the call, then restores
/// the original hook afterwards. The panic payload is discarded.
pub fn catch_unwind_silent<F: FnOnce() -> R + std::panic::UnwindSafe, R>(f: F) -> Option<R> {
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

/// Maximum sequence length (tokens, including special tokens) accepted by
/// the embedding model. Equal to embedding-gemma's trained context window.
/// Mirrors QMD's `EMBED_CONTEXT_SIZE` (qmd/src/llm.ts:867); kept as a
/// constant rather than env-overridable because memex pins to a single
/// embedding model — `embedding-gemma-300m`. If a future swap to a model
/// with a different context window happens, change this constant alongside
/// `CURRENT_MODEL_NAME`.
pub const EMBED_CONTEXT_SIZE: usize = 2048;

/// Margin reserved for special tokens added by the tokenizer (BOS+EOS).
/// Content tokens are truncated to `EMBED_CONTEXT_SIZE - SPECIAL_TOKEN_MARGIN`
/// before re-encoding, so the final input fits under the cap with the EOS
/// preserved. Matches QMD's `safeLimit = maxTokens - 4` (qmd/src/llm.ts:969).
pub const SPECIAL_TOKEN_MARGIN: usize = 4;

/// Producer of fixed-dimension embedding vectors. Implemented by the
/// real ONNX-backed `EmbeddingModel` in production and by `MockEmbedder`
/// in tests, so daemon code that needs a model can stay agnostic to
/// whether ONNX is loaded.
pub trait Embedder: Send {
    /// Human-readable model identifier — written to the
    /// `documents.embed_model` column so stale-embedding detection can
    /// see when the model has been swapped.
    fn model_name(&self) -> &str;
    /// Embed a single text. Vectors are `EMBEDDING_DIM` floats.
    fn embed_text(&mut self, text: &str) -> crate::error::Result<Vec<f32>>;
    /// Embed multiple texts in one call. Equivalent to calling
    /// `embed_text` per item but batched at the implementation level
    /// for `EmbeddingModel` (one ONNX session.run instead of N).
    fn embed_batch(&mut self, texts: &[&str]) -> crate::error::Result<Vec<Vec<f32>>>;
}

/// An ONNX embedding model loaded into an inference session.
pub struct EmbeddingModel {
    session: Session,
    /// Human-readable model name (e.g. "embedding-gemma-300m").
    pub model_name: String,
    /// Names of the model's input tensors, in order.
    pub input_names: Vec<String>,
    /// Names of the model's output tensors, in order.
    pub output_names: Vec<String>,
    /// HuggingFace BPE tokenizer matched to the model (`tokenizer.json`).
    pub tokenizer: tokenizers::Tokenizer,
}

impl Embedder for EmbeddingModel {
    fn model_name(&self) -> &str {
        &self.model_name
    }
    fn embed_text(&mut self, text: &str) -> crate::error::Result<Vec<f32>> {
        embed_text(self, text)
    }
    fn embed_batch(&mut self, texts: &[&str]) -> crate::error::Result<Vec<Vec<f32>>> {
        embed_batch(self, texts)
    }
}

/// FNV-1a-hashed pseudo-embeddings for tests that need an `Embedder`
/// but don't exercise vector search semantics. Deterministic per input,
/// shape-correct (`EMBEDDING_DIM`), but **not** semantically meaningful —
/// don't use this to assert "X is closer to Y than Z" claims.
#[cfg(any(test, feature = "test-utils"))]
pub struct MockEmbedder;

#[cfg(any(test, feature = "test-utils"))]
impl Embedder for MockEmbedder {
    fn model_name(&self) -> &str {
        "mock-embedder"
    }
    fn embed_text(&mut self, text: &str) -> crate::error::Result<Vec<f32>> {
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        for token in text.split_whitespace() {
            let mut hash = 2166136261u32;
            for b in token.as_bytes() {
                hash ^= *b as u32;
                hash = hash.wrapping_mul(16777619);
            }
            v[(hash as usize) % EMBEDDING_DIM] += 1.0;
        }
        Ok(v)
    }
    fn embed_batch(&mut self, texts: &[&str]) -> crate::error::Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed_text(t)).collect()
    }
}

/// Load an ONNX embedding model from the given path.
///
/// Inspects the model graph to record input/output tensor names, which are
/// needed later for `embed_text`. The caller resolves and loads the
/// matching `tokenizer.json`; if that's missing, it's a broken install,
/// not a degraded mode.
pub fn load_model(
    path: &std::path::Path,
    model_name: &str,
    tokenizer: tokenizers::Tokenizer,
) -> crate::error::Result<EmbeddingModel> {
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
        tokenizer,
    })
}

/// Embed a text string into a vector using the model's BPE tokenizer.
///
/// If the model's input names are unrecognised, returns an error.
pub fn embed_text(model: &mut EmbeddingModel, text: &str) -> crate::error::Result<Vec<f32>> {
    let v = embed_batch(model, &[text])?;
    Ok(v.into_iter().next().expect("embed_batch returns at least one row"))
}

/// Embed multiple texts in a single ONNX inference call.
///
/// Tokenizes each input, pads all rows to the longest length in the batch
/// (capped at `EMBED_CONTEXT_SIZE`), runs one `session.run`, then
/// mean-pools each row over only its non-padded positions using the
/// attention mask. Returns embeddings in input order.
///
/// Per-row mask-aware pooling is essential — a naive mean over the full
/// padded sequence would bias embeddings toward the all-zero pad-token
/// vector and degrade quality, especially for short rows in a batch with
/// long ones.
///
/// Empty input returns an empty Vec. An empty string within the batch
/// gets a single padding-only token (matching `embed_text`'s behavior).
pub fn embed_batch(
    model: &mut EmbeddingModel,
    texts: &[&str],
) -> crate::error::Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }

    let has_input_ids = model.input_names.iter().any(|n| n == "input_ids");
    let has_attention_mask = model.input_names.iter().any(|n| n == "attention_mask");
    if !(has_input_ids && has_attention_mask) {
        return Err(crate::error::MemexError::Other(anyhow::anyhow!(
            "unrecognized model input format: expected input_ids + attention_mask"
        )));
    }

    // Cap at the model's context window. See module-level constants.
    let max_len = EMBED_CONTEXT_SIZE;
    // When a body's content tokens exceed this limit, we truncate the
    // CONTENT to `max_len - SPECIAL_TOKEN_MARGIN` then re-encode WITH
    // specials, producing BOS + truncated_content + EOS ≤ max_len. This
    // preserves EOS at the boundary, which a naive `encode(text, true) +
    // take(max_len)` would drop. Measured impact: cos = 0.988 between
    // truncate-with-EOS and truncate-without-EOS for a 3000-token body;
    // QMD-style scoring is +0.003-0.010 better on retrieval against the
    // affected doc. See `core/tests/truncation_experiment.rs`.
    let safe_content_len = max_len.saturating_sub(SPECIAL_TOKEN_MARGIN);

    // Step 1: tokenize each input.
    let tk = &model.tokenizer;
    let mut rows: Vec<Vec<i64>> = Vec::with_capacity(texts.len());
    for text in texts {
        // Probe the content length without specials.
        let probe = tk
            .encode(*text, false)
            .map_err(|e| crate::error::MemexError::Other(anyhow::anyhow!("tokenize: {e}")))?;
        let content_ids = probe.get_ids();
        let ids: Vec<i64> = if content_ids.len() <= safe_content_len {
            // Fits — encode with specials directly.
            let encoded = tk
                .encode(*text, true)
                .map_err(|e| crate::error::MemexError::Other(anyhow::anyhow!("tokenize: {e}")))?;
            encoded.get_ids().iter().map(|&u| u as i64).collect()
        } else {
            // Over cap — truncate content, decode back, re-encode with specials.
            let truncated_ids: Vec<u32> = content_ids[..safe_content_len].to_vec();
            let truncated_text = tk
                .decode(&truncated_ids, true)
                .map_err(|e| crate::error::MemexError::Other(anyhow::anyhow!("decode: {e}")))?;
            let encoded = tk.encode(truncated_text.as_str(), true).map_err(|e| {
                crate::error::MemexError::Other(anyhow::anyhow!("re-tokenize: {e}"))
            })?;
            // Defensive cap in case re-tokenization produced more tokens
            // than expected (e.g., tokenizer added more than BOS+EOS).
            encoded
                .get_ids()
                .iter()
                .take(max_len)
                .map(|&u| u as i64)
                .collect()
        };
        // Empty text → single padding-only token so we don't produce a 0-len row.
        let row = if ids.is_empty() { vec![0i64] } else { ids };
        rows.push(row);
    }

    // Step 2: pad to the longest row in the batch.
    let padded_len = rows.iter().map(|r| r.len()).max().unwrap_or(1);
    let pad_id: i64 = 0;
    let batch = texts.len();

    let mut input_ids: Vec<i64> = vec![pad_id; batch * padded_len];
    let mut attn: Vec<i64> = vec![0i64; batch * padded_len];
    let mut true_lens: Vec<usize> = Vec::with_capacity(batch);
    for (b, row) in rows.iter().enumerate() {
        for (t, &id) in row.iter().enumerate() {
            input_ids[b * padded_len + t] = id;
            attn[b * padded_len + t] = 1;
        }
        true_lens.push(row.len());
    }

    // Step 3: build tensors and run inference.
    let input_ids_tensor =
        Tensor::from_array(([batch, padded_len], input_ids.into_boxed_slice()))?;
    let attn_tensor = Tensor::from_array(([batch, padded_len], attn.into_boxed_slice()))?;

    let has_token_type_ids = model.input_names.iter().any(|n| n == "token_type_ids");
    let outputs = if has_token_type_ids {
        let token_type_ids_data: Vec<i64> = vec![0i64; batch * padded_len];
        let token_type_ids_tensor = Tensor::from_array((
            [batch, padded_len],
            token_type_ids_data.into_boxed_slice(),
        ))?;
        model.session.run(ort::inputs! {
            "input_ids" => input_ids_tensor,
            "attention_mask" => attn_tensor,
            "token_type_ids" => token_type_ids_tensor,
        })?
    } else {
        model.session.run(ort::inputs! {
            "input_ids" => input_ids_tensor,
            "attention_mask" => attn_tensor,
        })?
    };

    // Step 4: extract output and mean-pool per row using the mask.
    let first_output_name = &model.output_names[0];
    let output = &outputs[first_output_name.as_str()];
    let (shape, data) = output.try_extract_tensor::<f32>()?;
    let dims: &[i64] = shape;
    let mut out: Vec<Vec<f32>> = Vec::with_capacity(batch);

    if dims.len() == 3 {
        // [batch, seq_len, hidden_dim] — mean-pool over seq_len, masked.
        let hidden_dim = dims[2] as usize;
        let seq = dims[1] as usize;
        for (b, &raw_len) in true_lens.iter().take(batch).enumerate() {
            let row_offset = b * seq * hidden_dim;
            let true_len = raw_len.min(seq);
            let mut emb = vec![0.0f32; hidden_dim];
            for t in 0..true_len {
                let pos = row_offset + t * hidden_dim;
                for d in 0..hidden_dim {
                    emb[d] += data[pos + d];
                }
            }
            if true_len > 0 {
                for v in &mut emb {
                    *v /= true_len as f32;
                }
            }
            out.push(emb);
        }
    } else if dims.len() == 2 {
        // [batch, hidden_dim] — already pooled.
        let hidden_dim = dims[1] as usize;
        for b in 0..batch {
            out.push(data[b * hidden_dim..(b + 1) * hidden_dim].to_vec());
        }
    } else {
        return Err(crate::error::MemexError::Other(anyhow::anyhow!(
            "unexpected embedding output rank: {}",
            dims.len()
        )));
    }

    Ok(out)
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


#[cfg(test)]
mod tests {
    use super::*;

    fn test_embed_text(text: &str) -> Vec<f32> {
        let mut embedding = vec![0.0f32; EMBEDDING_DIM];
        for token in text.split_whitespace() {
            let mut hash = 2166136261u32;
            for b in token.as_bytes() {
                hash ^= *b as u32;
                hash = hash.wrapping_mul(16777619);
            }
            let idx = (hash as usize) % EMBEDDING_DIM;
            embedding[idx] += 1.0;
        }
        embedding
    }

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
    fn embed_produces_768_dim_vector() {
        let embedding = test_embed_text("hello world");
        assert_eq!(
            embedding.len(),
            EMBEDDING_DIM,
            "expected {EMBEDDING_DIM}-dim vector, got {}",
            embedding.len()
        );
    }

    #[test]
    fn model_similar_texts_closer() {
        let e_cat = test_embed_text("The cat sat on the mat");
        let e_cat_variant = test_embed_text("The cat sat on a mat");
        let e_stock = test_embed_text("Stock markets rallied on Friday");

        let sim_close = cosine_similarity(&e_cat, &e_cat_variant);
        let sim_far = cosine_similarity(&e_cat, &e_stock);
        eprintln!("cat-variant similarity: {sim_close}");
        eprintln!("cat-stock similarity: {sim_far}");
        assert!(
            sim_close > sim_far,
            "expected nearby text to be closer than unrelated text"
        );
    }

}
