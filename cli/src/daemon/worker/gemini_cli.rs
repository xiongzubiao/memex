//! Gemini CLI subprocess worker.
//!
//! Spawns `gemini --acp -e none` once per worker and keeps it warm. Speaks
//! Zed's Agent Communication Protocol (JSON-RPC 2.0 over stdio).
//!
//! Handshake: `initialize` → `session/new` → `session/set_mode plan` →
//! `session/set_model`. Per job: `session/prompt` with the agent prompt
//! prepended to the user text. The response (matched by id) is the
//! terminal event; streaming chunks come inside `session/update`
//! notifications. Restart cadence = fresh `session/new` on the same
//! subprocess (not a full respawn).

use super::jsonrpc::RpcClient;
use super::{TurnOutcome, WORKER_PROMPT};
use crate::daemon::config::WorkerConfig;
use crate::memex_root;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

// --- ACP method params ---

#[derive(Debug, Serialize)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "clientCapabilities")]
    client_capabilities: ClientCapabilities,
}

#[derive(Debug, Serialize)]
struct ClientCapabilities {
    fs: FsCaps,
    terminal: bool,
}

#[derive(Debug, Serialize)]
struct FsCaps {
    #[serde(rename = "readTextFile")]
    read_text_file: bool,
    #[serde(rename = "writeTextFile")]
    write_text_file: bool,
}

#[derive(Debug, Serialize)]
struct SessionNewParams<'a> {
    cwd: &'a str,
    #[serde(rename = "mcpServers")]
    mcp_servers: [(); 0],
}

#[derive(Debug, Serialize)]
struct SessionSetModeParams<'a> {
    #[serde(rename = "sessionId")]
    session_id: &'a str,
    #[serde(rename = "modeId")]
    mode_id: &'static str,
}

#[derive(Debug, Serialize)]
struct SessionSetModelParams<'a> {
    #[serde(rename = "sessionId")]
    session_id: &'a str,
    #[serde(rename = "modelId")]
    model_id: &'a str,
}

#[derive(Debug, Serialize)]
struct SessionPromptParams<'a> {
    #[serde(rename = "sessionId")]
    session_id: &'a str,
    prompt: Vec<PromptBlock<'a>>,
}

#[derive(Debug, Serialize)]
struct PromptBlock<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    text: &'a str,
}

// --- ACP responses / notifications ---

#[derive(Debug, Deserialize)]
struct SessionNewResult {
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(Debug, Deserialize)]
struct SessionPromptResult {
    #[serde(rename = "stopReason")]
    stop_reason: String,
    #[serde(rename = "_meta", default)]
    meta: Option<SessionPromptMeta>,
}

#[derive(Debug, Deserialize)]
struct SessionPromptMeta {
    #[serde(default)]
    quota: Option<MetaQuota>,
}

#[derive(Debug, Deserialize)]
struct MetaQuota {
    #[serde(default)]
    token_count: Option<MetaTokenCount>,
}

