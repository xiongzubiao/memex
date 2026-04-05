use brainstormer::cli::auto_detect::*;

#[test]
fn t38_auto_detect_three_providers() {
    let env = vec![
        ("ANTHROPIC_API_KEY".to_string(), "sk-ant-xxx".to_string()),
        ("OPENAI_API_KEY".to_string(), "sk-xxx".to_string()),
        ("GEMINI_API_KEY".to_string(), "AIza-xxx".to_string()),
    ];
    let result = detect_providers_from_env(&env);
    assert_eq!(result.len(), 3);
    assert!(result.iter().any(|p| p.provider == "anthropic"));
    assert!(result.iter().any(|p| p.provider == "openai"));
    assert!(result.iter().any(|p| p.provider == "google"));
}

#[test]
fn t39_auto_detect_one_provider() {
    let env = vec![
        ("ANTHROPIC_API_KEY".to_string(), "sk-ant-xxx".to_string()),
    ];
    let result = detect_providers_from_env(&env);
    assert_eq!(result.len(), 1);
}

#[test]
fn t40_auto_detect_invalid_key() {
    let env = vec![
        ("ANTHROPIC_API_KEY".to_string(), "".to_string()),
        ("OPENAI_API_KEY".to_string(), "sk-xxx".to_string()),
    ];
    let result = detect_providers_from_env(&env);
    assert_eq!(result.len(), 1);
}
