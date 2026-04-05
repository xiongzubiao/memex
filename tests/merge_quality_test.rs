use brainstormer::tools::merge_quality::*;

#[test]
fn t19_quality_pass() {
    let llm_response = "All critiques addressed. PASS";
    let result = parse_quality_verdict(llm_response);
    assert!(result.passed);
    assert!(result.dropped_critiques.is_empty());
}

#[test]
fn t20_quality_fail() {
    let llm_response = "FAIL\nDropped critiques:\n- Error handling section ignored\n- No pagination added";
    let result = parse_quality_verdict(llm_response);
    assert!(!result.passed);
    assert_eq!(result.dropped_critiques.len(), 2);
}

#[test]
fn t21_third_failure_proceed_with_warning() {
    let attempt = 3;
    let max_attempts = 3;
    let should_proceed = should_proceed_despite_failure(attempt, max_attempts);
    assert!(should_proceed);
}