#[derive(Debug, Deserialize)]
struct MetaTokenCount {
    #[serde(default)]
    input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct SessionUpdateParams {
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    update: SessionUpdate,
}

#[derive(Debug, Deserialize)]
struct SessionUpdate {
    #[serde(rename = "sessionUpdate")]
    kind: String,
    #[serde(default)]
    content: Option<UpdateContent>,
}

#[derive(Debug, Deserialize)]
struct UpdateContent {
    #[serde(default)]
    text: Option<String>,
}

/// A persistent `gemini --acp` subprocess. Holds the session id so we can
/// issue `session/prompt` per job.
pub(super) struct GeminiCliSubprocess {
    #[allow(dead_code)]
    child: Child,
    rpc: RpcClient,
    session_id: String,
}

/// Deny all tools and MCP servers.
const DENY_ALL_POLICY_TOML: &str = r#"[[rule]]
toolName = "*"
mcpName = "*"
decision = "deny"
priority = 999
"#;

/// Write the deny-all policy to `memex_root()/.gemini/policies/memex.toml`
/// (gemini's conventional policies directory, rooted at MEMEX_ROOT
/// rather than $HOME so it travels with the daemon's state). Overwrite
/// unconditionally so on-disk content is always pinned to the compiled
/// constant — no stale file can silently re-permit tools.
async fn write_deny_all_policy() -> Result<std::path::PathBuf> {
    let dir = memex_root().join(".gemini").join("policies");
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating gemini policy dir {}", dir.display()))?;
    let path = dir.join("memex.toml");
    tokio::fs::write(&path, DENY_ALL_POLICY_TOML)
        .await
        .with_context(|| format!("writing gemini policy to {}", path.display()))?;
    Ok(path)
}

impl GeminiCliSubprocess {
    pub(super) async fn spawn(cfg: &WorkerConfig) -> Result<Self> {
        let policy_path = write_deny_all_policy().await?;

        let mut cmd = Command::new("gemini");
        // Gemini's Policy Engine (introduced after 1.0) rejects empty
        // allowlists — "Invalid policy rule: toolName is required". Pass
        // an explicit deny-all TOML via `--policy` so every built-in and
        // MCP tool is excluded from the model's memory entirely (in
        // non-interactive mode, `deny` skips the tool definition, saving
        // context and guaranteeing no tool-approval round-trips).
        //
        // `-e none` turns off auth prompting; `--extensions ""` disables
        // the extension loader. We don't pass `--allowed-tools` or
        // `--approval-mode` — the policy file is the source of truth.
        cmd.arg("--acp")
            .args(["-e", "none"])
            .args(["--extensions", ""])
            .args(["--policy", policy_path.to_string_lossy().as_ref()]);

        super::prepare_agent_cmd(
            &mut cmd,
            &[
                "HOME",
                "PATH",
                "LANG",
                "LC_ALL",
                "GEMINI_API_KEY",
                "GOOGLE_APPLICATION_CREDENTIALS",
                "GOOGLE_GENAI_USE_VERTEXAI",
                "MEMEX_ROOT",
                "MOCK_GEMINI_MODE",
            ],
        );

        let (mut child, stdin, stdout) =
            super::spawn_with_pipes(&mut cmd, "spawning gemini --acp")?;
        // Take stderr out of the child so we can drain it on crash.
        let stderr = child.stderr.take();

        let mut sub = Self {
            child,
            rpc: RpcClient::new(stdin, stdout),
            session_id: String::new(),
        };
        match sub.initialize().await {
            Ok(()) => {}
            Err(e) => {
                let diag = super::drain_stderr(stderr, 4096).await;
                let msg = if diag.is_empty() {
                    format!("gemini initialize: {e}")
                } else {
                    format!("gemini initialize: {e}; stderr: {diag}")
                };
                bail!("{msg}");
            }
        }
        sub.new_session(cfg)
            .await
            .context("gemini session/new + set_mode + set_model")?;
        Ok(sub)
    }

