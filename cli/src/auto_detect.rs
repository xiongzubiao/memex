pub use memex_agent::builder::ModelRef;

/// Detected provider from environment.
#[derive(Debug, Clone)]
pub struct DetectedProvider {
    pub provider: String,
    pub env_var: String,
    pub frontier_model: ModelRef,
    pub midtier_model: ModelRef,
}

struct ProviderSpec {
    env_var: &'static str,
    provider: &'static str,
    frontier_model: &'static str,
    midtier_model: &'static str,
}

const PROVIDER_ENV_VARS: &[ProviderSpec] = &[
    ProviderSpec {
        env_var: "ANTHROPIC_API_KEY",
        provider: "anthropic",
        frontier_model: "claude-opus-4-6",
        midtier_model: "claude-sonnet-4-6",
    },
    ProviderSpec {
        env_var: "OPENAI_API_KEY",
        provider: "openai",
        frontier_model: "gpt-5.4",
        midtier_model: "gpt-5.4-mini",
    },
    ProviderSpec {
        env_var: "GEMINI_API_KEY",
        provider: "gemini",
        frontier_model: "gemini-3.1-pro-preview",
        midtier_model: "gemini-3-flash-preview",
    },
];

/// Detect available providers from environment variables.
pub fn detect_providers_from_env(env_vars: &[(String, String)]) -> Vec<DetectedProvider> {
    let env_map: std::collections::HashMap<&str, &str> = env_vars
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    PROVIDER_ENV_VARS
        .iter()
        .filter_map(|spec| match env_map.get(spec.env_var) {
            Some(key) if !key.is_empty() => Some(DetectedProvider {
                provider: spec.provider.to_string(),
                env_var: spec.env_var.to_string(),
                frontier_model: ModelRef {
                    provider: spec.provider.to_string(),
                    model: spec.frontier_model.to_string(),
                },
                midtier_model: ModelRef {
                    provider: spec.provider.to_string(),
                    model: spec.midtier_model.to_string(),
                },
            }),
            _ => None,
        })
        .collect()
}

/// Check real environment variables (for production use).
pub fn detect_providers() -> Vec<DetectedProvider> {
    let env_vars: Vec<(String, String)> = PROVIDER_ENV_VARS
        .iter()
        .filter_map(|spec| {
            std::env::var(spec.env_var)
                .ok()
                .map(|val| (spec.env_var.to_string(), val))
        })
        .collect();
    detect_providers_from_env(&env_vars)
}
