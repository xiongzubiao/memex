//! Verify batched ONNX inference is numerically faithful and tolerates
//! variable-length inputs. Skipped at runtime when the ONNX model isn't
//! installed.

mod common;

#[test]
fn embed_batch_matches_embed_text_serial_within_tolerance() {
    let Some(mut model) = common::try_load_embedding_model() else {
        return;
    };

    let texts = [
        "task: search result | query: what are bearer tokens",
        "title: Auth | text: bearer tokens are short-lived credentials sent in the Authorization header",
        "title: Cooking | text: boil water then add pasta",
    ];

    let serial: Vec<Vec<f32>> = texts
        .iter()
        .map(|t| memex_core::embed::embed_text(&mut model, t).unwrap())
        .collect();

    let refs: Vec<&str> = texts.to_vec();
    let batched = memex_core::embed::embed_batch(&mut model, &refs).unwrap();

    // Threshold note: a single-row batch hits cos=1.0 (see
    // `embed_batch_single_row_matches_embed_text` below). With multiple
    // rows, ORT's CPU kernels execute slightly different partial-sum
    // orderings than single-row, and EmbeddingGemma's bidirectional
    // self-attention amplifies that across ~24 layers. Empirically
    // this produces ~0.985-0.995 cosine vs. serial — semantically
    // equivalent but below bit-equivalence. 0.98 floor catches genuine
    // masking/pooling regressions while tolerating ORT non-determinism.
    assert_eq!(serial.len(), batched.len());
    let mut min_cos: f32 = 1.0;
    for (i, (s, b)) in serial.iter().zip(batched.iter()).enumerate() {
        assert_eq!(s.len(), b.len(), "row {i} dim mismatch");
        let cos = memex_core::embed::cosine_similarity(s, b);
        if cos < min_cos {
            min_cos = cos;
        }
    }
    assert!(
        min_cos > 0.98,
        "minimum cosine across batch = {min_cos:.6} (expected > 0.98)"
    );
}

#[test]
fn embed_batch_handles_variable_lengths() {
    let Some(mut model) = common::try_load_embedding_model() else {
        return;
    };
    let texts: Vec<&str> = vec![
        "short",
        "a much longer sentence with quite a few more tokens than the first one",
    ];
    let v = memex_core::embed::embed_batch(&mut model, &texts).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].len(), memex_core::embed::EMBEDDING_DIM);
    assert_eq!(v[1].len(), memex_core::embed::EMBEDDING_DIM);
}

#[test]
fn embed_batch_empty_input_returns_empty() {
    let Some(mut model) = common::try_load_embedding_model() else {
        return;
    };
    let v = memex_core::embed::embed_batch(&mut model, &[]).unwrap();
    assert!(v.is_empty());
}

/// Single-element batch: numerically identical to the unbatched path
/// (no padding involved, single ONNX call). Isolates whether divergence
/// comes from batching plumbing vs. attention-mask leakage across pads.
#[test]
fn embed_batch_single_row_matches_embed_text() {
    let Some(mut model) = common::try_load_embedding_model() else {
        return;
    };
    let text = "title: Auth | text: bearer tokens are short-lived credentials sent in the Authorization header";
    let single = memex_core::embed::embed_text(&mut model, text).unwrap();
    let batched = memex_core::embed::embed_batch(&mut model, &[text]).unwrap();
    assert_eq!(batched.len(), 1);
    let cos = memex_core::embed::cosine_similarity(&single, &batched[0]);
    assert!(
        cos > 0.9999,
        "single-row batched should be ~identical to embed_text: cos={cos:.6}"
    );
}

/// Identical-input batch: when all rows are the same text, no padding
/// is involved (`padded_len == row.len()`). Within-batch rows must be
/// byte-identical to each other (same input → same output regardless of
/// position).
#[test]
fn embed_batch_identical_inputs_are_position_invariant() {
    let Some(mut model) = common::try_load_embedding_model() else {
        return;
    };
    let text =
        "title: Auth | text: bearer tokens authenticate API requests over HTTPS connections.";
    let batched = memex_core::embed::embed_batch(&mut model, &[text, text, text]).unwrap();
    for (d, (v0, v1)) in batched[0].iter().zip(batched[1].iter()).enumerate() {
        assert!(
            (v0 - v1).abs() < 1e-5,
            "batched rows 0 and 1 disagree at dim {d}: {v0} vs {v1}"
        );
    }
}
