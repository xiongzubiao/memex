use crate::agent_memory::MemexMemory;
use crate::config::BrainstormConfig;
use anyhow::{Context, Result};
use memex_core::Memex;
use std::collections::HashMap;
use std::sync::Arc;
use zeroclaw::agent::{Agent, AgentBuilder as ZcAgentBuilder};
use zeroclaw::config::{DelegateAgentConfig, SwarmConfig, SwarmStrategy};
use zeroclaw::observability::NoopObserver;
use zeroclaw::providers::{Provider, ProviderRuntimeOptions};
use zeroclaw::tools::{DelegateTool, SwarmTool};

/// Default temperature for proposer/reviewer sub-agents.
const DEFAULT_SUB_AGENT_TEMPERATURE: f64 = 0.7;

/// Default timeout (seconds) for each swarm invocation.
const DEFAULT_SWARM_TIMEOUT_SECS: u64 = 120;

/// Proposer system prompt loaded at compile time.
const PROPOSER_SYSTEM_PROMPT: &str = include_str!("../prompts/proposer.md");

/// Reviewer system prompt loaded at compile time.
const REVIEWER_SYSTEM_PROMPT: &str = include_str!("../prompts/reviewer.md");

/// Parse a model string of the form "provider/model" into `(provider, model)`.
///
/// If the string contains no `/`, the entire string is treated as the model name
/// and `"openrouter"` is used as the provider (safe default for most hosted models).
///
/// Reference to a specific model on a specific provider (e.g. "openai/gpt-4o").
/// Parses the "provider/model" format used in memex config into separate fields
/// matching zeroclaw's DelegateAgentConfig.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    /// Parse "provider/model" string. Defaults to "openrouter" if no slash.
    pub fn parse(s: &str) -> Self {
        match s.find('/') {
            Some(idx) => Self {
                provider: s[..idx].to_string(),
                model: s[idx + 1..].to_string(),
            },
            None => Self {
                provider: "openrouter".to_string(),
                model: s.to_string(),
            },
        }
    }
}

/// Builder that wires a zeroclaw `Agent` for the memex brainstorming pipeline.
///
/// The produced agent gets:
/// - `MemexMemory` backed by the given `Memex` instance.
/// - `SwarmTool` with two swarms:
///   - `"proposers"` — parallel fan-out to every model in `config.proposers.models`.
///   - `"reviewers"` — parallel fan-out to every model in `config.reviewers.models`.
/// - `DelegateTool` for direct single-agent delegation.
/// - The workspace directory set to the memex root (so zeroclaw loads personality files).
/// - **No** `file_write` tool (security: the agent must not overwrite personality files).
pub struct MemexAgentBuilder {
    memex: Arc<Memex>,
    config: BrainstormConfig,
    /// Provider for the orchestrator (the top-level agent turn).
    orchestrator_provider: Box<dyn Provider>,
    /// Runtime auth/config options used when creating providers for sub-agents.
    provider_runtime_options: ProviderRuntimeOptions,
    /// When true, wrap tools with progress reporting to stderr.
    progress: bool,
}

impl MemexAgentBuilder {
    /// Create a builder. `orchestrator_provider` drives the orchestrator's LLM calls.
    pub fn new(
        memex: Arc<Memex>,
        config: BrainstormConfig,
        orchestrator_provider: Box<dyn Provider>,
    ) -> Self {
        Self {
            memex,
            config,
            orchestrator_provider,
            provider_runtime_options: ProviderRuntimeOptions::default(),
            progress: false,
        }
    }

    /// Configure runtime options used by swarm/delegate sub-agents.
    pub fn provider_runtime_options(mut self, options: ProviderRuntimeOptions) -> Self {
        self.provider_runtime_options = options;
        self
    }

    /// Enable progress reporting to stderr.
    pub fn progress(mut self, enabled: bool) -> Self {
        self.progress = enabled;
        self
    }

