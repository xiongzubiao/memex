use crate::types::ModelRef;

/// Detected provider from environment.
#[derive(Debug, Clone)]
pub struct DetectedProvider {
    pub provider: String,
    pub env_var: String,
    pub frontier_model: ModelRef,
    pub midtier_model: ModelRef,
}

const PROVIDER_ENV_VARS: &[(&str, &str, &str, &str, &str, &str)] = &[
    ("ANTHROPIC_API_KEY", "anthropic", "claude-opus-4-6", "claude-sonnet-4-6", "anthropic", "anthropic"),
    ("OPENAI_API_KEY", "openai", "gpt-5.4", "gpt-5.4-mini", "openai", "openai"),
    ("GEMINI_API_KEY", "google", "gemini-3.1-pro", "gemini-3.1-flash", "gemini", "gemini"),
];

/// Detect available providers from environment variables.
pub fn detect_providers_from_env(env_vars: &[(String, String)]) -> Vec<DetectedProvider> {
    let env_map: std::collections::HashMap<&str, &str> = env_vars
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    PROVIDER_ENV_VARS
        .iter()
        .filter_map(|(env_var, provider, frontier, midtier, f_prov, m_prov)| {
            match env_map.get(env_var) {
                Some(key) if !key.is_empty() => Some(DetectedProvider {
                    provider: provider.to_string(),
                    env_var: env_var.to_string(),
                    frontier_model: ModelRef {
                        provider: f_prov.to_string(),
                        model: frontier.to_string(),
                    },
                    midtier_model: ModelRef {
                        provider: m_prov.to_string(),
                        model: midtier.to_string(),
                    },
                }),
                _ => None,
            }
        })
        .collect()
}

/// Check real environment variables (for production use).
pub fn detect_providers() -> Vec<DetectedProvider> {
    let env_vars: Vec<(String, String)> = PROVIDER_ENV_VARS
        .iter()
        .filter_map(|(var, ..)| std::env::var(var).ok().map(|val| (var.to_string(), val)))
        .collect();
    detect_providers_from_env(&env_vars)
}
