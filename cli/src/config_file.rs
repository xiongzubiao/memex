use crate::auto_detect::DetectedProvider;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub use memex_agent::builder::ModelRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Global,
    Ingest,
    Query,
    Lint,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperationSection {
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub ingest: OperationSection,
    #[serde(default)]
    pub query: OperationSection,
    #[serde(default)]
    pub lint: OperationSection,
    #[serde(default)]
    pub providers: toml::Table,
    #[serde(default)]
    pub brainstorm: toml::Table,
}

#[derive(Debug, Clone)]
pub struct MemexConfig {
    pub raw_content: String,
    stored: StoredConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDefaults {
    pub global_model: String,
    pub ingest_model: String,
    pub query_model: String,
    pub lint_model: String,
    pub proposer_model: String,
    pub reviewer_model: String,
}

impl ProviderDefaults {
    pub fn codex() -> Self {
        Self {
            global_model: "openai-codex/gpt-5.4".to_string(),
            ingest_model: "openai-codex/gpt-5.4-mini".to_string(),
            query_model: "openai-codex/gpt-5.4".to_string(),
            lint_model: "openai-codex/gpt-5.4-mini".to_string(),
            proposer_model: "openai-codex/gpt-5.4".to_string(),
            reviewer_model: "openai-codex/gpt-5.4-mini".to_string(),
        }
    }

    pub fn gemini() -> Self {
        Self {
            global_model: "gemini/gemini-3-flash-preview".to_string(),
            ingest_model: "gemini/gemini-3-flash-preview".to_string(),
            query_model: "gemini/gemini-3.1-pro-preview".to_string(),
            lint_model: "gemini/gemini-3-flash-preview".to_string(),
            proposer_model: "gemini/gemini-3.1-pro-preview".to_string(),
            reviewer_model: "gemini/gemini-3-flash-preview".to_string(),
        }
    }
}

impl StoredConfig {
    pub fn resolve_model(&self, kind: OperationKind) -> ModelRef {
        let selected = match kind {
            OperationKind::Global => self.model.as_deref(),
            OperationKind::Ingest => self.ingest.model.as_deref().or(self.model.as_deref()),
            OperationKind::Query => self.query.model.as_deref().or(self.model.as_deref()),
            OperationKind::Lint => self.lint.model.as_deref().or(self.model.as_deref()),
        }
        .unwrap_or("dry-run/default");

        ModelRef::parse(selected)
    }

