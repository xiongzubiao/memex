//! OpenAI API-backed worker.
//!
//! Executes one stateless Chat Completions call per job using `OPENAI_API_KEY`
//! and returns the assistant text in the shared `TurnOutcome` shape.

use super::{TaskKind, TurnOutcome, WORKER_PROMPT};
use crate::daemon::config::WorkerConfig;
use anyhow::{Context, Result, bail};
use async_openai::Client;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::chat::{
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs, CompletionUsage,
    CreateChatCompletionRequest, CreateChatCompletionRequestArgs, CreateChatCompletionResponse,
    FinishReason, ResponseFormat, ResponseFormatJsonSchema,
};
use serde_json::json;
use tokio::time::{Duration, sleep};

/// A lightweight stateless API client kept per worker task.
pub(super) struct OpenAiApiSubprocess {
    client: Client<OpenAIConfig>,
    model: String,
}

fn response_format_for_task(kind: TaskKind) -> ResponseFormat {
    match kind {
        TaskKind::Expand => ResponseFormat::JsonSchema {
            json_schema: ResponseFormatJsonSchema {
                name: "memex_expand_reply".to_string(),
                description: None,
                strict: Some(true),
                schema: Some(json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["lex", "vec", "hyde"],
                    "properties": {
                        "lex": { "type": "string" },
                        "vec": { "type": "string" },
                        "hyde": { "type": "string" }
                    }
                })),
            },
        },
        TaskKind::Synthesize => ResponseFormat::JsonSchema {
            json_schema: ResponseFormatJsonSchema {
                name: "memex_synth_reply".to_string(),
                description: None,
                strict: Some(true),
                schema: Some(json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["answer", "citations"],
                    "properties": {
                        "answer": { "type": "string" },
                        "citations": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    }
                })),
            },
        },
        TaskKind::Extract | TaskKind::Merge => ResponseFormat::JsonSchema {
            json_schema: ResponseFormatJsonSchema {
                name: "memex_pages_reply".to_string(),
                description: None,
                strict: Some(true),
                schema: Some(json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["pages"],
                    "properties": {
                        "pages": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["slug", "title", "tags", "body"],
                                "properties": {
                                    "slug": { "type": "string" },
                                    "title": { "type": "string" },
                                    "tags": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    },
                                    "body": { "type": "string" }
                                }
                            },
                        }
                    }
                })),
            },
        },
    }
}

fn is_transient_transport(e: &reqwest::Error) -> bool {
    if e.is_connect() || e.is_timeout() || e.is_request() {
        return true;
    }
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("broken pipe")
        || msg.contains("tls handshake eof")
        || msg.contains("connection reset")
        || msg.contains("temporarily unavailable")
}

fn is_transient_openai_error(e: &OpenAIError) -> bool {
    match e {
        OpenAIError::Reqwest(re) => is_transient_transport(re),
        OpenAIError::ApiError(api) => {
            let lc = format!(
                "{} {} {}",
                api.message,
                api.r#type.as_deref().unwrap_or(""),
                api.code.as_deref().unwrap_or("")
            )
            .to_ascii_lowercase();
            lc.contains("rate limit")
                || lc.contains("temporarily unavailable")
                || lc.contains("overloaded")
                || lc.contains("timeout")
                || lc.contains("server_error")
        }
        _ => false,
    }
}

fn finish_reason_str(reason: Option<FinishReason>) -> &'static str {
    match reason {
        Some(FinishReason::Stop) => "stop",
        Some(FinishReason::Length) => "length",
        Some(FinishReason::ToolCalls) => "tool_calls",
        Some(FinishReason::ContentFilter) => "content_filter",
        Some(FinishReason::FunctionCall) => "function_call",
        None => "unknown",
    }
}

fn usage_prompt_tokens(usage: Option<&CompletionUsage>) -> Option<u64> {
    usage.map(|u| u.prompt_tokens as u64)
}

fn usage_completion_tokens(usage: Option<&CompletionUsage>) -> Option<u64> {
    usage.map(|u| u.completion_tokens as u64)
}

fn usage_reasoning_tokens(usage: Option<&CompletionUsage>) -> Option<u64> {
    usage
        .and_then(|u| u.completion_tokens_details.as_ref())
        .and_then(|d| d.reasoning_tokens)
        .map(|n| n as u64)
}

fn usage_total_tokens(usage: Option<&CompletionUsage>) -> Option<u64> {
    usage.map(|u| u.total_tokens as u64)
}

fn build_request(
    model: &str,
    user_text: &str,
    task_kind: TaskKind,
) -> Result<CreateChatCompletionRequest> {
    let mut request = CreateChatCompletionRequestArgs::default();
    request.model(model);
    request.messages([
        ChatCompletionRequestSystemMessageArgs::default()
            .content(WORKER_PROMPT)
            .build()
            .context("building openai system message")?
            .into(),
        ChatCompletionRequestUserMessageArgs::default()
            .content(user_text)
            .build()
            .context("building openai user message")?
            .into(),
    ]);
    request.response_format(response_format_for_task(task_kind));
    // Bound reasoning effort (mirrors claude_code.rs rationale).
    request.reasoning_effort(async_openai::types::chat::ReasoningEffort::Low);
    request
        .build()
        .context("building openai chat completion request")
}

