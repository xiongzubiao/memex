//! Worker pool for agent workers.
//!
//! Each agent implementation (Claude Code, Codex, Gemini CLI, OpenAI API) is a sibling module behind
//! the same `WorkerPool` entry.

pub mod claude_code;
pub mod codex;
pub mod gemini_cli;
mod jsonrpc;
pub mod openai_api;
pub(crate) mod parse;

/// Maximum accumulated LLM response size (bytes). Prevents OOM from
/// runaway streaming. The largest model output is ~512KB (128K tokens).
pub(crate) const MAX_RESPONSE_BYTES: usize = 1_048_576;

/// Type alias for test-mode prompt-handler closures. Takes a rendered prompt,
/// returns the canned LLM response string.
#[cfg(any(test, feature = "test-harness"))]
pub type MockPromptFn = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>;

use crate::daemon::config::{Backend, WorkerConfig};
use crate::daemon::memex_handle::MemexHandle;
use crate::daemon::queue::{
    BackendJob, ExpandResult, IngestResult, JobReceiver, JobSender, MergeResult, SynthResult,
    WorkerError, queue,
};
use crate::memex_root;
use anyhow::Result;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Outcome of a single turn. All providers produce this shape.
///
/// A single `BackendError` variant covers every non-success path:
/// model-not-found (HTTP 404), auth failure, rate limit, schema-invalid
/// reply, etc. `code` carries the backend-native identifier when
/// available (`api_error_status` for Claude Code, `kind` for Codex,
/// OpenAI error `code` / type, `rpc=<n>` for JSON-RPC) so the caller can
/// log it and forward it untouched into the propagated error message.
/// No variant-level auth specialization — a user seeing
/// `[authentication_failed] ...` knows what to do.
#[derive(Debug)]
pub(crate) enum TurnOutcome {
    Ok {
        text: String,
        input_tokens: u64,
    },
    BackendError {
        message: String,
        code: Option<String>,
    },
}

impl TurnOutcome {
    pub(super) fn backend_err(message: impl Into<String>, code: impl Into<String>) -> Self {
        TurnOutcome::BackendError {
            message: message.into(),
            code: Some(code.into()),
        }
    }

    pub(super) fn backend_err_no_code(message: impl Into<String>) -> Self {
        TurnOutcome::BackendError {
            message: message.into(),
            code: None,
        }
    }
}

/// Worker prompt shared by every backend (extract / merge / expand /
/// synthesize task instructions). Each backend embeds it as
/// `baseInstructions` / `--system-prompt` / equivalent on session start.
pub(super) const WORKER_PROMPT: &str = include_str!("prompt.txt");

