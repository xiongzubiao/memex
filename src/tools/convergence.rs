use crate::types::*;

/// Parse LLM's semantic delta classification.
pub fn parse_semantic_delta(response: &str) -> SemanticDelta {
    let lower = response.trim().to_lowercase();
    if lower.starts_with("none") || lower.contains("no meaningful change") {
        SemanticDelta::None
    } else if lower.starts_with("small") || lower.contains("refinement") {
        SemanticDelta::Small
    } else if lower.starts_with("large") || lower.contains("rewrite") {
        SemanticDelta::Large
    } else {
        SemanticDelta::Small
    }
}

/// Parse relative score from LLM review output.
pub fn parse_relative_score(response: &str) -> RelativeScore {
    let lower = response.to_lowercase();
    if lower.contains("better") {
        RelativeScore::Better
    } else if lower.contains("worse") {
        RelativeScore::Worse
    } else {
        RelativeScore::Same
    }
}

/// Check consensus: no Worse votes means consensus.
pub fn check_consensus(votes: &[LlmVote]) -> bool {
    !votes.iter().any(|v| v.score == RelativeScore::Worse)
}

/// Detect irreconcilable objections via simple similarity.
/// In production, this uses embedding cosine similarity.
/// For unit tests, we use a basic word overlap heuristic.
pub fn detect_irreconcilable(history: &[String], _threshold: f64) -> bool {
    if history.len() < 2 {
        return false;
    }
    let last = &history[history.len() - 1];
    let prev = &history[history.len() - 2];
    let sim = word_overlap_similarity(prev, last);
    sim > 0.5
}

/// Word overlap Jaccard similarity (fallback for when embeddings aren't available).
fn word_overlap_similarity(a: &str, b: &str) -> f64 {
    let lower_a = a.to_lowercase();
    let lower_b = b.to_lowercase();
    let words_a: std::collections::HashSet<&str> = lower_a.split_whitespace().collect();
    let words_b: std::collections::HashSet<&str> = lower_b.split_whitespace().collect();
    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }
    let intersection = words_a.intersection(&words_b).count() as f64;
    let union = words_a.union(&words_b).count() as f64;
    intersection / union
}

/// Build the final convergence result from per-section statuses.
pub fn build_convergence_result(sections: &[SectionConvergence]) -> ConvergenceResult {
    let all_converged = sections.iter().all(|s| s.converged);
    ConvergenceResult {
        sections: sections.to_vec(),
        all_converged,
        should_loop: !all_converged,
    }
}

/// Evaluate whether a section has converged based on all signals.
pub fn evaluate_section(
    name: &str,
    votes: &[LlmVote],
    semantic_delta: SemanticDelta,
    objection_history: &[String],
) -> SectionConvergence {
    let consensus = check_consensus(votes);
    let majority_positive = if votes.is_empty() {
        true // No votes = no objections
    } else {
        let non_worse = votes
            .iter()
            .filter(|v| v.score != RelativeScore::Worse)
            .count();
        non_worse > votes.len() / 2
    };
    let small_or_no_change = matches!(semantic_delta, SemanticDelta::None | SemanticDelta::Small);
    let irreconcilable = detect_irreconcilable(objection_history, 0.85);

    let converged = majority_positive && small_or_no_change && consensus;

    let trend = if votes.is_empty() {
        Trend::Same
    } else {
        let better_count = votes
            .iter()
            .filter(|v| v.score == RelativeScore::Better)
            .count();
        let worse_count = votes
            .iter()
            .filter(|v| v.score == RelativeScore::Worse)
            .count();
        if better_count > worse_count {
            Trend::Better
        } else if worse_count > better_count {
            Trend::Worse
        } else {
            Trend::Same
        }
    };

    let agree_count = votes
        .iter()
        .filter(|v| v.score != RelativeScore::Worse)
        .count();
    let agreement = format!("{}/{} agree", agree_count, votes.len());

    SectionConvergence {
        name: name.to_string(),
        converged,
        trend,
        agreement,
        irreconcilable,
    }
}

/// Build a prompt for LLM-based semantic diff using the convergence_semantic.md template.
pub fn build_semantic_diff_prompt(
    section_name: &str,
    current: &str,
    previous: &str,
    round: u32,
    prev_round: u32,
) -> String {
    let mut vars = std::collections::HashMap::new();
    vars.insert("section_name".to_string(), section_name.to_string());
    vars.insert("current".to_string(), current.to_string());
    vars.insert("previous".to_string(), previous.to_string());
    vars.insert("round".to_string(), round.to_string());
    vars.insert("prev_round".to_string(), prev_round.to_string());

    match crate::template::load_prompt("convergence_semantic") {
        Ok(template) => crate::template::interpolate(&template, &vars),
        Err(_) => format!(
            "Did the meaning of section '{}' change?\n\nCurrent (Round {}):\n{}\n\nPrevious (Round {}):\n{}\n\nRespond: none, small, or large.",
            section_name, round, current, prev_round, previous
        ),
    }
}
