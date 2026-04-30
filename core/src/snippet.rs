//! Focused snippet extraction + intent term tokenization.
//! Mirrors QMD `src/store.ts:3811-3877`.

pub const INTENT_WEIGHT_SNIPPET: f64 = 0.3;
pub const INTENT_WEIGHT_CHUNK: f64 = 0.5;
pub const SNIPPET_MAX_LEN: usize = 300;
const SNIPPET_CONTEXT_PAD: usize = 100;
const SNIPPET_WINDOW_BEFORE: u32 = 1;
const SNIPPET_WINDOW_AFTER: u32 = 2;

const INTENT_STOP_WORDS: &[&str] = &[
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
/// QMD `src/store.ts:4141-4151` exactly:
///
/// ```js
/// let score = queryTerms.reduce((acc, t) => acc + (lower.includes(t) ? 1 : 0), 0);
/// for (const t of intentTerms) if (lower.includes(t)) score += INTENT_WEIGHT_CHUNK;
/// ```
///
/// Binary scoring keeps a chunk that simply *covers* both query terms
/// from being beaten by a chunk that repeats one of them many times.
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

/// Returns the diff-header-prefixed, line-numbered snippet string.
/// The header itself encodes focus_line / lines_before / lines_after /
/// snippet_lines — agents and CLI parse them directly from the header
/// (`@@ -49,4 @@ (48 before, 48 after)`), so the function returns just
/// the rendered text. QMD's `extractSnippet` returns the same fields
/// programmatically but no caller in QMD reads them either.
pub fn extract_focused_snippet(
    body: &str,
    chunk_pos: usize,
    chunk_len: usize,
    primary_query: &str,
    intent: Option<&str>,
    max_len: usize,
) -> String {
    if body.is_empty() {
        return "@@ -1,1 @@ (0 before, 0 after)\n1: ".into();
    }
    let body_len = body.len();
    let chunk_pos = chunk_pos.min(body_len);
    let chunk_end = (chunk_pos + chunk_len).min(body_len);
    let context_start = chunk_pos.saturating_sub(SNIPPET_CONTEXT_PAD);
    let context_end = (chunk_end + SNIPPET_CONTEXT_PAD).min(body_len);
    let context_start = floor_char_boundary(body, context_start);
    let context_end = ceil_char_boundary(body, context_end);
    let search_body = &body[context_start..context_end];
    let line_offset = body[..context_start].matches('\n').count() as u32;

    let lines: Vec<&str> = search_body.split('\n').collect();
    let query_terms: Vec<String> = primary_query
        .split_whitespace()
        .map(|s| s.to_lowercase())
        .collect();
    let intent_terms = intent.map(extract_intent_terms).unwrap_or_default();

    let mut best = 0usize;
    let mut best_score = -1.0_f64;
    for (i, line) in lines.iter().enumerate() {
        let lower = line.to_lowercase();
        let mut s = 0.0_f64;
        for q in &query_terms {
            s += lower.matches(q.as_str()).count() as f64;
        }
        for it in &intent_terms {
            // Exact substring match.
            let exact = lower.matches(it.as_str()).count() as f64;
            // Prefix match: any whitespace-delimited word in the line that is a
            // prefix of the intent term (e.g. "perf" matches "performance").
            let prefix: f64 = lower
                .split_whitespace()
                .filter(|w| w.len() > 1 && it.starts_with(*w))
                .count() as f64;
            s += exact.max(prefix) * INTENT_WEIGHT_SNIPPET;
        }
        if s > best_score {
            best_score = s;
            best = i;
        }
    }

    let win_start = best.saturating_sub(SNIPPET_WINDOW_BEFORE as usize);
    let win_end = (best + SNIPPET_WINDOW_AFTER as usize + 1).min(lines.len());
    let mut snippet_text = lines[win_start..win_end].join("\n");
    if snippet_text.len() > max_len {
        let cut = max_len.saturating_sub(3);
        let cut = floor_char_boundary(&snippet_text, cut);
        snippet_text.truncate(cut);
        snippet_text.push_str("...");
    }

    let doc_total_lines = body.matches('\n').count() as u32 + 1;
    let doc_start = line_offset + win_start as u32 + 1;
    let count = (win_end - win_start) as u32;
    let lines_before = doc_start - 1;
    let lines_after = doc_total_lines.saturating_sub(doc_start + count - 1);
    let header = format!(
        "@@ -{},{} @@ ({} before, {} after)",
        doc_start, count, lines_before, lines_after
    );
    let numbered = number_lines(&snippet_text, doc_start);
    format!("{header}\n{numbered}")
}

fn number_lines(text: &str, start_lineno: u32) -> String {
    text.split('\n')
        .enumerate()
        .map(|(i, line)| format!("{}: {}", start_lineno + i as u32, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
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

    // --- focused_snippet_*

    #[test]
    fn focused_snippet_picks_best_query_line() {
        let body = "intro line\n# Title\nbody mentions performance several times.\nfooter";
        let chunk_pos = 0;
        let chunk_len = body.len();
        let s = extract_focused_snippet(body, chunk_pos, chunk_len, "performance", None, SNIPPET_MAX_LEN);
        assert!(s.contains("performance"));
        assert!(s.starts_with("@@ -"));
    }

    #[test]
    fn focused_snippet_doc_relative_line_numbers() {
        let body: String = (1..=20).map(|i| format!("line {i}\n")).collect::<String>()
            + "needle\nafter1\nafter2\n";
        let chunk_pos = body.find("needle").unwrap();
        let chunk_len = body.len() - chunk_pos;
        let s = extract_focused_snippet(&body, chunk_pos, chunk_len, "needle", None, SNIPPET_MAX_LEN);
        // The numbered output should label the matching line "21:" — the
        // doc-relative line number of "needle" — even though the diff
        // header itself starts one line earlier (SNIPPET_WINDOW_BEFORE=1).
        assert!(s.contains("21: needle"), "expected `21: needle` in numbered output: {s}");
    }

    #[test]
    fn focused_snippet_truncates_with_ellipsis_when_over_max_len() {
        let big_line: String = "x".repeat(500);
        let body = format!("{big_line}\nneedle line\n{big_line}\n");
        let s = extract_focused_snippet(&body, 0, body.len(), "needle", None, SNIPPET_MAX_LEN);
        assert!(s.len() <= 400, "snippet should be capped, got {} chars", s.len());
        assert!(s.ends_with("..."));
    }

    #[test]
    fn focused_snippet_handles_empty_body() {
        let s = extract_focused_snippet("", 0, 0, "anything", None, SNIPPET_MAX_LEN);
        assert!(s.starts_with("@@ -1,1 @@"));
    }

    #[test]
    fn focused_snippet_intent_changes_chosen_line() {
        let body = "line A unrelated\nline B has perf\nline C nothing\n";
        let s1 = extract_focused_snippet(body, 0, body.len(), "missing", None, SNIPPET_MAX_LEN);
        let s2 = extract_focused_snippet(body, 0, body.len(), "missing", Some("performance"), SNIPPET_MAX_LEN);
        assert_ne!(s1, s2, "intent should change the chosen line");
    }

    #[test]
    fn focused_snippet_byte_offsets_safe_with_multibyte() {
        let body = "前段\n中文标题\n中文正文一\n中文正文二\n";
        let chunk_pos = 0;
        let chunk_len = body.len();
        let s = extract_focused_snippet(body, chunk_pos, chunk_len, "正文一", None, SNIPPET_MAX_LEN);
        assert!(s.contains("正文一"));
    }
}
