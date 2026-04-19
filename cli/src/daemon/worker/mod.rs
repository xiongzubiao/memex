//! Worker pool for agent subprocesses.
//!
//! Plan 3: single agent implementation (Claude). Plan 5 adds `codex.rs` and
//! `gemini.rs` as sibling modules behind the same `WorkerPool` entry.

pub mod claude;
pub mod codex;
pub mod gemini;
mod jsonrpc;
pub(crate) mod parse;

use crate::daemon::config::{Agent, WorkerConfig};
use crate::daemon::queue::{
    AgentJob, ExpandResult, JobReceiver, JobSender, SynthResult, WorkerError, queue,
};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Outcome of a single turn. All three providers produce this shape.
#[derive(Debug)]
pub(crate) enum TurnOutcome {
    Ok { text: String, input_tokens: u64 },
    Auth(String),
    Structured(String),
}

/// Outcome of one job, variant matches the AgentJob that was dispatched.
#[derive(Debug)]
pub(super) enum JobOutcome {
    Synth(SynthResult),
    Expand(ExpandResult),
}

/// Configure a subprocess command for an agent worker: sanitize env,
/// pipe stdio, enable kill-on-drop, and set cwd to `~/.memex` (a neutral
/// dir that prevents picking up the user's project-local CLAUDE.md /
/// GEMINI.md). `allowlist` names the env vars to inherit from the parent;
/// all `LC_*` locale vars are forwarded unconditionally.
pub(super) fn prepare_agent_cmd(cmd: &mut tokio::process::Command, allowlist: &[&str]) {
    cmd.env_clear();
    for key in allowlist {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
    for (k, v) in std::env::vars().filter(|(k, _)| k.starts_with("LC_")) {
        cmd.env(k, v);
    }
    if let Some(home) = dirs::home_dir() {
        cmd.current_dir(home.join(".memex"));
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
}

/// Classify an error message heuristically by keyword match. Used when
/// the underlying agent protocol doesn't provide a structured error kind
/// and we need to decide between `TurnOutcome::Auth` and
/// `TurnOutcome::Structured`.
pub(crate) fn classify_error_text(text: &str) -> TurnOutcome {
    let lc = text.to_ascii_lowercase();
    if lc.contains("auth")
        || lc.contains("unauthorized")
        || lc.contains("forbidden")
        || lc.contains("api key")
        || lc.contains("credentials")
        || lc.contains("login")
    {
        TurnOutcome::Auth(text.to_string())
    } else {
        TurnOutcome::Structured(text.to_string())
    }
}

/// Variant-wrapped subprocess. Each variant holds the provider-specific
/// struct defined in its own module. Dispatch methods below enable
/// protocol-agnostic use in `run_job_with_retry`.
enum Subprocess {
    Claude(claude::ClaudeSubprocess),
    Codex(codex::CodexSubprocess),
    Gemini(gemini::GeminiSubprocess),
}

impl Subprocess {
    async fn spawn(agent: Agent, cfg: &WorkerConfig) -> Result<Self> {
        Ok(match agent {
            Agent::Claude => Subprocess::Claude(claude::ClaudeSubprocess::spawn(cfg).await?),
            Agent::Codex => Subprocess::Codex(codex::CodexSubprocess::spawn(cfg).await?),
            Agent::Gemini => Subprocess::Gemini(gemini::GeminiSubprocess::spawn(cfg).await?),
        })
    }

    async fn one_turn(&mut self, prompt: &str) -> Result<TurnOutcome> {
        match self {
            Subprocess::Claude(s) => s.one_turn(prompt).await,
            Subprocess::Codex(s) => s.one_turn(prompt).await,
            Subprocess::Gemini(s) => s.one_turn(prompt).await,
        }
    }

    /// Reset state for the restart cadence. Returns `true` if the
    /// subprocess is still usable after the soft reset (codex fresh_thread,
    /// gemini fresh_session); `false` if the caller should drop and respawn
    /// on next use (claude — full subprocess restart is the documented
    /// reset semantic).
    async fn soft_reset(&mut self, cfg: &WorkerConfig) -> bool {
        match self {
            Subprocess::Claude(_) => false, // full respawn on next job
            Subprocess::Codex(s) => s.fresh_thread(cfg).await.is_ok(),
            Subprocess::Gemini(s) => s.fresh_session(cfg).await.is_ok(),
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
    live: Arc<AtomicUsize>,
    busy: Arc<AtomicUsize>,
    next_worker_id: Arc<AtomicUsize>,
}

impl WorkerPool {
    /// Build the pool's fields without spawning any worker tasks.
    fn empty(cfg: WorkerConfig) -> Self {
        let (tx, rx) = queue(cfg.max_count);
        Self {
            tx,
            rx,
            cfg,
            live: Arc::new(AtomicUsize::new(0)),
            busy: Arc::new(AtomicUsize::new(0)),
            next_worker_id: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn new(cfg: WorkerConfig) -> Self {
        let pool = Self::empty(cfg);
        pool.spawn_worker(true);
        pool
    }

    /// Enqueue a job, spawning an extra worker first if no live worker is
    /// idle (queue backlog OR all live workers currently busy) and there is
    /// headroom under `max_count`.
    pub async fn submit(
        &self,
        job: AgentJob,
    ) -> Result<(), async_channel::SendError<AgentJob>> {
        let live = self.live.load(Ordering::Acquire);
        let busy = self.busy.load(Ordering::Acquire);
        if (self.tx.len() > 0 || busy >= live) && live < self.cfg.max_count {
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
        let live = self.live.clone();
        let busy = self.busy.clone();
        tokio::spawn(async move {
            // RAII: decrement live on drop so a panic in `run()` can't
            // permanently inflate the worker count.
            let _live_guard = CountGuard(live);
            run(worker_id, rx, cfg, is_min, busy).await;
        });
    }

    /// Build a pool without spawning any worker tasks. Handler unit tests
    /// only exercise code paths that don't reach the queue.
    #[cfg(test)]
    pub fn new_inert_for_test() -> Self {
        Self::empty(WorkerConfig::default())
    }
}

async fn run(
    worker_id: usize,
    rx: JobReceiver,
    cfg: WorkerConfig,
    is_min: bool,
    busy: Arc<AtomicUsize>,
) {
    let span = tracing::info_span!("worker", id = worker_id, agent = ?cfg.agent);
    let _enter = span.enter();

    let mut subprocess: Option<Subprocess> = None;
    let mut jobs_done: u32 = 0;
    let mut cumulative_input_tokens: u64 = 0;

    let model_name = cfg.model.as_deref().unwrap_or_else(|| cfg.agent.default_model());
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
        let context_hit = cumulative_input_tokens >= context_threshold;
        if count_hit || context_hit {
            if let Some(sp) = subprocess.as_mut() {
                let trigger = if context_hit { "context" } else { "count" };
                tracing::info!(
                    cumulative_input_tokens,
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
            cumulative_input_tokens = 0;
        }

        let (outcome, input_tokens) = run_job_with_retry(&mut subprocess, &cfg, &job).await;

        // Count any turn that actually ran to a terminal event. Only skip
        // Crash / Timeout — the subprocess didn't complete a turn. New
        // WorkerError variants default to "counted" rather than silently
        // undercounting.
        let counted = !matches!(
            &outcome,
            JobOutcome::Synth(Err(WorkerError::Crash(_) | WorkerError::Timeout))
                | JobOutcome::Expand(Err(WorkerError::Crash(_) | WorkerError::Timeout))
        );
        if counted {
            jobs_done = jobs_done.saturating_add(1);
        }
        cumulative_input_tokens = cumulative_input_tokens.saturating_add(input_tokens);

        // Deliver reply on the matching variant. The type invariant is that
        // run_job_with_retry's outcome variant matches the job's variant
        // (the prompt construction branch drives both), so the catch-all
        // should be unreachable. If it ever fires — e.g., a future editor
        // adds a new AgentJob variant without extending run_job_with_retry
        // — log and drop the reply rather than panicking the worker (which
        // would kill the min worker and force a cold respawn).
        match (job, outcome) {
            (AgentJob::Synth(j), JobOutcome::Synth(r)) => {
                let _ = j.reply.send(r);
            }
            (AgentJob::Expand(j), JobOutcome::Expand(r)) => {
                let _ = j.reply.send(r);
            }
            _ => {
                tracing::error!("internal type mismatch between AgentJob and JobOutcome");
                debug_assert!(false, "job type mismatch");
            }
        }
        // _busy_guard drops here, decrementing busy.
    }
}

/// Atomic counter guard: decrements on drop. Used to keep `WorkerPool`'s
/// `live` and `busy` counters correct across normal exit, idle reap, and
/// panic within the worker task.
struct CountGuard(Arc<AtomicUsize>);

impl CountGuard {
    /// Increment `counter` now; the returned guard decrements on drop.
    fn inc(counter: &Arc<AtomicUsize>) -> Self {
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
    job: &AgentJob,
) -> (JobOutcome, u64) {
    let (prompt, is_expand) = match job {
        AgentJob::Synth(j) => (
            format!(
                "[TASK: SYNTHESIZE]\n\n{}\n\nQuestion: {}\n",
                j.context, j.question
            ),
            false,
        ),
        AgentJob::Expand(j) => (
            format!("[TASK: EXPAND]\n\nQuestion: {}\n", j.question),
            true,
        ),
    };

    let timeout = std::time::Duration::from_secs(cfg.timeout_sec);
    let mut last_err: WorkerError = WorkerError::Crash("no attempt completed".into());

    // Wrap a provider-shape result as the job-shape JobOutcome variant.
    let as_err = |e: WorkerError| {
        if is_expand {
            JobOutcome::Expand(Err(e))
        } else {
            JobOutcome::Synth(Err(e))
        }
    };

    for attempt in 0..2u32 {
        if subprocess.is_none() {
            match Subprocess::spawn(cfg.agent, cfg).await {
                Ok(s) => *subprocess = Some(s),
                Err(e) => {
                    tracing::warn!(attempt, ?e, "spawn failed");
                    last_err = WorkerError::Crash(format!("spawn failed: {e}"));
                    continue;
                }
            }
        }
        let sp = subprocess.as_mut().expect("spawned above");
        let turn = tokio::time::timeout(timeout, sp.one_turn(&prompt)).await;

        match turn {
            Ok(Ok(TurnOutcome::Ok { text, input_tokens })) => {
                let outcome = if is_expand {
                    JobOutcome::Expand(parse::parse_expand(&text).map_err(|raw| {
                        tracing::warn!(raw = %raw, "expand reply didn't parse");
                        WorkerError::AgentError(raw)
                    }))
                } else {
                    JobOutcome::Synth(parse::parse_reply(&text).map_err(|raw| {
                        tracing::warn!(raw = %raw, "synth reply didn't parse");
                        WorkerError::AgentError(raw)
                    }))
                };
                return (outcome, input_tokens);
            }
            Ok(Ok(TurnOutcome::Auth(text))) => {
                tracing::warn!(text = %text, "auth failed");
                // Reset protocol state: the subprocess may have buffered
                // notifications for the errored turn that would otherwise
                // arrive as stale IDs on the next turn. For claude,
                // soft_reset returns false and the next job respawns.
                if let Some(sp) = subprocess.as_mut()
                    && !sp.soft_reset(cfg).await
                {
                    *subprocess = None;
                }
                return (as_err(WorkerError::AuthFailed(text)), 0);
            }
            Ok(Ok(TurnOutcome::Structured(text))) => {
                tracing::warn!(text = %text, "structured error");
                if let Some(sp) = subprocess.as_mut()
                    && !sp.soft_reset(cfg).await
                {
                    *subprocess = None;
                }
                return (as_err(WorkerError::AgentError(text)), 0);
            }
            Ok(Err(e)) => {
                tracing::warn!(attempt, ?e, "turn crash; retrying");
                last_err = WorkerError::Crash(e.to_string());
                *subprocess = None;
            }
            Err(_elapsed) => {
                tracing::warn!(attempt, "turn timed out; retrying");
                last_err = WorkerError::Timeout;
                *subprocess = None;
            }
        }
    }
    (as_err(last_err), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin each keyword in the union to prevent silent regressions if
    /// someone later trims the list thinking a keyword is unused.
    /// Each keyword exists because at least one provider needs it.
    #[test]
    fn classify_error_text_recognizes_every_keyword() {
        // codex + gemini: generic auth errors
        for kw in ["authentication", "unauthorized", "forbidden", "api key"] {
            let out = classify_error_text(&format!("turn failed: {kw} problem"));
            assert!(
                matches!(out, TurnOutcome::Auth(_)),
                "expected Auth for keyword {kw:?}, got {out:?}"
            );
        }
        // codex-specific: "login" (e.g., "Not logged in · Please run /login")
        assert!(matches!(
            classify_error_text("please login and retry"),
            TurnOutcome::Auth(_)
        ));
        // gemini-specific: "credentials"
        assert!(matches!(
            classify_error_text("invalid credentials"),
            TurnOutcome::Auth(_)
        ));
    }

    #[test]
    fn classify_error_text_defaults_to_structured() {
        assert!(matches!(
            classify_error_text("overloaded, try again later"),
            TurnOutcome::Structured(_)
        ));
        assert!(matches!(
            classify_error_text("rate limit exceeded"),
            TurnOutcome::Structured(_)
        ));
    }

    #[test]
    fn classify_error_text_is_case_insensitive() {
        assert!(matches!(
            classify_error_text("AUTHENTICATION FAILED"),
            TurnOutcome::Auth(_)
        ));
    }

    #[test]
    fn context_threshold_resolves_for_each_agent() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Gemini] {
            let name = agent.default_model();
            let info = memex_core::model::lookup_model(name);
            assert!(
                info.max_input_tokens > 0,
                "expected non-zero max_input_tokens for {name}"
            );
        }
    }
}