    /// Build the zeroclaw `Agent`.
    pub fn build(self) -> Result<Agent> {
        let workspace_dir = self.memex.root().to_path_buf();

        // --- Memory ---
        let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(MemexMemory::with_threshold(
            Arc::clone(&self.memex),
            self.config.small_memex_threshold,
        ));

        // --- Build agent registry and swarms ---
        let (agents, swarms) = self.build_swarm_configs();

        // --- Tools: swarm + delegate + common set ---
        let security: Arc<_> = Arc::default();
        let mut tools: Vec<Box<dyn zeroclaw::tools::Tool>> = vec![
            Box::new(SwarmTool::new(
                swarms,
                agents.clone(),
                None,
                Arc::clone(&security),
                self.provider_runtime_options.clone(),
            )),
            Box::new(DelegateTool::new_with_options(
                agents,
                None,
                Arc::clone(&security),
                self.provider_runtime_options.clone(),
            )),
        ];
        tools.extend(crate::tools::build_common_tools(&memory));

        if self.progress {
            tools = tools
                .into_iter()
                .map(crate::progress::ProgressTool::wrap)
                .collect();
        }

        // --- Observer ---
        let observer: Arc<dyn zeroclaw::observability::traits::Observer> = Arc::new(NoopObserver);

        // --- Assemble agent ---
        let model_name = ModelRef::parse(&self.config.orchestrator).model;

        ZcAgentBuilder::new()
            .provider(self.orchestrator_provider)
            .tools(tools)
            .memory(memory)
            .observer(observer)
            .tool_dispatcher(Box::new(zeroclaw::agent::dispatcher::NativeToolDispatcher))
            .model_name(model_name)
            .workspace_dir(workspace_dir)
            .build()
            .context("failed to build zeroclaw Agent")
    }

    /// Build the `DelegateAgentConfig` map and `SwarmConfig` map from `self.config`.
    fn build_swarm_configs(
        &self,
    ) -> (
        HashMap<String, DelegateAgentConfig>,
        HashMap<String, SwarmConfig>,
    ) {
        let mut agents: HashMap<String, DelegateAgentConfig> = HashMap::new();

        let proposer_agent_names = register_role_agents(
            &mut agents,
            &self.config.proposer_models,
            "proposer",
            PROPOSER_SYSTEM_PROMPT,
        );
        let reviewer_agent_names = register_role_agents(
            &mut agents,
            &self.config.reviewer_models,
            "reviewer",
            REVIEWER_SYSTEM_PROMPT,
        );

        // Proposers swarm (parallel — all propose simultaneously)
        let proposers_swarm = SwarmConfig {
            agents: proposer_agent_names,
            strategy: SwarmStrategy::Parallel,
            router_prompt: None,
            description: Some(
                "Fan-out brainstorming proposals across all configured proposer models."
                    .to_string(),
            ),
            timeout_secs: DEFAULT_SWARM_TIMEOUT_SECS,
        };

        // Reviewers swarm (parallel — all review simultaneously)
        let reviewers_swarm = SwarmConfig {
            agents: reviewer_agent_names,
            strategy: SwarmStrategy::Parallel,
            router_prompt: None,
            description: Some(
                "Fan-out review passes across all configured reviewer models.".to_string(),
            ),
            timeout_secs: DEFAULT_SWARM_TIMEOUT_SECS,
        };

        let mut swarms: HashMap<String, SwarmConfig> = HashMap::new();
        swarms.insert("proposers".to_string(), proposers_swarm);
        swarms.insert("reviewers".to_string(), reviewers_swarm);

        (agents, swarms)
    }
}

