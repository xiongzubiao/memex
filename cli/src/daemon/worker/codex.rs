//! Codex subprocess worker.
//!
//! Spawns `codex app-server --listen stdio:// --disable plugins --disable codex_hooks`
//! once per worker and keeps it warm across jobs. Speaks JSON-RPC 2.0 over stdio.
//!
//! Handshake: `initialize` → `thread/start`. Per job: `turn/start` with the
//! task-tagged prompt; stream events until `turn/completed`. Restart cadence
//! = fresh `thread/start` on the same subprocess (not a full respawn).

use super::jsonrpc::RpcClient;
use super::{TurnOutcome, classify_error_text};
use crate::daemon::config::WorkerConfig;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

const AGENT_PROMPT: &str = include_str!("prompt.txt");

// --- App-server method params / responses ---

#[derive(Debug, Serialize)]
struct InitializeParams {
    #[serde(rename = "clientInfo")]
    client_info: ClientInfo,
}

#[derive(Debug, Serialize)]
struct ClientInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
struct ThreadStartParams<'a> {
    #[serde(rename = "baseInstructions")]
    base_instructions: &'a str,
    ephemeral: bool,
    sandbox: &'static str,
    model: &'a str,
    #[serde(rename = "approvalPolicy")]
    approval_policy: &'static str,
}

#[derive(Debug, Serialize)]
struct TurnStartParams<'a> {
    #[serde(rename = "threadId")]
    thread_id: &'a str,
    input: Vec<TurnInput<'a>>,
}

#[derive(Debug, Serialize)]
struct TurnInput<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    text: &'a str,
}

#[derive(Debug, Deserialize)]
struct ThreadStartResult {
    thread: ThreadId,
}

#[derive(Debug, Deserialize)]
struct ThreadId {
    id: String,
}

/// `item/agentMessage/delta` — streaming assistant text chunks.
#[derive(Debug, Deserialize)]
struct AgentMessageDelta {
    #[serde(default)]
    delta: Option<String>,
}

/// `turn/completed` — terminal event for a turn.
#[derive(Debug, Deserialize)]
struct TurnCompleted {
    turn: TurnTerminalState,
}

/// `thread/tokenUsage/updated` notification payload. `total.inputTokens`
/// is cumulative over the thread.
#[derive(Debug, Deserialize)]
struct TokenUsageUpdated {
    total: TokenTotals,
}

#[derive(Debug, Deserialize)]
struct TokenTotals {
    #[serde(rename = "inputTokens", default)]
    input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct TurnTerminalState {
    status: String,
    #[serde(default)]
    error: Option<TurnError>,
}

#[derive(Debug, Deserialize)]
struct TurnError {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// A persistent `codex app-server` subprocess. Holds the thread id so we
/// can issue `turn/start` per job.
pub(super) struct CodexSubprocess {
    #[allow(dead_code)]
    child: Child,
    rpc: RpcClient,
    thread_id: String,
    /// Cumulative `total.inputTokens` observed on the current thread.
    /// Per-turn input_tokens = current_total - previous_total. Reset to 0
    /// on fresh_thread.
    previous_total: u64,
}

impl CodexSubprocess {
    /// Spawn the subprocess, run `initialize`, then `thread/start`. Returns
    /// a ready-for-turns handle.
    pub(super) async fn spawn(cfg: &WorkerConfig) -> Result<Self> {
        let mut cmd = Command::new("codex");
        cmd.arg("app-server")
            .args(["--listen", "stdio://"])
            .args(["--disable", "plugins"])
            .args(["--disable", "codex_hooks"]);

        super::prepare_agent_cmd(
            &mut cmd,
            &[
                "HOME",
                "PATH",
                "LANG",
                "LC_ALL",
                "OPENAI_API_KEY",
                "MOCK_CODEX_MODE",
            ],
        );

        let mut child = cmd.spawn().context("spawning codex app-server")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing stdout"))?;

        let mut sub = Self {
            child,
            rpc: RpcClient::new(stdin, stdout),
            thread_id: String::new(),
            previous_total: 0,
        };
        sub.initialize().await.context("codex initialize")?;
        sub.start_thread(cfg).await.context("codex thread/start")?;
        Ok(sub)
    }

    async fn initialize(&mut self) -> Result<()> {
        let id = self.rpc.alloc_id();
        self.rpc
            .send_request(
                id,
                "initialize",
                InitializeParams {
                    client_info: ClientInfo {
                        name: "memex",
                        version: env!("CARGO_PKG_VERSION"),
                    },
                },
            )
            .await?;
        let resp = self.rpc.await_response(id).await?;
        if let Some(e) = resp.error {
            bail!("initialize failed: {}", e.message);
        }
        Ok(())
    }