impl OpenAiApiSubprocess {
    pub(super) async fn spawn(cfg: &WorkerConfig) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .context("OPENAI_API_KEY is required for daemon.worker.backend=openai-api")?;
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = cfg
            .model
            .clone()
            .unwrap_or_else(|| cfg.backend.default_model().to_string());

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_sec))
            .build()
            .context("building reqwest client")?;
        let config = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(base_url);
        let client = Client::with_config(config).with_http_client(http_client);

        Ok(Self { client, model })
    }

    pub(super) async fn one_turn(
        &mut self,
        user_text: &str,
        task_kind: TaskKind,
    ) -> Result<TurnOutcome> {
        let mut last_err: Option<anyhow::Error> = None;
        let mut response_opt: Option<CreateChatCompletionResponse> = None;
        for attempt in 0..3u32 {
            let request = build_request(&self.model, user_text, task_kind)?;
            let send = self.client.chat().create(request).await;
            match send {
                Ok(response) => {
                    response_opt = Some(response);
                    break;
                }
                Err(e) => {
                    let transient = is_transient_openai_error(&e);
                    tracing::warn!(
                        backend = "openai-api",
                        model = %self.model,
                        ?task_kind,
                        attempt,
                        transient,
                        error = %e.to_string(),
                        "openai request failed"
                    );
                    match &e {
                        OpenAIError::ApiError(api) => {
                            // Prefer the structured error code from the API
                            // (e.g., `invalid_api_key`, `model_not_found`);
                            // fall back to the error type. Without this the
                            // full diagnostic is buried inside the message.
                            let code = api.code.clone().or_else(|| api.r#type.clone());
                            return Ok(TurnOutcome::BackendError {
                                message: e.to_string(),
                                code,
                            });
                        }
                        OpenAIError::InvalidArgument(_) => {
                            return Ok(TurnOutcome::backend_err(e.to_string(), "invalid_argument"));
                        }
                        OpenAIError::JSONDeserialize(_, _) => {
                            return Ok(TurnOutcome::backend_err(e.to_string(), "json_deserialize"));
                        }
                        _ => {
                            last_err = Some(
                                anyhow::anyhow!(e.to_string()).context("openai request failed"),
                            );
                        }
                    }
                    if transient && attempt < 2 {
                        let backoff_ms = 250u64 * (attempt as u64 + 1);
                        sleep(Duration::from_millis(backoff_ms)).await;
                        continue;
                    }
                    break;
                }
            }
        }
        let response = match response_opt {
            Some(r) => r,
            None => {
                if let Some(e) = last_err {
                    return Err(e.context(format!(
                        "backend=openai-api model={} task={task_kind:?}",
                        self.model
                    )));
                }
                bail!(
                    "openai request failed with unknown transport error (model={}, task={task_kind:?})",
                    self.model
                );
            }
        };

        let usage = response.usage.as_ref();
        let answer = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if answer.is_empty() {
            let refusal = response
                .choices
                .first()
                .and_then(|c| c.message.refusal.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            if let Some(r) = refusal {
                return Ok(TurnOutcome::BackendError {
                    message: r,
                    code: Some("refusal".into()),
                });
            }
            let finish_reason =
                finish_reason_str(response.choices.first().and_then(|c| c.finish_reason));
            return Ok(TurnOutcome::BackendError {
                message: format!(
                    "openai returned empty assistant message (model={}, task={task_kind:?}, prompt_tokens={:?}, completion_tokens={:?}, reasoning_tokens={:?}, total_tokens={:?})",
                    self.model,
                    usage_prompt_tokens(usage),
                    usage_completion_tokens(usage),
                    usage_reasoning_tokens(usage),
                    usage_total_tokens(usage)
                ),
                code: Some(format!("empty_response:{finish_reason}")),
            });
        }
        let input_tokens = match usage_prompt_tokens(usage) {
            Some(n) => n,
            None => {
                // Official OpenAI always sends usage; a missing block means
                // a proxy stripped it — warn so ops can investigate.
                tracing::warn!(
                    backend = "openai-api",
                    model = %self.model,
                    ?task_kind,
                    "openai response missing usage block; counting 0 input tokens"
                );
                0
            }
        };
        Ok(TurnOutcome::Ok {
            text: answer,
            input_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_format_is_defined_for_all_memex_tasks() {
        let f = response_format_for_task(TaskKind::Expand);
        match f {
            ResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.name, "memex_expand_reply")
            }
            _ => panic!("unexpected response format"),
        }

        let f = response_format_for_task(TaskKind::Synthesize);
        match f {
            ResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.name, "memex_synth_reply")
            }
            _ => panic!("unexpected response format"),
        }

        let f = response_format_for_task(TaskKind::Extract);
        match f {
            ResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.name, "memex_pages_reply");
                assert_eq!(
                    json_schema.schema.expect("schema")["properties"]["pages"]["type"],
                    "array"
                );
            }
            _ => panic!("unexpected response format"),
        }

        let f = response_format_for_task(TaskKind::Merge);
        match f {
            ResponseFormat::JsonSchema { json_schema } => {
                assert_eq!(json_schema.name, "memex_pages_reply")
            }
            _ => panic!("unexpected response format"),
        }
    }

    #[test]
    fn finish_reason_str_formats_expected_names() {
        assert_eq!(finish_reason_str(Some(FinishReason::Stop)), "stop");
        assert_eq!(finish_reason_str(Some(FinishReason::Length)), "length");
        assert_eq!(
            finish_reason_str(Some(FinishReason::ToolCalls)),
            "tool_calls"
        );
        assert_eq!(
            finish_reason_str(Some(FinishReason::ContentFilter)),
            "content_filter"
        );
        assert_eq!(
            finish_reason_str(Some(FinishReason::FunctionCall)),
            "function_call"
        );
        assert_eq!(finish_reason_str(None), "unknown");
    }
}