    fn add_model_to_brainstorm_array(brainstorm: &mut toml::Table, key: &str, model: &str) {
        let provider_prefix = model.split('/').next().unwrap_or_default();
        let existing = brainstorm.get(key).and_then(|v| v.as_array());

        let has_provider = existing
            .map(|arr| {
                arr.iter().any(|v| {
                    v.as_str()
                        .map(|s| s.starts_with(&format!("{provider_prefix}/")))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);

        if !has_provider {
            let mut new_arr = existing.cloned().unwrap_or_default();
            new_arr.push(toml::Value::String(model.to_string()));
            brainstorm.insert(key.to_string(), toml::Value::Array(new_arr));
        }
    }

    pub fn apply_provider_defaults(&mut self, defaults: &ProviderDefaults, make_default: bool) {
        if make_default || self.model.is_none() {
            self.model = Some(defaults.global_model.clone());
        }
        if self.ingest.model.is_none() {
            self.ingest.model = Some(defaults.ingest_model.clone());
        }
        if self.query.model.is_none() {
            self.query.model = Some(defaults.query_model.clone());
        }
        if self.lint.model.is_none() {
            self.lint.model = Some(defaults.lint_model.clone());
        }

        // Set orchestrator if not already set
        if self
            .brainstorm
            .get("orchestrator")
            .and_then(|v| v.as_str())
            .is_none()
        {
            self.brainstorm.insert(
                "orchestrator".to_string(),
                toml::Value::String(defaults.global_model.clone()),
            );
        }

        // Add proposer/reviewer models to brainstorm if the provider isn't already present
        Self::add_model_to_brainstorm_array(
            &mut self.brainstorm,
            "proposer_models",
            &defaults.proposer_model,
        );
        Self::add_model_to_brainstorm_array(
            &mut self.brainstorm,
            "reviewer_models",
            &defaults.reviewer_model,
        );
    }
}

impl MemexConfig {
    pub fn from_raw_content(raw_content: String, stored: StoredConfig) -> Self {
        Self {
            raw_content,
            stored,
        }
    }

    pub fn resolve_model(&self, kind: OperationKind) -> ModelRef {
        self.stored.resolve_model(kind)
    }

    pub fn stored(&self) -> &StoredConfig {
        &self.stored
    }
}

pub fn load_config_str(raw: &str) -> anyhow::Result<StoredConfig> {
    toml::from_str(raw).map_err(|e| anyhow::anyhow!("Invalid config.toml: {e}"))
}

pub fn load_memex_config(root: &Path) -> anyhow::Result<MemexConfig> {
    let config_path = root.join("config.toml");
    if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        let stored = load_config_str(&raw)?;
        return Ok(MemexConfig::from_raw_content(raw, stored));
    }

    Ok(default_memex_config_from_providers(
        &crate::auto_detect::detect_providers(),
    ))
}

pub fn load_or_default_stored_config(root: &Path) -> anyhow::Result<StoredConfig> {
    let config_path = root.join("config.toml");
    if !config_path.exists() {
        return Ok(StoredConfig::default());
    }

    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    load_config_str(&raw)
}

pub fn write_stored_config(root: &Path, stored: &StoredConfig) -> anyhow::Result<()> {
    let config_path = root.join("config.toml");
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut rendered = toml::to_string_pretty(stored).context("Failed to serialize config.toml")?;
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    std::fs::write(&config_path, rendered)
        .with_context(|| format!("Failed to write {}", config_path.display()))
}

pub fn apply_provider_defaults_and_persist(
    root: &Path,
    defaults: &ProviderDefaults,
    make_default: bool,
) -> anyhow::Result<StoredConfig> {
    let mut stored = load_or_default_stored_config(root)?;
    stored.apply_provider_defaults(defaults, make_default);
    write_stored_config(root, &stored)?;
    Ok(stored)
}

pub fn default_memex_config_from_providers(providers: &[DetectedProvider]) -> MemexConfig {
    let mut stored = StoredConfig::default();
    if let Some(primary) = providers.first() {
        stored.model = Some(format!(
            "{}/{}",
            primary.midtier_model.provider, primary.midtier_model.model
        ));
    } else {
        stored.model = Some("dry-run/default".to_string());
    }
    MemexConfig::from_raw_content(String::new(), stored)
}

pub fn build_config_toml(providers: &[DetectedProvider]) -> String {
    if providers.is_empty() {
        return "model = \"dry-run/default\"\n".to_string();
    }

    let mut lines = Vec::new();
    let primary = &providers[0];

    let global_model = format!(
        "{}/{}",
        primary.midtier_model.provider, primary.midtier_model.model
    );
    lines.push("# Global default model (used when per-operation model is not set)".to_string());
    lines.push(format!("model = \"{global_model}\""));
    lines.push(String::new());

    lines.push("# Provider credentials".to_string());
    for provider in providers {
        lines.push(format!("[providers.{}]", provider.provider));
        lines.push(format!("api_key_env = \"{}\"", provider.env_var));
        lines.push(String::new());
    }

    lines.push("# Per-operation settings (all optional, fall back to global model)".to_string());

    let cheap_model = if providers.len() > 1 {
        let provider = &providers[providers.len() - 1];
        format!(
            "{}/{}",
            provider.midtier_model.provider, provider.midtier_model.model
        )
    } else {
        global_model.clone()
    };

    lines.push("[ingest]".to_string());
    if cheap_model != global_model {
        lines.push(format!("model = \"{cheap_model}\""));
    }
    lines.push(String::new());

    lines.push("[query]".to_string());
    lines.push(String::new());

    lines.push("[lint]".to_string());
    if cheap_model != global_model {
        lines.push(format!("model = \"{cheap_model}\""));
    }
    lines.push(String::new());

    lines.push("[brainstorm]".to_string());
    lines.push(format!("orchestrator = \"{global_model}\""));
    lines.push("cost_budget = 10.0".to_string());

    let proposer_models: Vec<String> = providers
        .iter()
        .map(|provider| {
            format!(
                "\"{}/{}\"",
                provider.frontier_model.provider, provider.frontier_model.model
            )
        })
        .collect();
    lines.push(format!(
        "proposer_models = [{}]",
        proposer_models.join(", ")
    ));

    let reviewer_models: Vec<String> = if providers.len() > 1 {
        providers[1..]
            .iter()
            .map(|provider| {
                format!(
                    "\"{}/{}\"",
                    provider.midtier_model.provider, provider.midtier_model.model
                )
            })
            .collect()
    } else {
        vec![format!(
            "\"{}/{}\"",
            primary.midtier_model.provider, primary.midtier_model.model
        )]
    };
    lines.push(format!(
        "reviewer_models = [{}]",
        reviewer_models.join(", ")
    ));
    lines.push(String::new());

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_covers_global_ingest_and_lint_branches() {
        let cfg = load_config_str(
            r#"
model = "openai-codex/gpt-5.4"

[ingest]
model = "gemini/gemini-3-flash-preview"

[query]
model = "gemini/gemini-3.1-pro-preview"

[lint]
model = "anthropic/claude-sonnet-4-6"
"#,
        )
        .unwrap();

        let global = cfg.resolve_model(OperationKind::Global);
        assert_eq!(global.provider, "openai-codex");
        assert_eq!(global.model, "gpt-5.4");

        let ingest = cfg.resolve_model(OperationKind::Ingest);
        assert_eq!(ingest.provider, "gemini");
        assert_eq!(ingest.model, "gemini-3-flash-preview");

        let lint = cfg.resolve_model(OperationKind::Lint);
        assert_eq!(lint.provider, "anthropic");
        assert_eq!(lint.model, "claude-sonnet-4-6");
    }

    #[test]
    fn resolve_query_model_prefers_section_override() {
        let cfg = load_config_str(
            r#"
model = "openai-codex/gpt-5.4"

[query]
model = "gemini/gemini-3.1-pro-preview"
"#,
        )
        .unwrap();

        let resolved = cfg.resolve_model(OperationKind::Query);
        assert_eq!(resolved.provider, "gemini");
        assert_eq!(resolved.model, "gemini-3.1-pro-preview");
    }

    #[test]
    fn apply_provider_defaults_only_fills_missing_values() {
        let mut cfg = load_config_str(
            r#"
model = "anthropic/claude-sonnet-4-6"

[query]
model = "anthropic/claude-opus-4-6"
"#,
        )
        .unwrap();

        cfg.apply_provider_defaults(&ProviderDefaults::gemini(), false);

        assert_eq!(cfg.model.as_deref(), Some("anthropic/claude-sonnet-4-6"));
        assert_eq!(
            cfg.query.model.as_deref(),
            Some("anthropic/claude-opus-4-6")
        );
        assert_eq!(
            cfg.ingest.model.as_deref(),
            Some("gemini/gemini-3-flash-preview")
        );
        assert_eq!(
            cfg.lint.model.as_deref(),
            Some("gemini/gemini-3-flash-preview")
        );
    }

    #[test]
    fn apply_provider_defaults_adds_gemini_to_brainstorm_models() {
        let mut cfg = load_config_str(
            r#"
model = "openai-codex/gpt-5.4"

[ingest]
model = "openai-codex/gpt-5.4-mini"

[query]
model = "openai-codex/gpt-5.4"

[lint]
model = "openai-codex/gpt-5.4-mini"

[brainstorm]
proposer_models = ["openai-codex/gpt-5.4"]
reviewer_models = ["openai-codex/gpt-5.4-mini"]
"#,
        )
        .unwrap();

        cfg.apply_provider_defaults(&ProviderDefaults::gemini(), false);

        // Existing models should not change
        assert_eq!(cfg.model.as_deref(), Some("openai-codex/gpt-5.4"));
        assert_eq!(
            cfg.ingest.model.as_deref(),
            Some("openai-codex/gpt-5.4-mini")
        );
        assert_eq!(cfg.query.model.as_deref(), Some("openai-codex/gpt-5.4"));
        assert_eq!(cfg.lint.model.as_deref(), Some("openai-codex/gpt-5.4-mini"));

        // Gemini should be added to proposer and reviewer
        let proposers = cfg
            .brainstorm
            .get("proposer_models")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(proposers.len(), 2);
        assert_eq!(proposers[1].as_str(), Some("gemini/gemini-3.1-pro-preview"));

        let reviewers = cfg
            .brainstorm
            .get("reviewer_models")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(reviewers.len(), 2);
        assert_eq!(reviewers[1].as_str(), Some("gemini/gemini-3-flash-preview"));
    }

    #[test]
    fn apply_provider_defaults_populates_empty_brainstorm() {
        let mut cfg = load_config_str(
            r#"
model = "openai-codex/gpt-5.4"

[ingest]
model = "openai-codex/gpt-5.4-mini"
"#,
        )
        .unwrap();

        cfg.apply_provider_defaults(&ProviderDefaults::gemini(), false);

        // Existing models unchanged
        assert_eq!(cfg.model.as_deref(), Some("openai-codex/gpt-5.4"));
        assert_eq!(
            cfg.ingest.model.as_deref(),
            Some("openai-codex/gpt-5.4-mini")
        );

        // Brainstorm fields created from scratch
        assert_eq!(
            cfg.brainstorm.get("orchestrator").and_then(|v| v.as_str()),
            Some("gemini/gemini-3-flash-preview")
        );

        let proposers = cfg
            .brainstorm
            .get("proposer_models")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(proposers.len(), 1);
        assert_eq!(proposers[0].as_str(), Some("gemini/gemini-3.1-pro-preview"));

        let reviewers = cfg
            .brainstorm
            .get("reviewer_models")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(reviewers.len(), 1);
        assert_eq!(reviewers[0].as_str(), Some("gemini/gemini-3-flash-preview"));
    }

    #[test]
    fn apply_provider_defaults_skips_brainstorm_if_provider_already_present() {
        let mut cfg = load_config_str(
            r#"
model = "openai-codex/gpt-5.4"

[brainstorm]
proposer_models = ["openai-codex/gpt-5.4", "gemini/gemini-3.1-pro-preview"]
reviewer_models = ["gemini/gemini-3-flash-preview"]
"#,
        )
        .unwrap();

        cfg.apply_provider_defaults(&ProviderDefaults::gemini(), false);

        let proposers = cfg
            .brainstorm
            .get("proposer_models")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(proposers.len(), 2); // not duplicated

        let reviewers = cfg
            .brainstorm
            .get("reviewer_models")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(reviewers.len(), 1); // not duplicated
    }

    #[test]
    fn default_config_fallback_is_deterministic_without_env() {
        let providers = vec![DetectedProvider {
            provider: "openai".to_string(),
            env_var: "OPENAI_API_KEY".to_string(),
            frontier_model: ModelRef {
                provider: "openai".to_string(),
                model: "gpt-5.4".to_string(),
            },
            midtier_model: ModelRef {
                provider: "openai".to_string(),
                model: "gpt-5.4-mini".to_string(),
            },
        }];

        let cfg = default_memex_config_from_providers(&providers);
        let resolved = cfg.resolve_model(OperationKind::Global);
        assert_eq!(resolved.provider, "openai");
        assert_eq!(resolved.model, "gpt-5.4-mini");

        let dry_run = default_memex_config_from_providers(&[]);
        let resolved = dry_run.resolve_model(OperationKind::Global);
        assert_eq!(resolved.provider, "dry-run");
        assert_eq!(resolved.model, "default");
    }

    #[test]
    fn build_config_toml_round_trips_with_brainstorm_and_providers() {
        let providers = vec![
            DetectedProvider {
                provider: "openai".to_string(),
                env_var: "OPENAI_API_KEY".to_string(),
                frontier_model: ModelRef {
                    provider: "openai".to_string(),
                    model: "gpt-5.4".to_string(),
                },
                midtier_model: ModelRef {
                    provider: "openai".to_string(),
                    model: "gpt-5.4-mini".to_string(),
                },
            },
            DetectedProvider {
                provider: "gemini".to_string(),
                env_var: "GEMINI_API_KEY".to_string(),
                frontier_model: ModelRef {
                    provider: "gemini".to_string(),
                    model: "gemini-3.1-pro-preview".to_string(),
                },
                midtier_model: ModelRef {
                    provider: "gemini".to_string(),
                    model: "gemini-3-flash-preview".to_string(),
                },
            },
        ];

        let rendered = build_config_toml(&providers);
        let stored = load_config_str(&rendered).unwrap();

        assert_eq!(stored.model.as_deref(), Some("openai/gpt-5.4-mini"));
        assert_eq!(
            stored.ingest.model.as_deref(),
            Some("gemini/gemini-3-flash-preview")
        );
        assert_eq!(
            stored.lint.model.as_deref(),
            Some("gemini/gemini-3-flash-preview")
        );
        assert!(stored.query.model.is_none());

        let brainstorm = stored.brainstorm;
        assert_eq!(
            brainstorm.get("orchestrator").and_then(|v| v.as_str()),
            Some("openai/gpt-5.4-mini")
        );
        assert_eq!(
            brainstorm.get("cost_budget").and_then(|v| v.as_float()),
            Some(10.0)
        );
        assert_eq!(
            brainstorm
                .get("proposer_models")
                .and_then(|v| v.as_array())
                .map(|v| v.len()),
            Some(2)
        );
        assert_eq!(
            brainstorm
                .get("reviewer_models")
                .and_then(|v| v.as_array())
                .map(|v| v.len()),
            Some(1)
        );

        assert_eq!(
            stored
                .providers
                .get("openai")
                .and_then(|v| v.as_table())
                .and_then(|t| t.get("api_key_env"))
                .and_then(|v| v.as_str()),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            stored
                .providers
                .get("gemini")
                .and_then(|v| v.as_table())
                .and_then(|t| t.get("api_key_env"))
                .and_then(|v| v.as_str()),
            Some("GEMINI_API_KEY")
        );
    }
}