    async fn start_thread(&mut self, cfg: &WorkerConfig) -> Result<()> {
        let id = self.rpc.alloc_id();
        let model = cfg.model.as_deref().unwrap_or_else(|| cfg.agent.default_model());
        self.rpc
            .send_request(
                id,
                "thread/start",
                ThreadStartParams {
                    base_instructions: AGENT_PROMPT,
                    ephemeral: true,
                    sandbox: "read-only",
                    model,
                    approval_policy: "never",
                },
            )
            .await?;
        let resp = self.rpc.await_response(id).await?;
        if let Some(e) = resp.error {
            bail!("thread/start failed: {}", e.message);
        }
        let result: ThreadStartResult = serde_json::from_value(
            resp.result
                .ok_or_else(|| anyhow!("thread/start response missing result"))?,
        )
        .context("parsing thread/start result")?;
        self.thread_id = result.thread.id;
        Ok(())
    }

    /// Send one turn; collect streaming `item/agentMessage/delta` chunks
    /// until `turn/completed`; return a structured outcome.
    pub(super) async fn one_turn(&mut self, user_text: &str) -> Result<TurnOutcome> {
        let id = self.rpc.alloc_id();
        let thread_id = self.thread_id.clone();
        self.rpc
            .send_request(
                id,
                "turn/start",
                TurnStartParams {
                    thread_id: &thread_id,
                    input: vec![TurnInput {
                        ty: "text",
                        text: user_text,
                    }],
                },
            )
            .await?;

        // Codex sends the turn/start ack (response), then notifications
        // until turn/completed. Drain until the terminal notification.
        let mut answer = String::new();
        let mut latest_total: Option<u64> = None;

        loop {
            let Some(msg) = self.rpc.read_msg().await? else {
                bail!("codex stdout EOF before turn/completed");
            };

            // Response to our turn/start (ack or error)
            if msg.id == Some(id) {
                if let Some(e) = msg.error {
                    return Ok(classify_error_text(&e.message));
                }
                continue;
            }

            match msg.method.as_deref() {
                Some("item/agentMessage/delta") => {
                    if let Some(params) = msg.params
                        && let Ok(d) = serde_json::from_value::<AgentMessageDelta>(params)
                        && let Some(delta) = d.delta
                    {
                        if answer.len() + delta.len() > super::MAX_RESPONSE_BYTES {
                            bail!("agent response exceeded 1MB limit");
                        }
                        answer.push_str(&delta);
                    }
                }
                Some("thread/tokenUsage/updated") => {
                    if let Some(params) = msg.params
                        && let Ok(tu) = serde_json::from_value::<TokenUsageUpdated>(params)
                    {
                        latest_total = Some(tu.total.input_tokens);
                    }
                }
                Some("turn/completed") => {
                    if let Some(params) = msg.params
                        && let Ok(tc) = serde_json::from_value::<TurnCompleted>(params)
                    {
                        match tc.turn.status.as_str() {
                            "completed" => {
                                let input_tokens = match latest_total {
                                    Some(current) => {
                                        let delta = current.saturating_sub(self.previous_total);
                                        self.previous_total = current;
                                        delta
                                    }
                                    None => 0,
                                };
                                return Ok(TurnOutcome::Ok {
                                    text: answer,
                                    input_tokens,
                                });
                            }
                            _ => {
                                let kind = tc.turn.error.as_ref().and_then(|e| e.kind.clone());
                                let message = tc
                                    .turn
                                    .error
                                    .as_ref()
                                    .and_then(|e| e.message.clone())
                                    .unwrap_or_else(|| {
                                        if answer.is_empty() {
                                            "turn failed".to_string()
                                        } else {
                                            answer.clone()
                                        }
                                    });
                                return Ok(match kind.as_deref() {
                                    Some("auth") | Some("unauthorized") | Some("forbidden") => {
                                        TurnOutcome::Auth(message)
                                    }
                                    _ => TurnOutcome::Structured(message),
                                });
                            }
                        }
                    } else {
                        return Ok(TurnOutcome::Structured("turn/completed malformed".into()));
                    }
                }
                _ => {}
            }
        }
    }

    /// Reset the thread for the restart cadence. Keeps the subprocess;
    /// just starts a fresh thread. Updates self.thread_id and resets the
    /// cumulative token counter.
    pub(super) async fn fresh_thread(&mut self, cfg: &WorkerConfig) -> Result<()> {
        self.previous_total = 0;
        self.start_thread(cfg).await
    }
}
