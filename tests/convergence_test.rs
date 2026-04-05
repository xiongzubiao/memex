use brainstormer::tools::convergence::*;
use brainstormer::types::*;

#[test]
fn t01_semantic_diff_detects_change() {
    let result = parse_semantic_delta("large");
    assert_eq!(result, SemanticDelta::Large);
}

#[test]
fn t02_semantic_diff_detects_no_change() {
    let result = parse_semantic_delta("none");
    assert_eq!(result, SemanticDelta::None);
}

#[test]
fn t03_relative_scoring_parse() {
    let llm_output = "Score: better\nThe architecture section now handles edge cases.";
    let result = parse_relative_score(llm_output);
    assert_eq!(result, RelativeScore::Better);
}

#[test]
fn t04_relative_scoring_malformed_fallback() {
    let llm_output = "I think the section is kind of okay maybe?";
    let result = parse_relative_score(llm_output);
    assert_eq!(result, RelativeScore::Same);
}

#[test]
fn t05_consensus_all_agree() {
    let votes = vec![
        LlmVote { model_id: "opus".into(), score: RelativeScore::Better, comment: None },
        LlmVote { model_id: "gpt".into(), score: RelativeScore::Better, comment: None },
        LlmVote { model_id: "gemini".into(), score: RelativeScore::Same, comment: None },
    ];
    assert!(check_consensus(&votes));
}

#[test]
fn t06_consensus_one_disagrees() {
    let votes = vec![
        LlmVote { model_id: "opus".into(), score: RelativeScore::Better, comment: None },
        LlmVote { model_id: "gpt".into(), score: RelativeScore::Worse, comment: None },
        LlmVote { model_id: "gemini".into(), score: RelativeScore::Better, comment: None },
    ];
    assert!(!check_consensus(&votes));
}

#[test]
fn t07_convergence_guard_same_objection_twice() {
    let history = vec![
        "Security model is missing encryption at rest".to_string(),
        "The security model lacks encryption at rest".to_string(),
    ];
    assert!(detect_irreconcilable(&history, 0.85));
}

#[test]
fn t08_mixed_sections() {
    let sections = vec![
        SectionConvergence {
            name: "Problem".into(),
            converged: true,
            trend: Trend::Same,
            agreement: "3/3".into(),
            irreconcilable: false,
        },
        SectionConvergence {
            name: "Architecture".into(),
            converged: false,
            trend: Trend::Better,
            agreement: "2/3".into(),
            irreconcilable: false,
        },
    ];
    let result = build_convergence_result(&sections);
    assert!(!result.all_converged);
    assert!(result.should_loop);
}

#[test]
fn t09_all_sections_converge_first_check() {
    let sections = vec![
        SectionConvergence {
            name: "Problem".into(),
            converged: true,
            trend: Trend::Same,
            agreement: "3/3".into(),
            irreconcilable: false,
        },
        SectionConvergence {
            name: "API".into(),
            converged: true,
            trend: Trend::Better,
            agreement: "3/3".into(),
            irreconcilable: false,
        },
    ];
    let result = build_convergence_result(&sections);
    assert!(result.all_converged);
    assert!(!result.should_loop);
}
