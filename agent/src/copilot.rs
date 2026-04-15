//! Copilot agent builder for interactive ingest, query, and lint workflows.
//!
//! Produces a lightweight zeroclaw `Agent` with file_read + memory tools (no swarms/delegates).
//! The agent uses the memex workspace_dir for identity (AGENTS.md) and MemexMemory for wiki ops.

use crate::agent_memory::MemexMemory;
use anyhow::{Context, Result};
use memex_core::Memex;
use std::sync::Arc;
use zeroclaw::agent::{Agent, AgentBuilder};
use zeroclaw::observability::NoopObserver;
use zeroclaw::providers::Provider;

/// Which copilot workflow the agent will follow.
#[derive(Debug, Clone, Copy)]
pub enum CopilotMode {
    Ingest,
    Query,
    Lint,
    Brainstorm,
}

/// Prompt templates compiled into the binary.
const INGEST_PROMPT: &str = include_str!("../prompts/ingest.md");
const QUERY_PROMPT: &str = include_str!("../prompts/query.md");
const LINT_PROMPT: &str = include_str!("../prompts/lint.md");
const BRAINSTORM_PROMPT: &str = include_str!("../prompts/brainstorm.md");

/// Build a copilot agent for interactive ingest/query/lint.
///
/// The agent gets:
/// - `file_read` — read source files and wiki pages
/// - `memory_store` — write wiki pages via MemexMemory
/// - `memory_recall` — search wiki via MemexMemory
/// - Identity loaded from workspace_dir (AGENTS.md, IDENTITY.md, SOUL.md)
pub fn build_copilot_agent(
    memex: Arc<Memex>,
    provider: Box<dyn Provider>,
    model_name: &str,
    small_memex_threshold: usize,
) -> Result<Agent> {
    let memory: Arc<dyn zeroclaw::memory::Memory> = Arc::new(MemexMemory::with_threshold(
        Arc::clone(&memex),
        small_memex_threshold,
    ));
    let tools = crate::tools::build_common_tools(&memory);

    let observer: Arc<dyn zeroclaw::observability::traits::Observer> = Arc::new(NoopObserver);

    AgentBuilder::new()
        .provider(provider)
        .tools(tools)
        .memory(memory)
        .observer(observer)
        .tool_dispatcher(Box::new(zeroclaw::agent::dispatcher::NativeToolDispatcher))
        .model_name(model_name.to_string())
        .workspace_dir(memex.root().to_path_buf())
        .build()
        .context("failed to build copilot agent")
}

/// Format the initial prompt for a copilot turn.
///
/// Prepends the workflow instructions for the given mode to the user's actual request.
pub fn format_copilot_prompt(mode: CopilotMode, user_content: &str) -> String {
    let template = match mode {
        CopilotMode::Ingest => INGEST_PROMPT,
        CopilotMode::Query => QUERY_PROMPT,
        CopilotMode::Lint => LINT_PROMPT,
        CopilotMode::Brainstorm => BRAINSTORM_PROMPT,
    };
    format!("{template}\n\n{user_content}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dry_run::DryRunProvider;
    use memex_core::Memex;
    use tempfile::TempDir;
    use zeroclaw::providers::Provider;

    fn make_memex(dir: &TempDir) -> Arc<Memex> {
        let root = dir.path().join("memex");
        Arc::new(Memex::open(root, Box::new(DryRunProvider), "test-model").unwrap())
    }

    #[test]
    fn build_copilot_agent_does_not_panic() {
        let dir = TempDir::new().unwrap();
        let memex = make_memex(&dir);
        let provider = Box::new(DryRunProvider) as Box<dyn Provider>;
        let result = build_copilot_agent(memex, provider, "test-model", 15_000);
        assert!(
            result.is_ok(),
            "copilot agent build failed: {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[test]
    fn format_ingest_prompt_includes_sources() {
        let prompt = format_copilot_prompt(CopilotMode::Ingest, "Ingest: /tmp/file.md");
        assert!(prompt.contains("Ingest: /tmp/file.md"));
        assert!(prompt.contains("file_read"));
    }

    #[test]
    fn format_query_prompt_includes_question() {
        let prompt = format_copilot_prompt(CopilotMode::Query, "What is Rust?");
        assert!(prompt.contains("What is Rust?"));
        assert!(prompt.contains("memory_recall"));
    }

    #[test]
    fn format_lint_prompt_includes_instructions() {
        let prompt = format_copilot_prompt(CopilotMode::Lint, "Check my wiki");
        assert!(prompt.contains("Check my wiki"));
        assert!(prompt.contains("health"));
    }
}
