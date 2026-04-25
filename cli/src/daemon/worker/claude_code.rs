//! Claude Code subprocess worker.
//!
//! Spawns `claude -p --input-format stream-json --output-format stream-json ...`
//! once per worker and keeps it warm across jobs. Each job is one turn: write
//! one JSON user message to stdin, read JSON events from stdout until a
//! terminal `result` event, reply with the assistant text + extracted
//! citations.

use super::{TurnOutcome, WORKER_PROMPT};
use crate::daemon::config::WorkerConfig;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

/// A persistent `claude -p` subprocess speaking stream-json on stdin/stdout.
pub(super) struct ClaudeCodeSubprocess {
    #[allow(dead_code)]
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl ClaudeCodeSubprocess {
    pub(super) async fn spawn(cfg: &WorkerConfig) -> Result<Self> {
        let model = cfg
            .model
            .as_deref()
            .unwrap_or_else(|| cfg.backend.default_model());
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg("--verbose")
            .args(["--input-format", "stream-json"])
            .args(["--output-format", "stream-json"])
            .args(["--model", model])
            .args(["--system-prompt", WORKER_PROMPT])
            .arg("--disable-slash-commands")
            .args(["--tools", ""])
            .args(["--setting-sources", ""])
            .args(["--mcp-config", r#"{"mcpServers":{}}"#])
            .arg("--strict-mcp-config")
            .arg("--no-session-persistence");

        super::prepare_agent_cmd(
            &mut cmd,
            &[
                "HOME",
                "PATH",
                "LANG",
                "LC_ALL",
                "USER",
                "LOGNAME",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "ANTHROPIC_MODEL",
                "MEMEX_ROOT",
                // Test fixture: selects mock-claude-code.sh behavior in integration tests.
                "MOCK_CLAUDE_CODE_MODE",
            ],
        );

        let (child, stdin, stdout) = super::spawn_with_pipes(&mut cmd, "spawning claude")?;
        let stdout = BufReader::new(stdout).lines();

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Send one user turn as stream-json and collect the assistant reply.
    /// Returns a structured outcome distinguishing success, auth failure,
    /// and other Claude errors.
    pub(super) async fn one_turn(
        &mut self,
        user_text: &str,
        _task_kind: super::TaskKind,
    ) -> Result<TurnOutcome> {
        let user_msg = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": user_text}]
            }
        });
        let line = format!("{}\n", user_msg);
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("writing to claude stdin")?;
        self.stdin.flush().await.ok();

        let mut answer = String::new();
        let mut last_assistant_error: Option<String> = None;
        loop {
            let Some(line) = self
                .stdout
                .next_line()
                .await
                .context("reading claude stdout")?
            else {
                bail!("claude stdout EOF before result");
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let ev: StreamEvent = match serde_json::from_str(line) {
                Ok(e) => e,
                Err(e) => {
                    // Unknown/malformed event: skip but log. Silently
                    // continuing here once masked a real bug — claude
                    // emitted `api_error_status` as a number but we
                    // declared it `Option<String>`, the whole `result`
                    // event failed to parse, and the worker blocked on
                    // stdout until the turn-timeout fired.
                    tracing::warn!(
                        error = %e,
                        line_preview = %line.chars().take(200).collect::<String>(),
                        "skipping unparseable claude stream-json event"
                    );
                    continue;
                }
            };
            match ev {
                StreamEvent::Assistant { message, error } => {
                    if let Some(err_kind) = error {
                        last_assistant_error = Some(err_kind);
                    }
                    for block in message.content {
                        if block.ty == "text"
                            && let Some(t) = block.text
                        {
                            if answer.len() + t.len() > super::MAX_RESPONSE_BYTES {
                                bail!("agent response exceeded 1MB limit");
                            }
                            answer.push_str(&t);
                        }
                    }
                }
                StreamEvent::Result {
                    result,
                    is_error,
                    api_error_status,
                    usage,
                    ..
                } => {
                    let text = if !answer.is_empty() {
                        answer
                    } else {
                        result
                            .filter(|s| !s.is_empty())
                            .or_else(|| last_assistant_error.clone())
                            .unwrap_or_default()
                    };
                    if is_error {
                        // Prefer the per-assistant-event `error` kind (e.g.
                        // `authentication_failed`) over the API status when
                        // both are present — it's the most specific label.
                        let code = last_assistant_error.clone().or(api_error_status);
                        return Ok(TurnOutcome::BackendError { message: text, code });
                    }
                    let input_tokens = usage
                        .map(|u| {
                            u.input_tokens
                                .unwrap_or(0)
                                .saturating_add(u.cache_read_input_tokens.unwrap_or(0))
                                .saturating_add(u.cache_creation_input_tokens.unwrap_or(0))
                        })
                        .unwrap_or(0);
                    return Ok(TurnOutcome::Ok { text, input_tokens });
                }
                _ => {}
            }
        }
    }
}

/// Stream-json event shape (partial — only what we consume).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum StreamEvent {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "assistant")]
    Assistant {
        message: AssistantMessage,
        /// Top-level error code from Claude when the assistant event
        /// represents an error condition (e.g. "authentication_failed").
        #[serde(default)]
        error: Option<String>,
    },
    #[serde(rename = "result")]
    Result {
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        #[allow(dead_code)]
        subtype: Option<String>,
        /// True when the turn ended in an error. Paired with `result`
        /// carrying a short description.
        #[serde(default)]
        is_error: bool,
        /// Programmatic error status from Claude/API when available.
        /// Claude emits this as either a number (HTTP status like `404`)
        /// or a string (like `"overloaded"`), so we accept both and
        /// normalize to a string. Previously typed as `Option<String>`,
        /// which silently failed to deserialize on numeric statuses —
        /// the worker then skipped the `result` event entirely and
        /// blocked on stdout until the tokio turn-timeout fired.
        /// Propagated via `BackendError.code` so users see the status.
        #[serde(default, deserialize_with = "deserialize_api_error_status")]
        api_error_status: Option<String>,
        #[serde(default)]
        usage: Option<Usage>,
    },
    #[serde(other)]
    Other,
}

fn deserialize_api_error_status<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(serde_json::Value::Number(n)) => Ok(Some(n.to_string())),
        Some(serde_json::Value::Bool(b)) => Ok(Some(b.to_string())),
        // Anything else (object/array) is unexpected; stringify it so
        // the full diagnostic still reaches the user via `[code]` prefix.
        Some(other) => Ok(Some(other.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}
