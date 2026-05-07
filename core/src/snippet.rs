//! Chunk-direct snippet rendering + helpers used by `render_snippets`
//! in `retrieval.rs`. The earlier "focused snippet" line picker (with
//! per-chunk top-N line zooming) is gone — semantic-bounded chunks
//! produced by `chunking::chunk_full_pipeline` are tight enough that
//! re-ranking lines within a chunk added no signal over BM25 + RRF and
//! clipped useful surrounding context.

pub const INTENT_WEIGHT_CHUNK: f64 = 0.5;

pub(crate) const INTENT_STOP_WORDS: &[&str] = &[
    "the","a","an","and","or","but","if","then","else","when","is","are","was","were","be","been",
    "being","have","has","had","do","does","did","of","in","on","at","to","for","with","by",
    "about","as","into","through","during","before","after","above","below","up","down","out",
    "off","over","under","again","further","once","this","that","these","those","i","you","he",
    "she","it","we","they","them","my","your","his","her","its","our","their","what","which",
    "who","whom","how","why","not","no","so","than","too","very","just",
];

pub fn extract_intent_terms(intent: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in intent.split_whitespace() {
        let lower = raw.to_lowercase();
        let trimmed: String = lower
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        if trimmed.chars().count() <= 1 { continue; }
        if INTENT_STOP_WORDS.contains(&trimmed.as_str()) { continue; }
        out.push(trimmed);
    }
    out
}

/// BM25 best-chunk scorer: chunk-text scored by query terms (weight
/// 1.0) plus intent terms at `INTENT_WEIGHT_CHUNK = 0.5`.
/// Per-term contribution is **binary** (1.0 if the term appears
/// anywhere in the chunk, 0 otherwise) — not term-frequency. Mirrors
/// QMD `src/store.ts:4141-4151`.
///
/// Used by `render_snippets` only when a result has `chunk_seq=None`
/// (legacy / doc-granularity paths). When chunk_seq is `Some`, the
/// retrieved chunk is rendered directly without re-ranking.
pub fn score_chunk(chunk_text: &str, query_terms: &[String], intent_terms: &[String]) -> f64 {
    let lower = chunk_text.to_lowercase();
    let mut score = 0.0_f64;
    for q in query_terms {
        if lower.contains(q.as_str()) {
            score += 1.0;
        }
    }
    for it in intent_terms {
        if lower.contains(it.as_str()) {
            score += INTENT_WEIGHT_CHUNK;
        }
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- intent_terms_*

    #[test]
    fn intent_terms_lowercase_and_filter_stopwords() {
        let terms = extract_intent_terms("the performance of WEB pages on mobile");
        assert!(terms.contains(&"performance".to_string()));
        assert!(terms.contains(&"web".to_string()));
        assert!(terms.contains(&"pages".to_string()));
        assert!(terms.contains(&"mobile".to_string()));
        assert!(!terms.iter().any(|t| t == "the"));
        assert!(!terms.iter().any(|t| t == "of"));
        assert!(!terms.iter().any(|t| t == "on"));
    }

    #[test]
    fn intent_terms_strip_edge_punctuation_keep_internal() {
        let terms = extract_intent_terms("Node.js: API endpoints");
        assert!(terms.contains(&"node.js".to_string()));
        assert!(terms.contains(&"api".to_string()));
        assert!(terms.contains(&"endpoints".to_string()));
    }

    #[test]
    fn intent_terms_drop_single_char() {
        let terms = extract_intent_terms("C++ a b cd");
        assert!(!terms.iter().any(|t| t == "a" || t == "b" || t == "c"));
        assert!(terms.contains(&"cd".to_string()));
    }

    #[test]
    fn intent_terms_empty_input_returns_empty() {
        assert!(extract_intent_terms("").is_empty());
        assert!(extract_intent_terms("   ").is_empty());
    }

    // --- score_chunk_*

    #[test]
    fn score_chunk_query_term_dominates_intent_term() {
        let q = vec!["performance".to_string()];
        let i = vec!["web".to_string()];
        let s_query = score_chunk("performance is great", &q, &i);
        let s_intent = score_chunk("web is great", &q, &i);
        assert!(s_query > s_intent, "query weight 1.0 > intent weight 0.5");
    }

    #[test]
    fn score_chunk_intent_adds_at_half_weight() {
        let q = vec!["foo".to_string()];
        let i = vec!["bar".to_string()];
        let s_q = score_chunk("foo only", &q, &i);
        let s_qi = score_chunk("foo bar", &q, &i);
        assert!((s_qi - s_q - INTENT_WEIGHT_CHUNK).abs() < 1e-9);
    }

    #[test]
    fn score_chunk_is_binary_per_term_not_term_frequency() {
        // Mirrors QMD store.ts:4146 — `lower.includes(term) ? 1 : 0`,
        // not `count(term)`. A chunk that merely covers all query terms
        // shouldn't be beaten by one that repeats one term many times.
        let q = vec!["foo".to_string()];
        let s_once = score_chunk("foo once", &q, &[]);
        let s_many = score_chunk("foo foo foo foo foo", &q, &[]);
        assert_eq!(s_once, s_many, "score must be binary per term, not term-frequency");
        assert_eq!(s_once, 1.0);
    }

    #[test]
    fn score_chunk_zero_when_no_terms_match() {
        let q = vec!["x".to_string()];
        let i = vec!["y".to_string()];
        assert_eq!(score_chunk("nothing here", &q, &i), 0.0);
    }

}
