//! Cost tracking provider wrapper.
//!
//! Wraps a zeroclaw `Provider` to intercept LLM calls and record token usage.
//! The CLI reads accumulated stats after a session to display cost summaries.
//!
//! ## Limitation: orchestrator-only tracking
//!
//! Currently only tracks the orchestrator's `chat()` calls. Sub-agent calls
//! (swarm proposers/reviewers, delegates) are NOT tracked because zeroclaw's
//! `SwarmTool` and `DelegateTool` create their own providers internally and
//! call `chat_with_system()` which returns `String` (no usage metadata).
//!
//! TODO: Fix in zeroclaw fork — change `SwarmTool::call_agent()` and
//! `DelegateTool` to use `provider.chat()` (returns `ChatResponse` with
//! `usage: Option<TokenUsage>`), and add a provider wrapper hook so callers
//! can inject `CostTrackingProvider` around sub-agent providers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zeroclaw::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities, ToolsPayload,
};
use zeroclaw::tools::ToolSpec;

/// Accumulated token usage from a session.
#[derive(Debug, Default)]
pub struct CostStats {
    input_tokens: AtomicU64,
    output_tokens: AtomicU64,
    call_count: AtomicU64,
}

impl CostStats {
    pub fn input_tokens(&self) -> u64 {
        self.input_tokens.load(Ordering::Relaxed)
    }

    pub fn output_tokens(&self) -> u64 {
        self.output_tokens.load(Ordering::Relaxed)
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens() + self.output_tokens()
    }

    pub fn call_count(&self) -> u64 {
        self.call_count.load(Ordering::Relaxed)
    }

    fn record(&self, response: &ChatResponse) {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        if let Some(ref usage) = response.usage {
            if let Some(input) = usage.input_tokens {
                self.input_tokens.fetch_add(input, Ordering::Relaxed);
            }
            if let Some(output) = usage.output_tokens {
                self.output_tokens.fetch_add(output, Ordering::Relaxed);
            }
        }
    }
}

/// Provider wrapper that records token usage from every LLM call.
pub struct CostTrackingProvider {
    inner: Box<dyn Provider>,
    stats: Arc<CostStats>,
}

impl CostTrackingProvider {
    pub fn new(inner: Box<dyn Provider>, stats: Arc<CostStats>) -> Self {
        Self { inner, stats }
    }
}

#[async_trait::async_trait]
impl Provider for CostTrackingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    fn convert_tools(&self, tools: &[ToolSpec]) -> ToolsPayload {
        self.inner.convert_tools(tools)
    }

    async fn chat_with_system(
        &self,
        system: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.inner
            .chat_with_system(system, message, model, temperature)
            .await
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let response = self.inner.chat(request, model, temperature).await?;
        self.stats.record(&response);
        Ok(response)
    }

    fn supports_native_tools(&self) -> bool {
        self.inner.supports_native_tools()
    }

    fn supports_vision(&self) -> bool {
        self.inner.supports_vision()
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        self.inner.warmup().await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let response = self
            .inner
            .chat_with_tools(messages, tools, model, temperature)
            .await?;
        self.stats.record(&response);
        Ok(response)
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.inner
            .chat_with_history(messages, model, temperature)
            .await
    }
}