fn register_role_agents(
    agents: &mut HashMap<String, DelegateAgentConfig>,
    models: &[String],
    role: &str,
    system_prompt: &str,
) -> Vec<String> {
    let mut names = Vec::new();
    for (idx, model_str) in models.iter().enumerate() {
        let mref = ModelRef::parse(model_str);
        let name = format!("{role}_{idx}");
        agents.insert(
            name.clone(),
            DelegateAgentConfig {
                provider: mref.provider.clone(),
                model: mref.model.clone(),
                system_prompt: Some(system_prompt.to_string()),
                api_key: None,
                temperature: Some(DEFAULT_SUB_AGENT_TEMPERATURE),
                max_depth: 1,
                agentic: false,
                allowed_tools: Vec::new(),
                max_iterations: 5,
                timeout_secs: Some(DEFAULT_SWARM_TIMEOUT_SECS),
                agentic_timeout_secs: None,
                skills_directory: None,
                memory_namespace: Some(role.to_string()),
            },
        );
        names.push(name);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrainstormConfig;
    use crate::dry_run::DryRunProvider;
    use memex_core::Memex;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;
    use zeroclaw::providers::{Provider, ProviderRuntimeOptions};

    fn make_config() -> BrainstormConfig {
        BrainstormConfig {
            orchestrator: "dry-run/test-model".to_string(),
            proposer_models: vec![
                "openai/gpt-4o".to_string(),
                "anthropic/claude-sonnet-4-6".to_string(),
            ],
            reviewer_models: vec!["openai/gpt-4o-mini".to_string()],
            ..BrainstormConfig::default()
        }
    }

    fn make_memex(dir: &TempDir) -> Arc<Memex> {
        let root = dir.path().join("memex");
        Arc::new(Memex::open(root, Box::new(DryRunProvider), "test-model").unwrap())
    }

    #[test]
    fn model_ref_with_slash() {
        let r = ModelRef::parse("openai/gpt-4o");
        assert_eq!(r.provider, "openai");
        assert_eq!(r.model, "gpt-4o");
    }

    #[test]
    fn model_ref_without_slash() {
        let r = ModelRef::parse("gpt-4o");
        assert_eq!(r.provider, "openrouter");
        assert_eq!(r.model, "gpt-4o");
    }

    #[test]
    fn model_ref_multiple_slashes() {
        let r = ModelRef::parse("openrouter/some/nested/model");
        assert_eq!(r.provider, "openrouter");
        assert_eq!(r.model, "some/nested/model");
    }

    #[test]
    fn builder_build_does_not_panic() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let config = make_config();
        let provider = Box::new(DryRunProvider) as Box<dyn Provider>;

        let builder = MemexAgentBuilder::new(memex, config, provider);
        let result = builder.build();
        assert!(
            result.is_ok(),
            "build() should succeed: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[test]
    fn builder_uses_supplied_provider_runtime_options() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let config = make_config();
        let provider = Box::new(DryRunProvider) as Box<dyn Provider>;

        let builder = MemexAgentBuilder::new(memex, config, provider).provider_runtime_options(
            ProviderRuntimeOptions {
                zeroclaw_dir: Some(PathBuf::from("/tmp/memex-auth")),
                secrets_encrypt: true,
                ..Default::default()
            },
        );

        let result = builder.build();
        assert!(
            result.is_ok(),
            "build() with custom ProviderRuntimeOptions should succeed: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[test]
    fn build_swarm_configs_matches_model_counts() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let config = make_config();
        let provider = Box::new(DryRunProvider) as Box<dyn Provider>;

        let builder = MemexAgentBuilder::new(Arc::clone(&memex), config.clone(), provider);
        let (agents, swarms) = builder.build_swarm_configs();

        // 2 proposers + 1 reviewer = 3 agents total
        assert_eq!(agents.len(), 3);

        let proposers = swarms.get("proposers").expect("proposers swarm missing");
        assert_eq!(proposers.agents.len(), config.proposer_models.len());
        assert_eq!(proposers.strategy, SwarmStrategy::Parallel);

        let reviewers = swarms.get("reviewers").expect("reviewers swarm missing");
        assert_eq!(reviewers.agents.len(), config.reviewer_models.len());
        assert_eq!(reviewers.strategy, SwarmStrategy::Parallel);
    }

    #[test]
    fn build_with_empty_panelists_does_not_panic() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let config = BrainstormConfig::default(); // empty proposer_models/reviewer_models
        let provider = Box::new(DryRunProvider) as Box<dyn Provider>;

        let builder = MemexAgentBuilder::new(memex, config, provider);
        let result = builder.build();
        assert!(
            result.is_ok(),
            "build() with empty panelists should succeed: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[test]
    fn proposer_system_prompt_is_non_empty() {
        assert!(
            !PROPOSER_SYSTEM_PROMPT.trim().is_empty(),
            "proposer system prompt must not be empty"
        );
    }

    #[test]
    fn reviewer_system_prompt_is_non_empty() {
        assert!(
            !REVIEWER_SYSTEM_PROMPT.trim().is_empty(),
            "reviewer system prompt must not be empty"
        );
    }
}
