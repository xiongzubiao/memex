//! Regression guard for the QMD-style truncation pattern (tokenize
//! content → slice to N-4 → re-encode-with-specials). Embeddings from
//! the in-tree `embed_text` must match a manual implementation of QMD's
//! pattern at the 2048-token boundary; a divergence indicates regression
//! to the older encode-then-take-N approach which loses the EOS token.
//!
//! Skipped at runtime when the ONNX model or tokenizer.json isn't
//! installed.

mod common;

use memex_core::embed::{cosine_similarity, embed_text, EmbeddingModel};

/// Memex's current behavior: tokenizer.encode(text, true) → take(N).
/// Truncates from the END of the combined sequence (BOS + content + EOS).
fn embed_memex_current(model: &mut EmbeddingModel, text: &str) -> Vec<f32> {
    embed_text(model, text).unwrap()
}

/// QMD-style: tokenize content WITHOUT specials → truncate content to N-4
/// → detokenize → re-encode WITH specials. Preserves EOS at the boundary.
fn embed_qmd_style(model: &mut EmbeddingModel, text: &str) -> Vec<f32> {
    let max_tokens = 2048_usize;
    let safe_limit = max_tokens - 4;
    let encoded_no_specials = model.tokenizer.encode(text, false).unwrap();
    let content_ids = encoded_no_specials.get_ids();
    let truncated_text = if content_ids.len() <= max_tokens {
        text.to_string()
    } else {
        let truncated_ids: Vec<u32> = content_ids[..safe_limit].to_vec();
        model.tokenizer.decode(&truncated_ids, true).unwrap()
    };
    embed_text(model, &truncated_text).unwrap()
}

/// Body that tokenizes to >2048 content tokens (forces truncation).
fn long_body_over_cap() -> String {
    let para = "Bearer tokens are short-lived authentication credentials sent in \
        the Authorization header to identify the calling user on each request. \
        They are issued by an OAuth provider and expire after a configurable \
        interval, typically between 15 minutes and 24 hours. The token itself \
        contains no user information; the server validates it by checking a \
        session store or JWT signature. Common pitfalls include logging the \
        token in plaintext, sending it over an unencrypted channel, or failing \
        to expire stale tokens after credential rotation. Modern OAuth flows \
        use refresh tokens to obtain new access tokens without re-prompting \
        the user, but refresh tokens themselves must be stored securely and \
        rotated on each use. Token revocation requires the server to maintain \
        a deny-list checked on every request, which adds latency but is the \
        only way to invalidate a token before its natural expiry. Stateless \
        JWTs trade revocation for scalability: there is no central session \
        store to query, but a leaked token remains valid until expiry. ";
    para.repeat(16)
}

#[test]
fn boundary_truncation_matches_qmd_pattern() {
    let Some(mut model) = common::try_load_embedding_model() else {
        return;
    };

    let over = long_body_over_cap();
    let v_a = embed_memex_current(&mut model, &over);
    let v_b = embed_qmd_style(&mut model, &over);
    let cos = cosine_similarity(&v_a, &v_b);
    assert!(
        cos > 0.999,
        "memex embed_text must match QMD's truncation pattern; cos={cos:.6} \
         indicates a regression to the encode-then-take-N approach"
    );
}