/// Format a cost summary line for terminal display.
pub fn format_cost_summary(stats: &CostStats) -> String {
    let calls = stats.call_count();
    let input = stats.input_tokens();
    let output = stats.output_tokens();
    if calls == 0 {
        return "No LLM calls made.".to_string();
    }
    format!(
        "{calls} LLM call{}, {:.1}K input tokens, {:.1}K output tokens",
        if calls == 1 { "" } else { "s" },
        input as f64 / 1000.0,
        output as f64 / 1000.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_stats_default_is_zero() {
        let stats = CostStats::default();
        assert_eq!(stats.input_tokens(), 0);
        assert_eq!(stats.output_tokens(), 0);
        assert_eq!(stats.call_count(), 0);
        assert_eq!(stats.total_tokens(), 0);
    }

    #[test]
    fn cost_stats_records_usage() {
        let stats = CostStats::default();
        let response = ChatResponse {
            text: Some("hello".to_string()),
            tool_calls: vec![],
            usage: Some(zeroclaw::providers::traits::TokenUsage {
                input_tokens: Some(100),
                output_tokens: Some(50),
                cached_input_tokens: None,
            }),
            reasoning_content: None,
        };
        stats.record(&response);
        assert_eq!(stats.input_tokens(), 100);
        assert_eq!(stats.output_tokens(), 50);
        assert_eq!(stats.call_count(), 1);
        assert_eq!(stats.total_tokens(), 150);
    }

    #[test]
    fn cost_stats_accumulates() {
        let stats = CostStats::default();
        for _ in 0..3 {
            let response = ChatResponse {
                text: None,
                tool_calls: vec![],
                usage: Some(zeroclaw::providers::traits::TokenUsage {
                    input_tokens: Some(100),
                    output_tokens: Some(50),
                    cached_input_tokens: None,
                }),
                reasoning_content: None,
            };
            stats.record(&response);
        }
        assert_eq!(stats.input_tokens(), 300);
        assert_eq!(stats.output_tokens(), 150);
        assert_eq!(stats.call_count(), 3);
    }

    #[test]
    fn format_summary_zero_calls() {
        let stats = CostStats::default();
        assert_eq!(format_cost_summary(&stats), "No LLM calls made.");
    }

    #[test]
    fn format_summary_with_calls() {
        let stats = CostStats::default();
        stats.input_tokens.store(45200, Ordering::Relaxed);
        stats.output_tokens.store(12800, Ordering::Relaxed);
        stats.call_count.store(12, Ordering::Relaxed);
        let summary = format_cost_summary(&stats);
        assert!(summary.contains("12 LLM calls"));
        assert!(summary.contains("45.2K input"));
        assert!(summary.contains("12.8K output"));
    }

    #[test]
    fn format_summary_single_call() {
        let stats = CostStats::default();
        stats.input_tokens.store(1000, Ordering::Relaxed);
        stats.output_tokens.store(500, Ordering::Relaxed);
        stats.call_count.store(1, Ordering::Relaxed);
        let summary = format_cost_summary(&stats);
        assert!(summary.contains("1 LLM call,"));
        assert!(!summary.contains("calls"));
    }

    #[test]
    fn cost_stats_handles_missing_usage() {
        let stats = CostStats::default();
        let response = ChatResponse {
            text: Some("hello".to_string()),
            tool_calls: vec![],
            usage: None,
            reasoning_content: None,
        };
        stats.record(&response);
        assert_eq!(stats.call_count(), 1);
        assert_eq!(stats.input_tokens(), 0);
        assert_eq!(stats.output_tokens(), 0);
    }

    /// Mock provider that returns ChatResponse with token usage populated.
    struct UsageTrackingMockProvider {
        input_tokens: u64,
        output_tokens: u64,
    }

    #[async_trait::async_trait]
    impl Provider for UsageTrackingMockProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("mock response".to_string())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some("mock response".to_string()),
                tool_calls: vec![],
                usage: Some(zeroclaw::providers::traits::TokenUsage {
                    input_tokens: Some(self.input_tokens),
                    output_tokens: Some(self.output_tokens),
                    cached_input_tokens: None,
                }),
                reasoning_content: None,
            })
        }
    }

    #[tokio::test]
    async fn tracking_provider_records_chat_usage() {
        let stats = Arc::new(CostStats::default());
        let mock: Box<dyn Provider> = Box::new(UsageTrackingMockProvider {
            input_tokens: 500,
            output_tokens: 200,
        });
        let provider = CostTrackingProvider::new(mock, Arc::clone(&stats));

        let request = ChatRequest {
            messages: &[ChatMessage::user("hello".to_string())],
            tools: None,
        };
        let response = provider.chat(request, "test-model", 0.7).await.unwrap();
        assert_eq!(response.text.unwrap(), "mock response");

        assert_eq!(stats.call_count(), 1);
        assert_eq!(stats.input_tokens(), 500);
        assert_eq!(stats.output_tokens(), 200);
    }

    #[tokio::test]
    async fn tracking_provider_accumulates_across_calls() {
        let stats = Arc::new(CostStats::default());
        let mock: Box<dyn Provider> = Box::new(UsageTrackingMockProvider {
            input_tokens: 100,
            output_tokens: 50,
        });
        let provider = CostTrackingProvider::new(mock, Arc::clone(&stats));

        for _ in 0..5 {
            let request = ChatRequest {
                messages: &[ChatMessage::user("hi".to_string())],
                tools: None,
            };
            provider.chat(request, "test-model", 0.7).await.unwrap();
        }

        assert_eq!(stats.call_count(), 5);
        assert_eq!(stats.input_tokens(), 500);
        assert_eq!(stats.output_tokens(), 250);
    }

    #[tokio::test]
    async fn tracking_provider_delegates_chat_with_system() {
        let stats = Arc::new(CostStats::default());
        let mock: Box<dyn Provider> = Box::new(UsageTrackingMockProvider {
            input_tokens: 100,
            output_tokens: 50,
        });
        let provider = CostTrackingProvider::new(mock, Arc::clone(&stats));

        // chat_with_system returns String, no usage to track
        let result = provider
            .chat_with_system(None, "hello", "test-model", 0.7)
            .await
            .unwrap();
        assert_eq!(result, "mock response");
        // No stats recorded (chat_with_system doesn't return ChatResponse)
        assert_eq!(stats.call_count(), 0);
    }
}
