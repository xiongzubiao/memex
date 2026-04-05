use brainstormer::tools::convergence::*;

#[test]
fn llm_semantic_diff_parses_none() {
    assert_eq!(parse_semantic_delta("none"), brainstormer::types::SemanticDelta::None);
    assert_eq!(parse_semantic_delta("None - no meaningful changes"), brainstormer::types::SemanticDelta::None);
}

#[test]
fn llm_semantic_diff_parses_small() {
    assert_eq!(parse_semantic_delta("small"), brainstormer::types::SemanticDelta::Small);
    assert_eq!(parse_semantic_delta("Small refinement to wording"), brainstormer::types::SemanticDelta::Small);
}

#[test]
fn llm_semantic_diff_parses_large() {
    assert_eq!(parse_semantic_delta("large"), brainstormer::types::SemanticDelta::Large);
    assert_eq!(parse_semantic_delta("Large - complete rewrite of approach"), brainstormer::types::SemanticDelta::Large);
}

#[test]
fn semantic_diff_prompt_contains_both_versions() {
    let prompt = build_semantic_diff_prompt("Architecture", "current version text", "previous version text", 2, 1);
    assert!(prompt.contains("Architecture"));
    assert!(prompt.contains("current version text"));
    assert!(prompt.contains("previous version text"));
    assert!(prompt.contains("2"));
    assert!(prompt.contains("1"));
}
