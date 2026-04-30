//! Reciprocal Rank Fusion: merge multiple ranked search result lists
//! into a single fused ranking. Used to combine BM25 and vector search
//! lanes in `retrieval::hybrid_retrieve_expanded`.

use super::SearchResult;

/// RRF fusion: merge multiple ranked search result lists.
///
/// - `weights` is parallel to `lists` — `weights[i]` is the multiplier
///   for list `i`. Missing entries default to 1.0. Compose weights from
///   independent trust axes (e.g. wiki-doc_type × primary-probe).
/// - Formula: score(d) = sum_i(w_i / (k + rank_i(d))) + bonus(rank_i(d))
/// - Bonus: +0.05 for rank 1, +0.02 for ranks 2-3 (1-based)
/// - Post-fusion: reassign scores as 1/rank
pub fn rrf_fuse(lists: &[Vec<SearchResult>], weights: &[f32], k: u32) -> Vec<SearchResult> {
    // Single list: no fusion needed, just assign 1/rank scores directly.
    if lists.len() == 1 {
        return lists[0]
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mut r = r.clone();
                r.score = 1.0 / (i as f32 + 1.0);
                r
            })
            .collect();
    }

    let k_f = k as f32;

    // Accumulate scores per document path
    let mut scores: std::collections::HashMap<
        String,                     // path as string key
        (SearchResult, f32, usize), // (best result, rrf_score, best_rank_1based)
    > = std::collections::HashMap::new();

    for (list_idx, list) in lists.iter().enumerate() {
        let weight: f32 = weights.get(list_idx).copied().unwrap_or(1.0);

        for (rank_0based, result) in list.iter().enumerate() {
            let rank_1based = rank_0based + 1;
            let contribution = weight / (k_f + rank_1based as f32);

            // Bonus for top ranks (1-based)
            let bonus = if rank_1based == 1 {
                0.05_f32
            } else if rank_1based <= 3 {
                0.02_f32
            } else {
                0.0
            };

            let path_key = result.path.to_string_lossy().to_string();

            scores
                .entry(path_key)
                .and_modify(|(existing, rrf_score, best_rank)| {
                    *rrf_score += contribution + bonus;
                    if rank_1based < *best_rank {
                        *best_rank = rank_1based;
                        *existing = result.clone();
                    }
                })
                .or_insert_with(|| (result.clone(), contribution + bonus, rank_1based));
        }
    }

    // Sort by RRF score descending, then assign 1/rank as final score
    let mut sorted: Vec<(SearchResult, f32)> = scores
        .into_values()
        .map(|(result, rrf_score, _)| (result, rrf_score))
        .collect();
    sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    sorted
        .into_iter()
        .enumerate()
        .map(|(i, (mut result, _))| {
            result.score = 1.0 / (i as f32 + 1.0);
            result
        })
        .collect()
}
