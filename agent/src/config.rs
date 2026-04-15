use serde::{Deserialize, Serialize};

/// Brainstorm configuration from config.toml `[brainstorm]` section.
///
/// Matches the spec's config format:
/// ```toml
/// [brainstorm]
/// orchestrator = "anthropic/claude-sonnet-4-6"
/// cost_budget = 10.0
/// proposer_models = ["openai/gpt-5.4", "anthropic/claude-opus-4-6"]
/// reviewer_models = ["openai/o4-mini"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainstormConfig {
    #[serde(default = "default_orchestrator")]
    pub orchestrator: String,
    #[serde(default = "default_cost_budget")]
    pub cost_budget: f64,
    #[serde(default = "default_small_memex_threshold")]
    pub small_memex_threshold: usize,
    #[serde(default)]
    pub proposer_models: Vec<String>,
    #[serde(default)]
    pub reviewer_models: Vec<String>,
}

fn default_orchestrator() -> String {
    "dry-run/default".to_string()
}
fn default_cost_budget() -> f64 {
    5.0
}
fn default_small_memex_threshold() -> usize {
    15_000
}

impl Default for BrainstormConfig {
    fn default() -> Self {
        Self {
            orchestrator: default_orchestrator(),
            cost_budget: default_cost_budget(),
            small_memex_threshold: default_small_memex_threshold(),
            proposer_models: Vec::new(),
            reviewer_models: Vec::new(),
        }
    }
}

/// Parse the small_memex_threshold from config.toml (lives in `[brainstorm]` section).
///
/// This controls when MemexMemory switches from returning the full index
/// to using LLM pre-filtering. Used by all agent builders, not just brainstorm.
pub fn parse_small_memex_threshold(config_content: &str) -> usize {
    parse_brainstorm_config(config_content).small_memex_threshold
}

/// Parse brainstorm config from config.toml content.
/// Returns default if `[brainstorm]` section is missing.
pub fn parse_brainstorm_config(config_content: &str) -> BrainstormConfig {
    let val: toml::Value = match toml::from_str(config_content) {
        Ok(v) => v,
        Err(_) => return BrainstormConfig::default(),
    };
    if let Some(brainstorm_table) = val.get("brainstorm") {
        let brainstorm_str = toml::to_string(brainstorm_table).unwrap_or_default();
        toml::from_str(&brainstorm_str).unwrap_or_default()
    } else {
        BrainstormConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_config_returns_defaults() {
        let cfg = parse_brainstorm_config("");
        assert_eq!(cfg.orchestrator, "dry-run/default");
        assert_eq!(cfg.cost_budget, 5.0);
        assert_eq!(cfg.small_memex_threshold, 15_000);
        assert!(cfg.proposer_models.is_empty());
        assert!(cfg.reviewer_models.is_empty());
    }

    #[test]
    fn parse_config_with_brainstorm_section() {
        let toml_str = r#"
[brainstorm]
orchestrator = "anthropic/claude-sonnet-4-6"
cost_budget = 10.0
small_memex_threshold = 20000
proposer_models = ["openai/gpt-5.4", "anthropic/claude-opus-4-6"]
reviewer_models = ["openai/o4-mini"]
"#;
        let cfg = parse_brainstorm_config(toml_str);
        assert_eq!(cfg.orchestrator, "anthropic/claude-sonnet-4-6");
        assert_eq!(cfg.cost_budget, 10.0);
        assert_eq!(cfg.small_memex_threshold, 20_000);
        assert_eq!(
            cfg.proposer_models,
            vec!["openai/gpt-5.4", "anthropic/claude-opus-4-6"]
        );
        assert_eq!(cfg.reviewer_models, vec!["openai/o4-mini"]);
    }

    #[test]
    fn parse_config_without_brainstorm_section() {
        let toml_str = r#"
model = "anthropic/claude-sonnet-4-6"

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
"#;
        let cfg = parse_brainstorm_config(toml_str);
        assert_eq!(cfg.orchestrator, "dry-run/default");
        assert_eq!(cfg.cost_budget, 5.0);
    }
}
