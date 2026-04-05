use brainstormer::tools::merge::*;

#[test]
fn t16_format_merge_input_multiple_outputs() {
    let outputs = vec![
        ("claude-opus".to_string(), "Use microservices with gRPC".to_string()),
        ("gpt-5".to_string(), "Monolith with clear module boundaries".to_string()),
    ];
    let formatted = format_outputs_for_merge(&outputs);
    assert!(formatted.contains("=== claude-opus ==="));
    assert!(formatted.contains("=== gpt-5 ==="));
    assert!(formatted.contains("microservices"));
    assert!(formatted.contains("Monolith"));
}

#[test]
fn t17_merge_model_fallback_selection() {
    use brainstormer::types::ModelRef;
    let primary = ModelRef { provider: "anthropic".into(), model: "claude-opus-4-6".into() };
    let fallbacks = vec![
        ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
        ModelRef { provider: "google".into(), model: "gemini-3.1-pro".into() },
    ];
    let selected = select_fallback_model(&primary, &fallbacks);
    assert!(selected.is_some());
    assert_ne!(selected.unwrap().model, primary.model);
}

#[test]
fn t18_format_merge_input_no_critiques() {
    let outputs = vec![
        ("model-a".to_string(), "Approach A".to_string()),
    ];
    let formatted = format_merge_prompt(&outputs, None, "Design a cache");
    assert!(formatted.contains("Approach A"));
    assert!(!formatted.contains("Critiques"));
}
