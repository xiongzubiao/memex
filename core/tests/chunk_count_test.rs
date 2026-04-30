//! Regression guard: a 144KB body must produce ~50 chunks, not 6,000+.
//!
//! Pre-fix (commit 1456369), `chunk_text` set `window = max_chars` so
//! `find_best_cutoff` could pick a high-scoring break-point at or near
//! `start`, producing degenerate 1-byte chunks at every section boundary.
//! Total: 6,158 chunks for a 144KB body, taking ~23 minutes to embed.
//! This test catches a regression to that behavior.

#[test]
fn chunk_count_for_140kb_body_is_proportional() {
    let mut body = String::with_capacity(180_000);
    for i in 0..12 {
        body.push_str(&format!("# Section {i}\n\n"));
        body.push_str(&"x".repeat(12_000));
        body.push_str("\n\n");
    }

    let chunks = memex_core::chunking::chunk_text(&body, 900, 0.15);
    let n = chunks.len();

    // Expected: body_len / (max_chars - overlap) ≈ 144182 / 3060 ≈ 47.
    // Allow generous slack but reject anything over 200 (catches the bug).
    assert!(
        (40..=200).contains(&n),
        "expected 40-200 chunks for a 144KB body, got {n}; \
         likely the chunking window regression is back"
    );

    // No chunk should be smaller than overlap_chars (no degenerate emission).
    let overlap_chars = (900 * 4) as f64 * 0.15;
    let min_acceptable = (overlap_chars + 1.0) as usize;
    for c in &chunks {
        assert!(
            c.len >= min_acceptable || c.pos + c.len == body.len(),
            "chunk too small: pos={}, len={} (min {})",
            c.pos,
            c.len,
            min_acceptable
        );
    }
}
