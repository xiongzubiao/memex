//! Verify `embed_text` handles edge inputs (empty, multi-byte, very
//! long, repeated calls) without panicking. Skipped at runtime when the
//! ONNX model isn't installed at `~/.memex/models/`.

mod common;

#[test]
fn embed_text_edge_cases_no_panic() {
    let Some(mut model) = common::try_load_embedding_model() else {
        return;
    };

    // Empty.
    let v = memex_core::embed::embed_text(&mut model, "").unwrap();
    assert_eq!(v.len(), memex_core::embed::EMBEDDING_DIM);

    // Multi-byte.
    let v = memex_core::embed::embed_text(
        &mut model,
        "こんにちは世界 🌍 café résumé naïve",
    )
    .unwrap();
    assert_eq!(v.len(), memex_core::embed::EMBEDDING_DIM);

    // Very long (>2048 tokens after tokenization). Should be truncated, not panic.
    let long = "the quick brown fox jumps over the lazy dog. ".repeat(1000);
    let v = memex_core::embed::embed_text(&mut model, &long).unwrap();
    assert_eq!(v.len(), memex_core::embed::EMBEDDING_DIM);

    // Repeated calls.
    for _ in 0..3 {
        let v = memex_core::embed::embed_text(&mut model, "hello world").unwrap();
        assert_eq!(v.len(), memex_core::embed::EMBEDDING_DIM);
    }
}
