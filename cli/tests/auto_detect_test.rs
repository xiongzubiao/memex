use memex_cli::auto_detect::*;
use memex_core::model_catalog::{DEFAULT_MODEL_INFO, lookup_model};

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
    assert!(result.iter().any(|p| p.provider == "gemini"));
}

#[test]
fn t39_auto_detect_one_provider() {
    let env = vec![("ANTHROPIC_API_KEY".to_string(), "sk-ant-xxx".to_string())];
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

#[test]
fn t41_auto_detect_gemini_uses_catalog_backed_models() {
    let env = vec![("GEMINI_API_KEY".to_string(), "AIza-xxx".to_string())];
    let result = detect_providers_from_env(&env);
    let gemini = result
        .iter()
        .find(|p| p.provider == "gemini")
        .expect("expected gemini provider");

    assert_eq!(gemini.frontier_model.model, "gemini-3.1-pro-preview");
    assert_eq!(gemini.midtier_model.model, "gemini-3-flash-preview");

    assert_ne!(
        lookup_model(&gemini.frontier_model.model),
        DEFAULT_MODEL_INFO,
        "frontier model should be present in the vendored catalog"
    );
    assert_ne!(
        lookup_model(&gemini.midtier_model.model),
        DEFAULT_MODEL_INFO,
        "midtier model should be present in the vendored catalog"
    );
}
