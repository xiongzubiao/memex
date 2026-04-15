//! Shared tool and security construction used by both copilot and brainstorm agent builders.

use std::sync::Arc;
use zeroclaw::providers::Provider;
use zeroclaw::tools::{
    AskUserTool, FileReadTool, MemoryRecallTool, MemoryStoreTool, PdfReadTool, Tool, WebFetchTool,
    WebSearchTool,
};

/// Thin adapter: wraps `Arc<dyn Provider>` as `Box<dyn Provider>` for zeroclaw's `AgentBuilder`.
pub struct ArcProvider(pub Arc<dyn Provider>);

#[async_trait::async_trait]
impl Provider for ArcProvider {
    fn capabilities(&self) -> zeroclaw::providers::traits::ProviderCapabilities {
        self.0.capabilities()
    }
    fn convert_tools(
        &self,
        tools: &[zeroclaw::tools::ToolSpec],
    ) -> zeroclaw::providers::traits::ToolsPayload {
        self.0.convert_tools(tools)
    }
    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.0
            .chat_with_system(system, message, model, temperature)
            .await
    }
    async fn chat(
        &self,
        request: zeroclaw::providers::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<zeroclaw::providers::traits::ChatResponse> {
        self.0.chat(request, model, temperature).await
    }
    fn supports_native_tools(&self) -> bool {
        self.0.supports_native_tools()
    }
    fn supports_vision(&self) -> bool {
        self.0.supports_vision()
    }
    async fn warmup(&self) -> anyhow::Result<()> {
        self.0.warmup().await
    }
    async fn chat_with_tools(
        &self,
        messages: &[zeroclaw::providers::traits::ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<zeroclaw::providers::traits::ChatResponse> {
        self.0
            .chat_with_tools(messages, tools, model, temperature)
            .await
    }
    async fn chat_with_history(
        &self,
        messages: &[zeroclaw::providers::traits::ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.0.chat_with_history(messages, model, temperature).await
    }
}

/// Adapter: wraps a zeroclaw `Provider` as a `memex_core::LlmProvider`.
///
/// Used by CLI to pass a zeroclaw provider to `Memex::open`.
pub struct ProviderLlmAdapter(pub Box<dyn Provider>);

#[async_trait::async_trait]
impl memex_core::LlmProvider for ProviderLlmAdapter {
    async fn chat(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.0
            .chat_with_system(system, message, model, temperature)
            .await
    }
}

/// Maximum response size for web_fetch (bytes).
const WEB_FETCH_MAX_SIZE: usize = 500_000;

/// Timeout for web_fetch requests (seconds).
const WEB_FETCH_TIMEOUT_SECS: u64 = 30;

/// Number of results for web_search.
const WEB_SEARCH_RESULTS: usize = 5;

/// Timeout for web_search requests (seconds).
const WEB_SEARCH_TIMEOUT_SECS: u64 = 30;

/// Build the common tool set shared by copilot and brainstorm agents.
///
/// Uses `SecurityPolicy::default()` (workspace_only: true, workspace_dir: ".").
/// The CLI should `chdir` to the appropriate directory before building agents
/// so that `file_read` can access source files.
///
/// Returns: file_read, memory_store, memory_recall, web_fetch, web_search, ask_user, pdf_read.
pub fn build_common_tools(memory: &Arc<dyn zeroclaw::memory::Memory>) -> Vec<Box<dyn Tool>> {
    // Arc::default() infers Arc<SecurityPolicy> from FileReadTool::new's signature.
    // This avoids naming zeroclaw::security::SecurityPolicy directly (pub(crate) module).
    let security: Arc<_> = Arc::default();
    vec![
        Box::new(FileReadTool::new(Arc::clone(&security))),
        Box::new(MemoryStoreTool::new(
            Arc::clone(memory),
            Arc::clone(&security),
        )),
        Box::new(MemoryRecallTool::new(Arc::clone(memory))),
        Box::new(WebFetchTool::new(
            Arc::clone(&security),
            vec![],
            vec![],
            WEB_FETCH_MAX_SIZE,
            WEB_FETCH_TIMEOUT_SECS,
            Default::default(),
            vec![],
        )),
        Box::new(WebSearchTool::new(
            "duckduckgo".into(),
            None,
            WEB_SEARCH_RESULTS,
            WEB_SEARCH_TIMEOUT_SECS,
        )),
        Box::new(AskUserTool::new(Arc::clone(&security))),
        Box::new(PdfReadTool::new(Arc::clone(&security))),
    ]
}
