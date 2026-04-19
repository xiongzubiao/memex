//! Claude subprocess worker.
//!
//! Spawns `claude -p --input-format stream-json --output-format stream-json ...`
//! once per worker and keeps it warm across jobs. Each job is one turn: write
//! one JSON user message to stdin, read JSON events from stdout until a
//! terminal `result` event, reply with the assistant text + extracted
//! citations.

use super::TurnOutcome;
use crate::daemon::config::WorkerConfig;
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

/// Compiled-in agent prompt. See `agent_prompt.txt`.
const AGENT_PROMPT: &str = include_str!("prompt.txt");

/// A persistent `claude -p` subprocess speaking stream-json on stdin/stdout.
pub(super) struct ClaudeSubprocess {
    #[allow(dead_code)]
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl ClaudeSubprocess {
    pub(super) async fn spawn(cfg: &WorkerConfig) -> Result<Self> {
        let model = cfg.model.as_deref().unwrap_or_else(|| cfg.agent.default_model());
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg("--verbose")
            .args(["--input-format", "stream-json"])
            .args(["--output-format", "stream-json"])
            .args(["--model", model])
            .args(["--system-prompt", AGENT_PROMPT])
            .arg("--disable-slash-commands")
            .args(["--tools", ""])
            .args(["--setting-sources", ""])
            .args(["--mcp-config", r#"{"mcpServers":{}}"#])
            .arg("--strict-mcp-config")
            .arg("--no-session-persistence")
            .arg("--dangerously-skip-permissions");

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
                // Test fixture: selects mock-claude.sh behavior in integration tests.
                "MOCK_CLAUDE_MODE",
            ],
        );

        let mut child = cmd.spawn().context("spawning claude")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing stdout"))?;
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
    pub(super) async fn one_turn(&mut self, user_text: &str) -> Result<TurnOutcome> {
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
                Err(_) => continue,
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
                            answer.push_str(&t);
                        }
                    }
                }
                StreamEvent::Result {
                    result,
                    is_error,
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
                        return Ok(match last_assistant_error.as_deref() {
                            Some("authentication_failed") => TurnOutcome::Auth(text),
                            _ => TurnOutcome::Structured(text),
                        });
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
        /// Programmatic error status from Claude/API when available
        /// (e.g. "overloaded"). `None` on non-API errors.
        #[serde(default)]
        #[allow(dead_code)]
        api_error_status: Option<String>,
        #[serde(default)]
        usage: Option<Usage>,
    },
    #[serde(other)]
    Other,
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