/// Spawn the configured command and detach its stdin / stdout. Uniform
/// error messages across backends; centralizes the stdin/stdout-take
/// dance that was duplicated in claude_code / codex / gemini_cli.
pub(super) fn spawn_with_pipes(
    cmd: &mut tokio::process::Command,
    label: &'static str,
) -> Result<(
    tokio::process::Child,
    tokio::process::ChildStdin,
    tokio::process::ChildStdout,
)> {
    use anyhow::{Context, anyhow};
    let mut child = cmd.spawn().context(label)?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("{label}: missing stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("{label}: missing stdout"))?;
    Ok((child, stdin, stdout))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskKind {
    Expand,
    Synthesize,
    Extract,
    Merge,
}

/// Outcome of one job, variant matches the BackendJob that was dispatched.
#[derive(Debug)]
pub(super) enum JobOutcome {
    Expand(ExpandResult),
    Synth(SynthResult),
    Ingest(IngestResult),
    Merge(MergeResult),
}

/// Configure a subprocess command for an agent worker: sanitize env,
/// pipe stdio, enable kill-on-drop, and set cwd to MEMEX_ROOT (neutral
/// dir that prevents picking up the user's project-local CLAUDE.md /
/// GEMINI.md). `allowlist` names the env vars to inherit from the parent;
/// all `LC_*` locale vars are forwarded unconditionally.
pub(super) fn prepare_agent_cmd(cmd: &mut tokio::process::Command, allowlist: &[&str]) {
    cmd.env_clear();
    // Mark this subprocess as memex-internal so the SessionStart hook in
    // a spawned agent (Claude Code, Codex, Gemini-CLI) exits immediately
    // instead of trying to ingest the daemon's own subagent transcript.
    // The transcript filter (`SessionFilter::InternalSession`) is a
    // belt-and-suspenders backup; this env var is the primary guard.
    cmd.env("MEMEX_INTERNAL", "1");
    for key in allowlist {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    for (k, v) in std::env::vars().filter(|(k, _)| k.starts_with("LC_")) {
        cmd.env(k, v);
    }
    cmd.current_dir(memex_root());
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
}

/// Drain stderr from a (crashed) child subprocess up to `max_bytes`.
/// Called on spawn/init failure so the real diagnostic reaches the CLI
/// instead of a generic "subprocess crashed" message. Never blocks — if
/// reading stderr stalls for more than 1s we return what we have.
pub(super) async fn drain_stderr(
    stderr: Option<tokio::process::ChildStderr>,
    max_bytes: usize,
) -> String {
    let Some(s) = stderr else {
        return String::new();
    };
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(max_bytes);
    let mut capped = s.take(max_bytes as u64);
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        capped.read_to_end(&mut buf),
    )
    .await;
    String::from_utf8_lossy(&buf).trim().to_string()
}

/// Variant-wrapped subprocess. Each variant holds the provider-specific
/// struct defined in its own module. Dispatch methods below enable
/// protocol-agnostic use in `run_job_with_retry`.
enum Subprocess {
    ClaudeCode(claude_code::ClaudeCodeSubprocess),
    Codex(codex::CodexSubprocess),
    GeminiCli(gemini_cli::GeminiCliSubprocess),
    OpenAiApi(openai_api::OpenAiApiSubprocess),
}

impl Subprocess {
    async fn spawn(backend: Backend, cfg: &WorkerConfig) -> Result<Self> {
        Ok(match backend {
            Backend::ClaudeCode => {
                Subprocess::ClaudeCode(claude_code::ClaudeCodeSubprocess::spawn(cfg).await?)
            }
            Backend::Codex => Subprocess::Codex(codex::CodexSubprocess::spawn(cfg).await?),
            Backend::GeminiCli => {
                Subprocess::GeminiCli(gemini_cli::GeminiCliSubprocess::spawn(cfg).await?)
            }
            Backend::OpenAiApi => {
                Subprocess::OpenAiApi(openai_api::OpenAiApiSubprocess::spawn(cfg).await?)
            }
        })
    }

    async fn one_turn(&mut self, prompt: &str, task_kind: TaskKind) -> Result<TurnOutcome> {
        match self {
            Subprocess::ClaudeCode(s) => s.one_turn(prompt, task_kind).await,
            Subprocess::Codex(s) => s.one_turn(prompt, task_kind).await,
            Subprocess::GeminiCli(s) => s.one_turn(prompt, task_kind).await,
            Subprocess::OpenAiApi(s) => s.one_turn(prompt, task_kind).await,
        }
    }

    /// Reset state for the restart cadence. Returns `true` if the
    /// subprocess is still usable after the soft reset (codex fresh_thread,
    /// gemini-cli fresh_session); `false` if the caller should drop and respawn
    /// on next use (claude code, full subprocess restart is the documented
    /// reset semantic).
    async fn soft_reset(&mut self, cfg: &WorkerConfig) -> bool {
        match self {
            Subprocess::ClaudeCode(_) => false, // full respawn on next job
            Subprocess::Codex(s) => s.fresh_thread(cfg).await.is_ok(),
            Subprocess::GeminiCli(s) => s.fresh_session(cfg).await.is_ok(),
            Subprocess::OpenAiApi(_) => true, // stateless requests
        }
    }
}

/// Autoscaling worker pool.
///
/// Spawns one persistent "min" worker eagerly. Additional non-min workers are
/// spawned on demand by `submit()` when no idle worker is available and `live
/// < max_count`. Non-min workers exit after `idle_reap_sec` with no job.
pub struct WorkerPool {
    tx: JobSender,
    rx: JobReceiver,
    cfg: WorkerConfig,
    memex_handle: Arc<MemexHandle>,
    live: Arc<AtomicUsize>,
    busy: Arc<AtomicUsize>,
    next_worker_id: Arc<AtomicUsize>,
}

impl WorkerPool {
    /// Build the pool's fields without spawning any worker tasks.
    fn empty(cfg: WorkerConfig, memex_handle: Arc<MemexHandle>) -> Self {
        let (tx, rx) = queue(cfg.max_count);
        Self {
            tx,
            rx,
            cfg,
            memex_handle,
            live: Arc::new(AtomicUsize::new(0)),
            busy: Arc::new(AtomicUsize::new(0)),
            next_worker_id: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn new(cfg: WorkerConfig, memex_handle: Arc<MemexHandle>) -> Self {
        let pool = Self::empty(cfg, memex_handle);
        pool.spawn_worker(true);
        pool
    }

    /// Enqueue a job, spawning an extra worker first if no live worker is
    /// idle (queue backlog OR all live workers currently busy) and there is
    /// headroom under `max_count`.
    pub async fn submit(
        &self,
        job: BackendJob,
    ) -> Result<(), async_channel::SendError<BackendJob>> {
        let live = self.live.load(Ordering::Acquire);
        let busy = self.busy.load(Ordering::Acquire);
        if (!self.tx.is_empty() || busy >= live) && live < self.cfg.max_count {
            self.spawn_extra_worker();
        }
        self.tx.send(job).await
    }

    fn spawn_extra_worker(&self) {
        let prev = self.live.fetch_add(1, Ordering::AcqRel);
        if prev >= self.cfg.max_count {
            self.live.fetch_sub(1, Ordering::AcqRel);
            return;
        }
        self.launch_worker_task(false);
    }

    fn spawn_worker(&self, is_min: bool) {
        self.live.fetch_add(1, Ordering::AcqRel);
        self.launch_worker_task(is_min);
    }

    fn launch_worker_task(&self, is_min: bool) {
        let worker_id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
        let rx = self.rx.clone();
        let cfg = self.cfg.clone();
        let memex_handle = self.memex_handle.clone();
        let live = self.live.clone();
        let busy = self.busy.clone();
        tokio::spawn(async move {
            // RAII: decrement live on drop so a panic in `run()` can't
            // permanently inflate the worker count.
            let _live_guard = CountGuard(live);
            run(worker_id, rx, cfg, memex_handle, is_min, busy).await;
        });
    }

    /// Build a pool without spawning any LLM-subprocess workers.
    /// Handler unit tests only exercise code paths that don't reach
    /// the queue; the integration retrieval-only path may hit the
    /// queue if `intent` forces expansion.
    ///
    /// Pre-set `live` to `max_count` so `submit()`'s autoscale branch
    /// skips spawning a real LLM worker (which would flake on LLM
    /// nondeterminism or burn API credits in environments where the
    /// `claude` CLI is on PATH). Pair that with a tiny drainer task
    /// that drops every queued job's reply sender so callers see
    /// "expansion unavailable" deterministically and fall back to
    /// un-expanded retrieval.
    #[cfg(any(test, feature = "test-harness"))]
    pub fn new_inert_for_test() -> Self {
        let pool = Self::empty(WorkerConfig::default(), MemexHandle::new());
        pool.live.store(pool.cfg.max_count, Ordering::Release);
        let rx = pool.rx.clone();
        tokio::spawn(async move { while rx.recv().await.is_ok() {} });
        pool
    }

    /// Drain the job queue via test closures instead of LLM subprocesses.
    /// Extract closures are required; merge closures are optional (jobs that
    /// would call merge return an error if no closure is supplied). Expand
    /// and Synth jobs are unsupported — return Backend errors.
    #[cfg(any(test, feature = "test-harness"))]
    pub fn new_with_mock(extract: MockPromptFn, merge: Option<MockPromptFn>) -> Self {
        let pool = Self::empty(WorkerConfig::default(), MemexHandle::new());
        // Pretend max-count workers are already alive so `submit()`'s
        // autoscale path never spawns a real subprocess worker. Without
        // this, `submit()` sees `live=0 < max_count` and launches a
        // backend subprocess, which fails in tests with no LLM backend
        // configured.
        pool.live
            .store(pool.cfg.max_count, std::sync::atomic::Ordering::Release);
        let rx = pool.rx.clone();
        tokio::spawn(async move {
            while let Ok(job) = rx.recv().await {
                match job {
                    BackendJob::Ingest(j) => {
                        let prompt = build_extract_prompt(&j);
                        let response = extract(&prompt);
                        let reply = match parse::parse_ingest(&response) {
                            Ok(r) => Ok(r),
                            Err(e) => Err(WorkerError::Backend {
                                message: e.reason,
                                code: None,
                            }),
                        };
                        let _ = j.reply.send(reply);
                    }
                    BackendJob::Merge(j) => match &merge {
                        Some(m) => {
                            let prompt = build_merge_prompt(&j.pages);
                            let response = m(&prompt);
                            let reply = match parse::parse_merge(&response) {
                                Ok(r) => Ok(r),
                                Err(e) => Err(WorkerError::Backend {
                                    message: e.reason,
                                    code: None,
                                }),
                            };
                            let _ = j.reply.send(reply);
                        }
                        None => {
                            let _ = j.reply.send(Err(WorkerError::Backend {
                                message: "mock pool: merge closure not provided".into(),
                                code: None,
                            }));
                        }
                    },
                    BackendJob::Expand(j) => {
                        let _ = j.reply.send(Err(WorkerError::Backend {
                            message: "mock pool does not handle Expand jobs".into(),
                            code: None,
                        }));
                    }
                    BackendJob::Synth(j) => {
                        let _ = j.reply.send(Err(WorkerError::Backend {
                            message: "mock pool does not handle Synth jobs".into(),
                            code: None,
                        }));
                    }
                }
            }
        });
        pool
    }
}

async fn run(
    worker_id: usize,
    rx: JobReceiver,
    cfg: WorkerConfig,
    memex_handle: Arc<MemexHandle>,
    is_min: bool,
    busy: Arc<AtomicUsize>,
) {
    let span = tracing::info_span!("worker", id = worker_id, backend = ?cfg.backend);
    let _enter = span.enter();

    let mut subprocess: Option<Subprocess> = None;
    let mut jobs_done: u32 = 0;
    // The last turn's API-billed input_tokens. In claude-code stream-json
    // the running conversation is replayed every turn, so this value is
    // the agent's currently-used context window — the right thing to
    // compare against `max_input_tokens` to decide "would the next turn
    // risk overflow."
    let mut last_turn_input_tokens: u64 = 0;

    let model_name = cfg
        .model
        .as_deref()
        .unwrap_or_else(|| cfg.backend.default_model());
    let max_input = memex_core::model::lookup_model(model_name).max_input_tokens as u64;
    let context_threshold = max_input * 7 / 10;

    let idle_reap = Duration::from_secs(cfg.idle_reap_sec);
    loop {
        let job = if is_min {
            match rx.recv().await {
                Ok(j) => j,
                Err(_) => break,
            }
        } else {
            match tokio::time::timeout(idle_reap, rx.recv()).await {
                Ok(Ok(j)) => j,
                Ok(Err(_)) => break,
                Err(_) => {
                    tracing::info!(worker_id, "worker reaped on idle");
                    break;
                }
            }
        };
        // RAII: increment then guard, so if the fetch_add-then-guard sequence
        // is interrupted the guard never runs without a prior increment. The
        // guard decrements on drop — even on panic mid-turn — so the busy
        // count can't be permanently inflated.
        let _busy_guard = CountGuard::inc(&busy);
        let count_hit = jobs_done >= cfg.restart_after_jobs;
        let context_hit = last_turn_input_tokens >= context_threshold;
        if count_hit || context_hit {
            if let Some(sp) = subprocess.as_mut() {
                let trigger = if context_hit { "context" } else { "count" };
                tracing::info!(
                    last_turn_input_tokens,
                    context_threshold,
                    jobs_done,
                    trigger,
                    "restart triggered"
                );
                if !sp.soft_reset(&cfg).await {
                    subprocess = None;
                }
            }
            jobs_done = 0;
        }

        let (outcome, input_tokens) =
            run_job_with_retry(&mut subprocess, &cfg, &memex_handle, &job).await;

        // Count any turn that actually ran to a terminal event. Only skip
        // Crash / Timeout — the subprocess didn't complete a turn. New
        // WorkerError variants default to "counted" rather than silently
        // undercounting.
        let counted = !matches!(
            &outcome,
            JobOutcome::Expand(Err(WorkerError::Crash(_) | WorkerError::Timeout { .. }))
                | JobOutcome::Synth(Err(WorkerError::Crash(_) | WorkerError::Timeout { .. }))
                | JobOutcome::Ingest(Err(WorkerError::Crash(_) | WorkerError::Timeout { .. }))
                | JobOutcome::Merge(Err(WorkerError::Crash(_) | WorkerError::Timeout { .. }))
        );
        if counted {
            jobs_done = jobs_done.saturating_add(1);
        }
        last_turn_input_tokens = input_tokens;

        // Deliver reply on the matching variant. The type invariant is that
        // run_job_with_retry's outcome variant matches the job's variant
        // (the prompt construction branch drives both), so the catch-all
        // should be unreachable. If it ever fires — e.g., a future editor
        // adds a new BackendJob variant without extending run_job_with_retry
        // — log and drop the reply rather than panicking the worker (which
        // would kill the min worker and force a cold respawn).
        match (job, outcome) {
            (BackendJob::Expand(j), JobOutcome::Expand(r)) => {
                let _ = j.reply.send(r);
            }
            (BackendJob::Synth(j), JobOutcome::Synth(r)) => {
                let _ = j.reply.send(r);
            }
            (BackendJob::Ingest(j), JobOutcome::Ingest(r)) => {
                let _ = j.reply.send(r);
            }
            (BackendJob::Merge(j), JobOutcome::Merge(r)) => {
                let _ = j.reply.send(r);
            }
            _ => {
                tracing::error!("internal type mismatch between BackendJob and JobOutcome");
                debug_assert!(false, "job type mismatch");
            }
        }
        // _busy_guard drops here, decrementing busy.
    }
}

/// Atomic counter guard: decrements on drop. Used to keep `WorkerPool`'s
/// `live` and `busy` counters correct across normal exit, idle reap, and
/// panic within the worker task. Also used by `server::run_daemon` to
/// track the startup-reconcile background task in `in_flight`, so the
/// drain loop waits for it on SIGTERM.
pub(crate) struct CountGuard(Arc<AtomicUsize>);

impl CountGuard {
    /// Increment `counter` now; the returned guard decrements on drop.
    pub(crate) fn inc(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter.clone())
    }
}

impl Drop for CountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn run_job_with_retry(
    subprocess: &mut Option<Subprocess>,
    cfg: &WorkerConfig,
    memex_handle: &Arc<MemexHandle>,
    job: &BackendJob,
) -> (JobOutcome, u64) {
    let model_name = cfg
        .model
        .as_deref()
        .unwrap_or_else(|| cfg.backend.default_model());

    // Two job groups with different prompt-construction needs:
    //
    // - Interactive query jobs (Expand, Synth): may carry an `intent`
    //   from the user's `--intent` flag; if so the worker prompt is
    //   prefixed with "Intent: <text>\n\n".
    // - Batch LLM jobs (Ingest, Merge): no intent concept; the prompt
    //   is built by the dedicated extract/merge helpers and used as-is.
    //
    // Each arm produces a fully-formed prompt so the unified machinery
    // below (cache key, subprocess, parse) doesn't need to know which
    // group a job came from.
    let (prompt, kind) = match job {
        BackendJob::Expand(j) => {
            let body = format!("[TASK: EXPAND]\n\nQuestion: {}\n", j.question);
            (
                apply_intent_prefix(&body, j.intent.as_deref()),
                TaskKind::Expand,
            )
        }
        BackendJob::Synth(j) => {
            let body = format!(
                "[TASK: SYNTHESIZE]\n\n{}\n\nQuestion: {}\n",
                j.context, j.question
            );
            (
                apply_intent_prefix(&body, j.intent.as_deref()),
                TaskKind::Synthesize,
            )
        }
        BackendJob::Ingest(j) => (build_extract_prompt(j), TaskKind::Extract),
        BackendJob::Merge(j) => (build_merge_prompt(&j.pages), TaskKind::Merge),
    };

    let task_label = match kind {
        TaskKind::Expand => "expand",
        TaskKind::Synthesize => "synth",
        TaskKind::Extract => "extract",
        TaskKind::Merge => "merge",
    };

    // Compute the cache key from (model, task, system_prompt, user_prompt).
    // WORKER_PROMPT is the system prompt shared by all backends.
    let cache_key =
        memex_core::llm_cache::cache_key(model_name, task_label, WORKER_PROMPT, &prompt);

    // Try to find a cached result. The MemexHandle is single-root and
    // pre-populated by the handler before any jobs are enqueued, so
    // `get` returns the active handle.
    let cached_memex: Option<std::sync::Arc<memex_core::Memex>> = memex_handle.get();

    if let Some(ref mx) = cached_memex
        && let Ok(Some(cached_text)) = mx
            .search()
            .with_connection(|c| memex_core::llm_cache::lookup_cache(c, &cache_key))
    {
        tracing::debug!(task_label, "llm_cache hit");
        let outcome = match kind {
            TaskKind::Expand => {
                JobOutcome::Expand(parse::parse_expansion(&cached_text).map_err(|e| {
                    WorkerError::Backend {
                        message: e.raw,
                        code: None,
                    }
                }))
            }
            TaskKind::Synthesize => {
                JobOutcome::Synth(parse::parse_synthesis(&cached_text).map_err(|e| {
                    WorkerError::Backend {
                        message: e.raw,
                        code: None,
                    }
                }))
            }
            TaskKind::Extract => {
                JobOutcome::Ingest(parse::parse_ingest(&cached_text).map_err(|e| {
                    WorkerError::Backend {
                        message: e.raw,
                        code: None,
                    }
                }))
            }
            TaskKind::Merge => JobOutcome::Merge(parse::parse_merge(&cached_text).map_err(|e| {
                WorkerError::Backend {
                    message: e.raw,
                    code: None,
                }
            })),
        };
        return (outcome, 0);
    }

    let timeout = std::time::Duration::from_secs(cfg.timeout_sec);
    let mut last_err: WorkerError = WorkerError::Crash("no attempt completed".into());

    let as_err = |e: WorkerError| match kind {
        TaskKind::Expand => JobOutcome::Expand(Err(e)),
        TaskKind::Synthesize => JobOutcome::Synth(Err(e)),
        TaskKind::Extract => JobOutcome::Ingest(Err(e)),
        TaskKind::Merge => JobOutcome::Merge(Err(e)),
    };

    // Schema-parse failures: agent gave us text, but it wasn't the JSON
    // we asked for. Surface raw text with no code.
    let parse_err = |e: parse::ParseFailure, stage: &str| -> WorkerError {
        tracing::warn!(
            parse_error = %e.reason,
            raw_len = e.raw.len(),
            raw_preview = %e.preview(400),
            "{} reply didn't parse",
            stage,
        );
        WorkerError::Backend {
            message: e.raw,
            code: None,
        }
    };

    for attempt in 0..2u32 {
        if subprocess.is_none() {
            match Subprocess::spawn(cfg.backend, cfg).await {
                Ok(s) => *subprocess = Some(s),
                Err(e) => {
                    tracing::warn!(attempt, ?e, "spawn failed");
                    last_err = WorkerError::Crash(format!("spawn failed: {e}"));
                    continue;
                }
            }
        }
        let sp = subprocess.as_mut().expect("spawned above");
        let turn = tokio::time::timeout(timeout, sp.one_turn(&prompt, kind)).await;

        match turn {
            Ok(Ok(TurnOutcome::Ok { text, input_tokens })) => {
                // Populate cache on success.
                if let Some(ref mx) = cached_memex {
                    let _ = mx.search().with_connection(|c| {
                        memex_core::llm_cache::insert_cache(c, &cache_key, &text)?;
                        Ok(())
                    });
                }
                let outcome = match kind {
                    TaskKind::Expand => JobOutcome::Expand(
                        parse::parse_expansion(&text).map_err(|e| parse_err(e, "expand")),
                    ),
                    TaskKind::Synthesize => JobOutcome::Synth(
                        parse::parse_synthesis(&text).map_err(|e| parse_err(e, "synth")),
                    ),
                    TaskKind::Extract => JobOutcome::Ingest(
                        parse::parse_ingest(&text).map_err(|e| parse_err(e, "ingest")),
                    ),
                    TaskKind::Merge => JobOutcome::Merge(
                        parse::parse_merge(&text).map_err(|e| parse_err(e, "merge")),
                    ),
                };
                return (outcome, input_tokens);
            }
            Ok(Ok(TurnOutcome::BackendError { message, code })) => {
                // Reset protocol state: the subprocess may have buffered
                // notifications for the errored turn that would otherwise
                // arrive as stale IDs on the next turn. For claude code,
                // soft_reset returns false and the next job respawns.
                if let Some(sp) = subprocess.as_mut()
                    && !sp.soft_reset(cfg).await
                {
                    *subprocess = None;
                }
                // No log here: the handler logs this as
                // `ERROR "handler error" code=backend_unavailable` with
                // the backend-native code prefixed into the message.
                return (as_err(WorkerError::Backend { message, code }), 0);
            }
            Ok(Err(e)) => {
                tracing::warn!(attempt, ?e, "turn crash; retrying");
                last_err = WorkerError::Crash(e.to_string());
                *subprocess = None;
            }
            Err(_elapsed) => {
                tracing::warn!(attempt, secs = cfg.timeout_sec, "turn timed out; retrying");
                last_err = WorkerError::Timeout {
                    secs: cfg.timeout_sec,
                };
                *subprocess = None;
            }
        }
    }
    (as_err(last_err), 0)
}

/// Prepend "Intent: <text>\n\n" to the user prompt when intent is non-blank.
/// Returns the original string unchanged when intent is None or blank.
fn apply_intent_prefix(user: &str, intent: Option<&str>) -> String {
    match intent {
        Some(t) if !t.trim().is_empty() => format!("Intent: {t}\n\n{user}"),
        _ => user.to_string(),
    }
}

pub(crate) fn build_extract_prompt(job: &crate::daemon::queue::IngestJob) -> String {
    let segments: Vec<serde_json::Value> = job
        .segments
        .iter()
        .map(|s| {
            let mut m = serde_json::Map::new();
            if let Some(idx) = s.index {
                m.insert("index".into(), idx.into());
            }
            if let Some(role) = &s.role {
                m.insert("role".into(), role.clone().into());
            }
            if let Some(ts) = &s.timestamp {
                m.insert("timestamp".into(), ts.clone().into());
            }
            m.insert("text".into(), s.text.clone().into());
            serde_json::Value::Object(m)
        })
        .collect();
    let mut payload = json!({
        "segments": segments,
        "source": job.source,
    });
    if let Some(chunk) = &job.chunk {
        payload["chunk_index"] = chunk.index.into();
        payload["total_chunks"] = chunk.total.into();
    }
    format!("[TASK: EXTRACT]\n\n{}\n", payload)
}

pub(crate) fn build_merge_prompt(pages: &[crate::daemon::queue::MergePair]) -> String {
    let payload = json!({
        "pages": pages
            .iter()
            .map(|p| json!({
                "slug": p.slug,
                "existing": p.existing,
                "proposed": p.proposed,
            }))
            .collect::<Vec<_>>(),
    });
    format!("[TASK: MERGE]\n\n{}\n", payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::queue::{ChunkPosition, ExtractSegment, IngestJob};

    /// Build a throwaway IngestJob for prompt-shape tests. The reply channel's
    /// receiver is dropped immediately — these tests assert on the prompt
    /// string, not on worker round-trip.
    fn job_for_test(
        segments: Vec<ExtractSegment>,
        source: &str,
        chunk: Option<ChunkPosition>,
    ) -> IngestJob {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        IngestJob {
            segments,
            source: source.into(),
            chunk,
            reply: tx,
        }
    }

    #[test]
    fn intent_prefix_helper_prepends_intent_when_set() {
        let user = "user question".to_string();
        let with = apply_intent_prefix(&user, Some("web page load times"));
        assert!(with.starts_with("Intent: web page load times\n\n"));
        assert!(with.ends_with("user question"));

        let without = apply_intent_prefix(&user, None);
        assert_eq!(without, user);

        let empty = apply_intent_prefix(&user, Some("   "));
        assert_eq!(empty, user, "blank intent should not prefix");
    }

    #[test]
    fn context_threshold_resolves_for_each_agent() {
        for backend in [
            Backend::ClaudeCode,
            Backend::Codex,
            Backend::GeminiCli,
            Backend::OpenAiApi,
        ] {
            let name = backend.default_model();
            let info = memex_core::model::lookup_model(name);
            assert!(
                info.max_input_tokens > 0,
                "expected non-zero max_input_tokens for {name}"
            );
        }
    }

    #[test]
    fn build_extract_prompt_emits_role_and_timestamp_for_transcript_segments() {
        let job = job_for_test(
            vec![
                ExtractSegment {
                    index: Some(1),
                    role: Some("user".into()),
                    timestamp: Some("2026-04-23T12:00:00Z".into()),
                    text: "hello".into(),
                },
                ExtractSegment {
                    index: Some(2),
                    role: Some("assistant".into()),
                    timestamp: Some("2026-04-23T12:00:01Z".into()),
                    text: "world".into(),
                },
            ],
            "/tmp/transcript.jsonl",
            None,
        );
        let p = build_extract_prompt(&job);
        assert!(p.starts_with("[TASK: EXTRACT]"));
        assert!(p.contains("\"role\":\"user\""));
        assert!(p.contains("\"role\":\"assistant\""));
        assert!(p.contains("\"timestamp\":\"2026-04-23T12:00:00Z\""));
        assert!(p.contains("\"source\":\"/tmp/transcript.jsonl\""));
        assert!(
            !p.contains("\"chunk_index\""),
            "transcript prompt must not include chunk metadata"
        );
    }

    #[test]
    fn build_extract_prompt_handles_empty_segments() {
        let job = job_for_test(vec![], "/tmp/empty.jsonl", None);
        let p = build_extract_prompt(&job);
        assert!(p.contains("\"segments\":[]"));
    }

    #[test]
    fn build_extract_prompt_omits_role_for_document_segments() {
        let job = job_for_test(
            vec![ExtractSegment {
                index: None,
                role: None,
                timestamp: None,
                text: "# Heading\n\nbody".into(),
            }],
            "https://example.com/doc",
            None,
        );
        let p = build_extract_prompt(&job);
        assert!(p.starts_with("[TASK: EXTRACT]"));
        assert!(
            !p.contains("\"role\""),
            "document segment must not emit role: {p}"
        );
        assert!(p.contains("\"text\":\"# Heading"));
        assert!(p.contains("\"source\":\"https://example.com/doc\""));
    }

    #[test]
    fn build_extract_prompt_includes_chunk_metadata_when_set() {
        let job = job_for_test(
            vec![ExtractSegment {
                index: None,
                role: None,
                timestamp: None,
                text: "chunk body".into(),
            }],
            "https://x.test/p",
            Some(ChunkPosition { index: 1, total: 3 }),
        );
        let p = build_extract_prompt(&job);
        assert!(p.starts_with("[TASK: EXTRACT]"));
        assert!(
            p.contains("\"chunk_index\":1"),
            "expected chunk_index=1: {p}"
        );
        assert!(p.contains("\"total_chunks\":3"));
    }

    #[test]
    fn build_extract_prompt_omits_chunk_metadata_when_unset() {
        let job = job_for_test(
            vec![ExtractSegment {
                index: None,
                role: None,
                timestamp: None,
                text: "single-chunk doc".into(),
            }],
            "https://x.test/p",
            None,
        );
        let p = build_extract_prompt(&job);
        assert!(p.starts_with("[TASK: EXTRACT]"));
        assert!(!p.contains("\"chunk_index\""));
        assert!(!p.contains("\"total_chunks\""));
    }

    #[test]
    fn build_extract_prompt_loses_no_segment_information() {
        use serde_json::Value;

        let segments = vec![
            ExtractSegment {
                index: Some(1),
                role: Some("user".into()),
                timestamp: Some("2026-04-23T12:00:00Z".into()),
                text: "first turn".into(),
            },
            ExtractSegment {
                index: Some(2),
                role: Some("assistant".into()),
                timestamp: Some("2026-04-23T12:00:30Z".into()),
                text: "reply with \"quotes\" and\nnewlines".into(),
            },
            ExtractSegment {
                index: Some(3),
                role: Some("user".into()),
                timestamp: None, // some agents miss timestamps on system turns
                text: "no timestamp on this one".into(),
            },
            ExtractSegment {
                index: Some(4),
                role: Some("tool".into()),
                timestamp: Some("2026-04-23T12:01:00Z".into()),
                text: "tool output with unicode: ✓ ☃ and a tab\there".into(),
            },
        ];
        let job = job_for_test(segments.clone(), "/path/to/session.jsonl", None);
        let p = build_extract_prompt(&job);

        let body = p
            .strip_prefix("[TASK: EXTRACT]\n\n")
            .expect("missing [TASK: EXTRACT] header")
            .trim_end();
        let parsed: Value = serde_json::from_str(body).expect("prompt body must be valid JSON");

        let segs = parsed["segments"]
            .as_array()
            .expect("segments must be an array");
        assert_eq!(segs.len(), segments.len(), "all segments must be preserved");

        for (i, expected) in segments.iter().enumerate() {
            let actual = &segs[i];
            assert_eq!(
                actual["text"].as_str(),
                Some(expected.text.as_str()),
                "segment {i} text must round-trip exactly (special chars too)",
            );
            assert_eq!(
                actual["role"].as_str(),
                expected.role.as_deref(),
                "segment {i} role must match",
            );
            match &expected.timestamp {
                Some(ts) => assert_eq!(
                    actual["timestamp"].as_str(),
                    Some(ts.as_str()),
                    "segment {i} timestamp must match when present",
                ),
                None => assert!(
                    actual.get("timestamp").is_none(),
                    "segment {i} missing timestamp must be absent in output, not null",
                ),
            }
            match expected.index {
                Some(idx) => assert_eq!(
                    actual["index"].as_u64(),
                    Some(idx as u64),
                    "segment {i} index must match",
                ),
                None => assert!(actual.get("index").is_none()),
            }
        }
        assert_eq!(
            parsed["source"].as_str(),
            Some("/path/to/session.jsonl"),
            "source must be preserved in envelope",
        );
    }

    /// Verify that the LLM cache wiring is reachable from the worker:
    /// open a real Memex in a tempdir, pre-populate the llm_cache table,
    /// and confirm that `MemexHandle::get` returns the handle and that
    /// `lookup_cache` / `insert_cache` round-trip correctly through
    /// `Db::with_connection`.
    #[test]
    fn llm_cache_accessible_via_memex_handle_get() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("memex");
        std::fs::create_dir_all(&root).unwrap();

        let cache = MemexHandle::new();
        // Empty cache returns None.
        assert!(cache.get().is_none(), "empty cache must return None");

        // Pre-populate via get_or_open (same as the handler does before dispatching jobs).
        let memex = cache.get_or_open(&root).unwrap();
        let got = cache.get();
        assert!(got.is_some(), "get must return handle after get_or_open");
        assert!(
            std::sync::Arc::ptr_eq(got.as_ref().unwrap(), &memex),
            "get must return the same Arc"
        );

        // Verify cache round-trip through with_connection.
        let key = memex_core::llm_cache::cache_key(
            "test-model",
            "synth",
            WORKER_PROMPT,
            "Intent: auth\n\nwhat are tokens?",
        );
        let miss = memex
            .search()
            .with_connection(|c| memex_core::llm_cache::lookup_cache(c, &key))
            .unwrap();
        assert!(miss.is_none(), "fresh db must have no cache entry");

        memex
            .search()
            .with_connection(|c| {
                memex_core::llm_cache::insert_cache(
                    c,
                    &key,
                    r#"{"answer":"tokens are...","citations":[]}"#,
                )?;
                Ok(())
            })
            .unwrap();

        let hit = memex
            .search()
            .with_connection(|c| memex_core::llm_cache::lookup_cache(c, &key))
            .unwrap();
        assert_eq!(
            hit.as_deref(),
            Some(r#"{"answer":"tokens are...","citations":[]}"#),
            "cache must return inserted value"
        );

        // Duplicate insert must be ignored (INSERT OR IGNORE).
        memex
            .search()
            .with_connection(|c| {
                memex_core::llm_cache::insert_cache(c, &key, "overwrite-attempt")?;
                Ok(())
            })
            .unwrap();
        let still_first = memex
            .search()
            .with_connection(|c| memex_core::llm_cache::lookup_cache(c, &key))
            .unwrap();
        assert_eq!(
            still_first.as_deref(),
            Some(r#"{"answer":"tokens are...","citations":[]}"#),
            "INSERT OR IGNORE must preserve first value"
        );
    }
}