    async fn initialize(&mut self) -> Result<()> {
        let id = self.rpc.alloc_id();
        self.rpc
            .send_request(
                id,
                "initialize",
                InitializeParams {
                    protocol_version: 1,
                    client_capabilities: ClientCapabilities {
                        fs: FsCaps {
                            read_text_file: true,
                            write_text_file: false,
                        },
                        terminal: false,
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

    /// session/new + set_mode=plan + set_model. Consolidated helper used by
    /// both spawn and fresh_session.
    async fn new_session(&mut self, cfg: &WorkerConfig) -> Result<()> {
        // session/new
        let new_id = self.rpc.alloc_id();
        let cwd = memex_root().to_string_lossy().into_owned();
        self.rpc
            .send_request(
                new_id,
                "session/new",
                SessionNewParams {
                    cwd: &cwd,
                    mcp_servers: [],
                },
            )
            .await?;
        let resp = self.rpc.await_response(new_id).await?;
        if let Some(e) = resp.error {
            bail!("session/new failed: {}", e.message);
        }
        let sn: SessionNewResult = serde_json::from_value(
            resp.result
                .ok_or_else(|| anyhow!("session/new response missing result"))?,
        )
        .context("parsing session/new result")?;
        self.session_id = sn.session_id;

        // session/set_mode plan
        let mode_id = self.rpc.alloc_id();
        let session_id = self.session_id.clone();
        self.rpc
            .send_request(
                mode_id,
                "session/set_mode",
                SessionSetModeParams {
                    session_id: &session_id,
                    mode_id: "plan",
                },
            )
            .await?;
        let resp = self.rpc.await_response(mode_id).await?;
        if let Some(e) = resp.error {
            tracing::warn!(error = %e.message, "session/set_mode plan not accepted; continuing");
        }

        // session/set_model
        let model = cfg
            .model
            .as_deref()
            .unwrap_or_else(|| cfg.backend.default_model());
        let set_id = self.rpc.alloc_id();
        self.rpc
            .send_request(
                set_id,
                "session/set_model",
                SessionSetModelParams {
                    session_id: &session_id,
                    model_id: model,
                },
            )
            .await?;
        let resp = self.rpc.await_response(set_id).await?;
        if let Some(e) = resp.error {
            tracing::warn!(error = %e.message, "session/set_model not accepted; using agent default");
        }

        Ok(())
    }

    pub(super) async fn one_turn(
        &mut self,
        user_text: &str,
        _task_kind: super::TaskKind,
    ) -> Result<TurnOutcome> {
        // Gemini has no built-in system prompt — prepend agent prompt to each
        // user turn. Pre-allocate to avoid reallocation on the ~2KB prompt.
        let mut full = String::with_capacity(WORKER_PROMPT.len() + user_text.len() + 4);
        full.push_str(WORKER_PROMPT);
        full.push_str("\n\n");
        full.push_str(user_text);
        full.push('\n');

        let id = self.rpc.alloc_id();
        let session_id = self.session_id.clone();
        self.rpc
            .send_request(
                id,
                "session/prompt",
                SessionPromptParams {
                    session_id: &session_id,
                    prompt: vec![PromptBlock {
                        ty: "text",
                        text: &full,
                    }],
                },
            )
            .await?;

        // Drain notifications (session/update chunks) until we see the
        // session/prompt response (matched by id). That response is the
        // terminal event — there's no separate turn/completed as in codex.
        let mut answer = String::new();

        loop {
            let Some(msg) = self.rpc.read_msg().await? else {
                bail!("gemini stdout EOF before session/prompt response");
            };

            if msg.id == Some(id) {
                if let Some(e) = msg.error {
                    return Ok(TurnOutcome::BackendError {
                        message: e.message,
                        code: Some(format!("rpc={}", e.code)),
                    });
                }
                let sp: SessionPromptResult = match msg
                    .result
                    .ok_or_else(|| anyhow!("session/prompt response missing result"))
                    .and_then(|r| {
                        serde_json::from_value(r).context("parsing session/prompt result")
                    }) {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(TurnOutcome::BackendError {
                            message: e.to_string(),
                            code: None,
                        });
                    }
                };
                let input_tokens = sp
                    .meta
                    .as_ref()
                    .and_then(|m| m.quota.as_ref())
                    .and_then(|q| q.token_count.as_ref())
                    .map(|t| t.input_tokens)
                    .unwrap_or(0);
                return Ok(match sp.stop_reason.as_str() {
                    "end_turn" => TurnOutcome::Ok {
                        text: answer,
                        input_tokens,
                    },
                    other => TurnOutcome::backend_err_no_code(format!("stopReason={other}")),
                });
            }

            // Notification. Filter by session_id so stale chunks from a
            // previous session (e.g., leftover from fresh_session) don't
            // bleed into the current answer.
            if msg.method.as_deref() == Some("session/update")
                && let Some(params) = msg.params
                && let Ok(u) = serde_json::from_value::<SessionUpdateParams>(params)
                && u.session_id.as_deref().is_none_or(|s| s == self.session_id)
                && u.update.kind == "agent_message_chunk"
                && let Some(c) = u.update.content
                && let Some(text) = c.text
            {
                if answer.len() + text.len() > super::MAX_RESPONSE_BYTES {
                    bail!("agent response exceeded 1MB limit");
                }
                answer.push_str(&text);
            }
            // agent_thought_chunk, available_commands_update,
            // current_mode_update — ignored (display-only).
        }
    }

    /// Reset the session for the restart cadence. Keeps the subprocess;
    /// issues a fresh session/new + set_mode + set_model. Updates
    /// self.session_id.
    pub(super) async fn fresh_session(&mut self, cfg: &WorkerConfig) -> Result<()> {
        self.new_session(cfg).await
    }
}
