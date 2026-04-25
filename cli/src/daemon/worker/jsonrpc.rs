//! Shared JSON-RPC 2.0 primitives for the codex (`app-server`) and gemini cli
//! (`--acp`) workers. Claude Code uses stream-json, not JSON-RPC, so it
//! doesn't consume this module.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};

#[derive(Debug, Serialize)]
pub(super) struct RpcRequest<'a, T: Serialize> {
    jsonrpc: &'static str,
    id: u64,
    method: &'a str,
    params: T,
}

impl<'a, T: Serialize> RpcRequest<'a, T> {
    fn new(id: u64, method: &'a str, params: T) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method,
            params,
        }
    }
}

/// Line read from stdout: either a response (has `id`) or a notification
/// (has `method`). Both shapes deserialize through this struct.
#[derive(Debug, Deserialize)]
pub(super) struct RpcMessage {
    #[serde(default)]
    pub(super) id: Option<u64>,
    #[serde(default)]
    pub(super) method: Option<String>,
    #[serde(default)]
    pub(super) result: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) error: Option<RpcError>,
    #[serde(default)]
    pub(super) params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RpcError {
    pub(super) code: i64,
    pub(super) message: String,
}

/// stdin/stdout halves of a JSON-RPC subprocess plus an incrementing id
/// counter. Shared I/O primitives for codex and gemini cli workers.
pub(super) struct RpcClient {
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl RpcClient {
    pub(super) fn new(stdin: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
        }
    }

    pub(super) fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub(super) async fn send_request<T: Serialize>(
        &mut self,
        id: u64,
        method: &str,
        params: T,
    ) -> Result<()> {
        let req = RpcRequest::new(id, method, params);
        let line = serde_json::to_string(&req).context("serializing request")?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("writing to subprocess stdin")?;
        self.stdin.write_all(b"\n").await.ok();
        self.stdin.flush().await.ok();
        Ok(())
    }

    /// Read the next message line from stdout. Returns None on EOF.
    /// Tolerates empty and malformed lines (init events, streaming noise).
    pub(super) async fn read_msg(&mut self) -> Result<Option<RpcMessage>> {
        loop {
            let Some(line) = self
                .stdout
                .next_line()
                .await
                .context("reading subprocess stdout")?
            else {
                return Ok(None);
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<RpcMessage>(line) {
                Ok(m) => return Ok(Some(m)),
                Err(_) => continue,
            }
        }
    }

    /// Await a response with the given id. Buffers unrelated notifications
    /// silently; caller re-runs `read_msg` to drain them explicitly when
    /// streaming (e.g., during `session/prompt` or `turn/start`).
    pub(super) async fn await_response(&mut self, expect_id: u64) -> Result<RpcMessage> {
        loop {
            let Some(msg) = self.read_msg().await? else {
                bail!("subprocess stdout EOF before response to id={expect_id}");
            };
            if msg.id == Some(expect_id) {
                return Ok(msg);
            }
        }
    }
}
