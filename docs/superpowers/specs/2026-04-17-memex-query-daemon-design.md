# memex query: thin-wrapper CLI with daemon-backed synthesis

**Status**: Draft
**Date**: 2026-04-17
**Topic**: Design 3b — `memex query` CLI subcommand that delegates retrieval + synthesis to a background daemon holding a persistent agent subprocess (claude / codex app-server / gemini --acp)

> **Scope expansion notice (2026-04-19):**
> The non-goals "Routing write/delete/lint through the daemon" and "Changing the ingest path" are un-deferred by `2026-04-19-daemon-ingestion-design.md`. The daemon is expanded from read-only query engine to the single mutating engine. All write operations (`write`, `delete`, `lint --fix`, `ingest`) now route through the daemon by default. The daemon also handles session transcript ingestion with an Extract + Merge LLM pipeline (1-2 calls per session). See the 04-19 spec for details.

## Context and motivation

`memex` currently exposes primitive CLI commands (`search`, `read`, `write`, `lint`, `delete`). The `memex-query` skill orchestrates retrieval and synthesis by having the agent issue multiple `memex search` / `memex read` calls from Bash, then synthesize an answer. This architecture has two practical problems when used by an agent:

1. **Main-context bloat.** Search output snippets and full page bodies accumulate in the agent's context window. At ~10k tokens per query, the agent's context overflows after ~20 queries.
2. **Per-call subprocess cost.** Every `memex search` reloads the ONNX embedding model (~500ms to several seconds). The agent pays this on every search turn.

This spec proposes a thin-wrapper architecture: one CLI command `memex query <question>` that does retrieval and synthesis internally and returns a synthesized answer. LLM work (query expansion when signal is weak, then final synthesis) runs in a persistent, minimized agent subprocess (claude / codex app-server / gemini --acp, selected via config) managed by a background daemon.

### Non-goals

- Replacing the existing `memex search` / `memex read` primitives. They remain as building blocks and as the escape hatch for agents that want raw data.
- Routing `search`/`read`/`write`/`lint`/`delete` through the daemon. These commands stay stateless (in-process ONNX + DB) as today. Daemon involvement is scoped to `memex query` because that's the path that benefits from the persistent agent subprocess and warm retrieval state. Routing other commands through the daemon is a possible future optimization that introduces its own cache-coherence questions and is deferred.
- Becoming an MCP server. That is a separate architectural choice (design 4) and can be added later.
- Changing the ingest path. Ingestion continues as today.
- Changing the parallel-access locking contract. The daemon inherits it (see below).

### Relationship to `2026-04-16-parallel-access-design.md`

This design introduces a **second** flock, distinct from the one parallel-access landed. The two serve different domains:

- **Lock 1 — wiki writer flock** (`{memex_root}/.lock`, parallel-access spec). Protects data integrity on writes. Held briefly per-operation (one `memex write` / `lint --fix` / `delete`). Readers bypass via WAL snapshots.
- **Lock 2 — daemon lifetime flock** (`~/.memex/daemon.lock`, this spec). Encodes process identity: "I am THE daemon for this user." Held for the daemon's entire runtime. Released automatically by the OS on exit or crash — making it the authoritative "daemon is alive" signal.

The daemon is a **pure reader** of wiki state: retrieval uses SQLite `SELECT` + filesystem `read` only. Per parallel-access, readers bypass Lock 1, so the daemon never acquires Lock 1 and never blocks user `memex write` / `memex lint --fix` calls in another terminal. A concurrent `memex write` can land new content mid-query; WAL snapshots ensure the daemon sees a consistent view for the duration of one retrieval.

The two locks never interact. They are in different directories (`{memex_root}/.lock` vs `~/.memex/daemon.lock`), protect different invariants (data vs process), and have different hold durations (per-op vs lifetime).

## Architecture overview

```
┌────────────────────────────────────────────────┐
│  Agent (Claude Code, Codex, Gemini, etc.)      │
│                                                │
│  runs: memex query "<question>" [--raw]        │
└─────────────────┬──────────────────────────────┘
                  │ Bash tool invocation
                  ▼
┌────────────────────────────────────────────────┐
│  memex query CLI                               │
│  1. Connect to daemon (start if absent)        │
│  2. Send {query, flags} over Unix socket       │
│  3. Stream response, print to stdout           │
└─────────────────┬──────────────────────────────┘
                  │ ~/.memex/daemon.sock
                  ▼
┌────────────────────────────────────────────────┐
│  memex daemon (one process, multi-project)     │
│                                                │
│  socket accept ─► connection handler (per req) │
│         │                                      │
│         │ retrieval_req                        │
│         ▼                                      │
│  ┌──────────────────────────────────────────┐  │
│  │ Retrieval worker (Actor: owns ONNX + DB) │  │
│  │ - eager-loaded ONNX (at daemon start)    │  │
│  │ - per-request SQLite open(memex_root)    │  │
│  │ - probe BM25 + vector + RRF fusion       │  │
│  │ - read top-K pages, format context       │  │
│  └──────────────────┬───────────────────────┘  │
│                     │ SynthJob                 │
│                     ▼                          │
│  ╔══════════════════════════════════════════╗  │
│  ║  Central agent job queue (MPMC, bounded) ║  │
│  ║  async-channel; sender cloned by         ║  │
│  ║  handlers, receiver cloned by workers    ║  │
│  ╚══════════════════════════════════════════╝  │
│                     │ rx.recv()                │
│   ┌─────────────────┼──────────────────┐       │
│   ▼                 ▼                  ▼       │
│  ┌────────┐     ┌────────┐     ┌────────┐      │
│  │ synth  │     │ synth  │     │ synth  │      │
│  │worker 1│     │worker 2│ ... │worker N│      │
│  │ agent  │     │ agent  │     │ agent  │      │
│  │subproc │     │subproc │     │subproc │      │
│  └────┬───┘     └────┬───┘     └────┬───┘      │
│       │              │              │          │
│       └──────┬───────┴───────┬──────┘          │
│              ▼               ▼                 │
│        result → reply_tx → handler → CLI       │
│                                                │
│  Worker restart: between jobs when counter     │
│  hits N, or on EOF / non-zero exit / auth      │
│  error. Failed jobs get ONE automatic retry    │
│  on the restarted worker.                      │
└────────────────────────────────────────────────┘
```

## Components

### 1. `memex query` subcommand (CLI)

New Rust subcommand in `cli/src/main.rs`.

**Signature:**

```
memex query <question> [--raw] [--top-k N]
```

- `<question>`: free-form user question (required, positional)
- `--raw`: return structured retrieval data instead of synthesized answer. Off by default.
- `--top-k N`: how many pages to retrieve. Default 5.

**Behavior:**

1. Resolve `MEMEX_ROOT` from env or default (`~/.memex`).
2. Try to connect to the daemon socket at `~/.memex/daemon.sock`.
   - **Connect succeeds** → proceed to step 3.
   - **Connect fails** → fork a detached daemon child (`memex daemon start`), then retry connect in a loop (up to ~2s total, 50ms between attempts) until success or timeout. The daemon's own lifetime flock (see Daemon section) handles all race resolution, so the CLI never needs its own lock.
3. Send JSON request over the socket:
   ```json
   {"op": "query", "v": 1, "question": "...", "raw": bool, "top_k": N, "memex_root": "/abs/path"}
   ```
4. Consume response stream until `done`. Print the human-readable payload to stdout:
   - Synthesized mode: print the `answer` text.
   - `--raw` mode: print the formatted `context` block.
   - If the stream includes a `queued` event (job sat in queue > 500ms before pickup), print a one-line status to stderr: `queued: N ahead`.
   Exit with the daemon's reported status code (per the Exit code contract).

**Client-side responsibilities:**

- Daemon auto-start and connection retry
- stdout/stderr passthrough
- Clean exit handling

### 2. memex daemon

**One binary, multiple subcommands.** The daemon is not a separate binary. The same `memex` binary is invoked in two roles:

- `memex query "..."` — **client** role: CLI path that talks to the daemon over the socket.
- `memex daemon [start|stop|status]` — **daemon** role: normally auto-spawned (detached child of `memex query`) but exposed as a user-facing subcommand for manual lifecycle and debugging.

Auto-spawn re-execs the same binary with `memex daemon start` and detaches. There is no `memexd`. This keeps distribution (one artifact), the config story (one `~/.memex/config.toml`), and the exit-code contract (see below) unified across all roles.

**Lifecycle:**

- Started on-demand by the first `memex query` invocation (or explicitly via `memex daemon start`).
- **Single daemon per user, multi-project**: one Unix socket at `~/.memex/daemon.sock`; the daemon serves any `MEMEX_ROOT` passed in the request. The retrieval actor opens the SQLite DB for that root on demand (~5ms, cheap).
- **Lifetime ownership flock** at `~/.memex/daemon.lock`. The daemon's very first startup step is `flock(daemon.lock, LOCK_EX | LOCK_NB)`.
  - Acquired → this process is THE daemon. Holds the flock for the rest of its lifetime (until exit/crash, at which point the OS releases it automatically).
  - Busy → another daemon is already running. Exit cleanly (exit 0). The caller that spawned us can connect to the running daemon.
  - The flock is the authoritative "daemon is alive" signal. PID file is still written for observability, but liveness is determined by the flock.
- **Socket setup (after flock acquired)**: unlink any stale `~/.memex/daemon.sock` (safe because we now hold the ownership flock — no race with another daemon), then `bind()`.
- **Eager ONNX load at startup**: the daemon doesn't accept connections until the ONNX embedding model is loaded. Moves the ~1-2s embedding-model load off the user's first-query critical path.
- Writes PID to `~/.memex/daemon.pid` after binding; removes it on clean shutdown.
- Idle timeout: exit after N minutes (default 15) of no connections. Clean exit releases the flock.
- Graceful shutdown on SIGTERM: stop accepting new connections, drain queue, wait for workers to finish current jobs (up to 60s), kill subprocesses, exit (OS releases flock).
- Crash (SIGKILL, panic, OOM): OS releases the flock; next daemon to try startup will acquire it and unlink the stale socket.
- Structured logging via the `tracing` crate; rotating file appender, JSON format. Events: spawn/shutdown, lock acquire/release, queue depth threshold, worker restart (counter/crash/auth), subprocess timeouts, auth errors, config parse errors.

**Startup strategies:**

Three non-exclusive ways the daemon gets running. All converge on the same daemon process (the lifetime flock enforces single-instance).

1. **On-demand (default, always available).** The `memex query` CLI spawns the daemon if none is running. First query of a cold session pays ~8-10s. No user setup required. Works for all agents, CLI scripts, cron jobs, benchmark harnesses — anything that can invoke `memex query`.

2. **Agent `SessionStart` hook (opt-in, cross-agent).** Register a hook that runs `memex daemon start` when the user's agent session begins. The daemon's own lifetime flock makes this safe to call repeatedly; if a daemon's already running, `memex daemon start` exits 0 cleanly. Claude Code, Codex CLI, and Gemini CLI all support `SessionStart` hooks with almost-identical JSON shape. The hook shipped with memex's plugin (`plugin/hooks/`) targets all three:

   - **Claude Code** — `plugin/hooks/claude-code.json` (or its plugin-declaration equivalent):
     ```json
     {"hooks":{"SessionStart":[{"matcher":"startup|resume|clear",
       "hooks":[{"type":"command","command":"memex daemon start"}]}]}}
     ```
   - **Codex CLI** — `~/.codex/hooks.json` (requires `[features] codex_hooks = true` in `~/.codex/config.toml`):
     ```json
     {"hooks":{"SessionStart":[{"matcher":"startup|resume",
       "hooks":[{"type":"command","command":"memex daemon start"}]}]}}
     ```
   - **Gemini CLI** — `~/.gemini/settings.json` under `hooks` (stable, on by default):
     ```json
     {"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"memex daemon start","name":"memex-daemon"}]}]}}
     ```

   With any of these registered, opening a new agent session pre-warms the daemon. First `memex query` of the session is indistinguishable from a warm query.

**Recommendation order for users:**
- Default: strategy (1) alone — zero setup, cold start on first query acceptable for most users.
- Agent users: add strategy (2) — five lines of JSON per agent, eliminates first-query cold start.

**Request handling:**

A query is a small state machine: retrieve → optionally expand → synthesize. The daemon orchestrates; worker subprocesses contribute single LLM turns as discrete jobs.

1. Validate: `op == "query"`, `v == 1` (supported). Unknown `op` → `bad_request` error. Unknown `v` → `version_mismatch` error with `supported: [1]`. Malformed JSON → `bad_request`.
2. **Retrieval phase** (retrieval actor):
   - Actor opens SQLite for the requested `memex_root` (reuses a recent open if same root).
   - Probe search (BM25 + vector with RRF fusion). Tags the result with `signal: strong` or `signal: weak` based on the BM25 top-1 vs top-2 gap.
   - Auto-read the top-K results across doc_types (wiki + source). If no results at all, respond with `retrieval_empty` error.
3. **Expansion phase** (only when `signal: weak`):
   - Daemon enqueues an `ExpandJob{question, reply_tx}` onto the central agent job queue.
   - A worker picks it up and sends a user message tagged `[TASK: EXPAND]` (the agent-prompt system prompt instructs the agent to recognize this tag and produce typed rewrite terms). One LLM turn generates: one short lexical variant, one semantic reformulation, one single-sentence hypothetical answer (HyDE).
   - Worker replies with `{"lex": "...", "vec": "...", "hyde": "..."}` and is released back to the pool.
   - Retrieval actor re-runs the search with those terms appended (equivalent to `memex search "<q>" --lex "..." --vec "..." --hyde "..."`), reads the new top-K, re-formats the context block.
   - If `signal: strong` on the probe, expansion is skipped — the probe's top-K is used as-is.
4. **Context formatting**: each page section is tagged with its **rank** (1-based) and the post-expansion signal strength, so the synthesizer can weight higher-ranked results more and treat weak-signal results with appropriate skepticism. Example:

   ```
   ## [rank 1, wiki, signal strong] auth-migration-timeline
   <body>
   ```
5. **Synthesis phase**:
   - Daemon enqueues a `SynthJob{context, question, reply_tx, start_instant}` onto the central agent job queue. (This is a **separate checkout** from the expansion job — different worker, or same worker re-used, but always via a fresh pool checkout.)
   - If the job sits in the queue > 500ms with no worker, handler emits `{"type": "queued", "ahead": <N>}` to the client.
   - A worker pulls the job, sends a user message tagged `[TASK: SYNTHESIZE]` containing the context block + question, and invokes its agent subprocess via the provider's persistent protocol (stream-json stdin for claude; `turn/start` on the codex app-server's open thread; `session/prompt` on the gemini ACP session). The agent replies with `{"answer": "...", "citations": [...]}`, worker is released back.
   - On worker subprocess crash / auth error / timeout: the worker respawns its agent subprocess and retries THIS job once. If the retry also fails, surface a typed error (`subprocess_crashed` / `auth_failed` / `subprocess_timeout`).
   - Connection handler awaits on `reply_tx`, surfaces result to CLI, releases.
6. If `--raw` mode: skip synthesis; return the formatted context block from step 4 directly. Expansion (step 3) still runs on weak-signal probes so the returned context reflects the full retrieval pipeline.

**Expansion and synthesis use the same worker pool.** There's no separate "expansion pool" — both job types (`ExpandJob`, `SynthJob`) flow through the same MPMC queue. Each job is a single LLM turn, one worker checkout. A worker's restart counter increments per job, regardless of type. This keeps the pool model simple: one worker type, one queue, two job shapes.

**Worker-selection strategy for expansion vs synth**: the current design uses any free worker for either job type. If future profiling shows expansion is noticeably cheaper (shorter responses) and worth a dedicated cheaper-model pool, we can split later. For now, uniform pool.

**Retrieval actor:**

One dedicated background task owns the ONNX embedding session and the current SQLite connection. Connection handlers send `RetrievalReq{memex_root, question, top_k}` via a channel and await a reply. Rationale: ONNX sessions are not always thread-safe across the ort crate's version boundaries, and serializing embeddings is fine because embedding is 50-100ms and SQLite lookup dominates. Memory stays bounded (one ONNX copy regardless of worker count). The actor caches the most recent `Memex` handle per `memex_root` to avoid re-opening on every request within the same project.

The actor opens the wiki via `memex_core::Memex::open(root)` — the **reader** constructor introduced by the parallel-access design (no writer flock, WAL snapshot reads). Never via `open_writer`; the daemon never mutates state.

**Synthesis worker pool + central queue:**

The daemon runs a single in-memory FIFO queue fed by connection handlers and drained by a pool of agent-subprocess workers (claude, codex, or gemini depending on `daemon.worker.backend`). Because multiple workers share the same queue, this is an **MPMC** (multi-producer, multi-consumer) channel — `tokio::mpsc` won't work here (single-consumer only); use `async-channel` (`async_channel::bounded`) or `flume::bounded` which expose cloneable receivers.

- **Queue**: `async_channel::bounded::<BackendJob>(cap)` with capacity `max(workers × 4, 32)`, where `AgentJob` is a union enum holding either `ExpandJob` or `SynthJob`. Both the sender and receiver handles are cloneable; producers clone the sender, each worker holds a clone of the receiver. When full, `send().await` applies backpressure (blocks the handler, which blocks the CLI, making load visible rather than silently OOM-ing).
- **Workers**: each runs a loop `while let Ok(job) = rx.recv().await { serve(job) }`. All workers race on the same receiver; the channel ensures each job is delivered to exactly one worker. FIFO ordering is preserved at the queue; fairness across workers is an async-channel property.
- **Pool size**: configurable via `~/.memex/config.toml` (`daemon.worker.max_count`) or `MEMEX__DAEMON__WORKER__MAX_COUNT`; default is `num_cpus::get()`. Tune downward for memory-constrained hosts or when API rate limits would throttle.
- **Lazy spawning + autoscale**: one "min" worker spawns eagerly; extra workers spawn on demand, up to `daemon.worker.max_count`. The scale-up signal at `submit()` time is `tx.len() > 0 || busy >= live` — i.e., either queued jobs have no picker, or every live worker is mid-job. `busy` is an atomic counter incremented when a worker receives a job and decremented when it finishes, so the signal catches "all workers occupied" even when the channel is empty. When the signal fires and `live < max_count`, the pool CAS-increments `live` and spawns a non-min worker. Non-min workers exit after `daemon.worker.idle_reap_sec` (default 600) with no job. The min worker tokio task stays alive until the daemon exits, but its subprocess is recycled by the same count/context/crash triggers as any non-min worker.
- **Cancellation**: if a CLI disconnects before its job reaches a worker, the handler drops `reply_tx`; the worker that picks up the job still runs the job (can't cancel the agent subprocess mid-turn cheaply) but discards the result on `send` to a closed channel. Cheap enough.
- **Per-worker counter**: applies to all three providers (all three are persistent-warm). Each worker tracks completed jobs; when the counter hits the restart threshold, the worker performs a provider-specific reset between jobs (after the Nth completes, before picking up the N+1st). Never mid-job. Resets by provider: claude restarts the whole `claude -p` subprocess; codex issues a fresh `thread/start` (optionally preceded by `thread/compact/start` if only partial reset is needed); gemini issues a fresh `session/new` (closing the previous session first). In-flight waiters unaffected because they're on other workers (or still in the queue).
- **Retry policy on failure**: if a worker's agent subprocess crashes, times out (60s default), or returns an auth error, the worker respawns the subprocess and **retries THIS job once** on the fresh subprocess. If the retry also fails, the worker sends a typed error back via `reply_tx` and continues. This handles transient 503s, rotated OAuth tokens, and hung subprocesses transparently.

**Benefits of the central queue vs. ad-hoc "block on semaphore" approaches:**

- FIFO fairness (first-come first-served, not scheduler-dependent)
- Observable depth: `rx.len()` is a metric
- Bounded capacity provides explicit backpressure; avoids unbounded connection-handler growth
- Workers scale independently of request arrival; changing pool size is a config tweak, not a routing rewrite

**Worker subprocess command** — the daemon supports three agent providers via `worker.agent = "claude" | "codex" | "gemini"`. Each uses a persistent-warm worker pattern — no provider is allowed to fall back to fresh-per-job, which is too bloated once persistent options exist. Claude uses `stream-json` stdin (native multi-turn). Codex uses `app-server` (JSON-RPC 2.0 with a thread/turn model; OpenAI prompt cache amortizes tokens). Gemini uses `--acp` (Zed's Agent Communication Protocol; JSON-RPC 2.0 with a session/prompt model; amortizes process-spawn latency if not tokens). All three run through the same MPMC queue and retrieval pipeline. See the per-provider notes for specifics.

### `claude` (persistent, recommended)

```
claude -p --input-format stream-json --output-format stream-json \
  --model sonnet \
  --system-prompt <agent-prompt> \
  --disable-slash-commands \
  --tools "" \
  --setting-sources "" \
  --mcp-config '{"mcpServers": {}}' \
  --strict-mcp-config \
  --no-session-persistence \
  --dangerously-skip-permissions
```

Every flag above is there to strip context the subprocess would otherwise inherit. Empirically measured on a dev machine with a typical Claude Code install:

| Configuration | Input tokens |
|---|---|
| Baseline `claude -p "say OK"` | **36,041** |
| + `--setting-sources ""` | **2,219** (hooks / user settings stripped) |
| + `--mcp-config '{"mcpServers":{}}' --strict-mcp-config` | **363** (MCP server schemas stripped) |

Two biggest-win flags:

- **`--setting-sources ""`** is the largest single reduction — it turns off user-level `SessionStart` hooks, which typically inject several thousand tokens of skill content into the first user message.
- **`--mcp-config '{"mcpServers":{}}' --strict-mcp-config`** removes user MCP server tool schemas from the system prompt (~1,900 additional tokens on a machine with plugins installed).

Stripped to these ~363 tokens of Claude Code framework boilerplate plus our ~500-700 token agent prompt, every Claude call starts with **~1k tokens of preamble** instead of 36k. Leaves the context budget for the retrieved pages and the answer.

**Context-stripping rationale (flag by flag):**

| Flag | What it strips |
|---|---|
| `--system-prompt <agent-prompt>` | Replaces Claude Code's default ~6k-token system prompt with our ~500-700 token one. |
| `--disable-slash-commands` | Skills (built-in and plugin) don't get loaded into the prompt. |
| `--tools ""` | No tool schemas injected into the system prompt. Synthesis doesn't need tools. |
| `--setting-sources ""` | User/project/local settings are not loaded — **critically, this disables user-level hooks** (like `SessionStart`), which can inject thousands of tokens of skill content into the first user message. Biggest single reduction for users with superpowers / plugin hooks installed. |
| `--mcp-config '{"mcpServers": {}}' --strict-mcp-config` | User's configured MCP servers (and their tool schemas) are ignored. Without this, MCP tools from the user's main Claude Code config add ~2k tokens to the system prompt. |
| `--no-session-persistence` | Skip saving the conversation; no session-loading overhead on next spawn. |
| `--dangerously-skip-permissions` | Skip permission prompts. Safe because `--tools ""` means the model has no tools to invoke anyway. |

**Subprocess environment sanitation** — the daemon passes only the minimum env vars when spawning the worker:

- **Passed**: `HOME` (for `~/.claude/.credentials.json` OAuth read), `PATH` (to find the `claude` binary), `LANG` / `LC_*` (character encoding), `CLAUDE_CODE_OAUTH_TOKEN` if present (auth fallback), `ANTHROPIC_MODEL` if set (model override, defensive).
- **Stripped**: everything else. Specifically, `MEMEX_*` vars (irrelevant to the agent's work), `CLAUDE_*` vars other than the auth token (framework context leaks), shell-specific (`PS1`, `SHELL`, `HISTFILE`), editor-specific (`EDITOR`, `VISUAL`), and TMUX / IDE-specific (`TMUX`, `ITERM_SESSION_ID`, `VSCODE_*`) — Claude Code reads several of these to tailor its behavior and they bloat context with no benefit to expansion or synthesis.

**Subprocess cwd**: set to `~/.memex/` (daemon's own data dir), **not** the user's current project. Claude Code auto-discovers `CLAUDE.md` in the cwd; setting cwd to a neutral, project-free directory prevents an accidental 10k-token context dump from the user's project CLAUDE.md.

**Other parameters:**

- Default Claude model is **Sonnet** (mid-tier). Rationale: synthesis must read multiple ranked sources, weight them by rank, reconcile contradictions, and produce a cited answer. This is reasoning-heavy; Haiku was empirically less reliable on early benchmarks despite being cheaper/faster. Escalation path: `opus` (flagship) via `daemon.worker.model` if Sonnet proves insufficient. Mid-tier is the parity choice across all three providers (see cross-provider table below).
- Restart cadence: every 15 queries per worker (empirically ~150k token context growth per worker — leaves margin before 200k limit). Restart is always between-jobs (never mid-job).
- Restart triggers: query-count threshold, subprocess EOF, subprocess non-zero exit, auth error, subprocess timeout.
- Worker instructions are a **compiled-in string constant** in the daemon binary (`const WORKER_PROMPT: &str = include_str!("prompt.txt")`). Ensures the prompt matches the code's assumptions about context format. Changes require a rebuild. No runtime override in this iteration; a `MEMEX_AGENT_PROMPT_FILE` override can be added later if demand appears.
- The prompt (~500-700 tokens) instructs the agent on **both** roles it plays per job:
  - **Expansion job** (user message tagged `[TASK: EXPAND]`, see request handling): produce typed rewrite terms — one lexical variant, one semantic reformulation, one HyDE-style hypothetical answer — as a JSON object.
  - **Synthesis job** (user message tagged `[TASK: SYNTHESIZE]`): read the ranked context, weight higher ranks more, treat weak-signal top results with skepticism, prefer higher-ranked sources when facts conflict, cite with `[[page-stem]]` links, and say "the wiki doesn't have this information" if no page answers the question.
  - The agent determines its role from the tag in the user message; same compiled-in system prompt handles both.

**Implementation note**: the final flag set should be verified empirically. Run `claude -p --output-format json <prompt>` with the above flags, inspect `usage.input_tokens + cache_read_input_tokens + cache_creation_input_tokens`, and confirm the total is at or below **~1k tokens** before the context block and user's question are added (measured: 363 Claude Code boilerplate + ~500-700 agent prompt). If higher, investigate which flag isn't taking effect — most common culprit is a user-level `SessionStart` hook still firing despite `--setting-sources ""` (regression in Claude Code, or setting-sources not accepting empty string on the installed version — fall back to `--setting-sources project`).

### `codex` (persistent via app-server)

Verified against codex CLI v0.118.0. Codex's baseline scaffolding is heavy (~21k uncached tokens on a fresh `exec` invocation — ~21× Claude's ~1k warm baseline), which rules out fresh-per-job as a viable daemon pattern.

**`codex app-server`** (JSON-RPC 2.0 persistent server; marked `[experimental]` but functional) is the only supported path. After the first turn in a thread, OpenAI's prompt cache hits the static prefix (baseInstructions + scaffolding), so subsequent turns pay only the delta (new context block + new question) in uncached input. Empirical measurements below.

**Launch command** — daemon spawns one `codex app-server` subprocess per worker:

```
codex app-server \
  --listen stdio:// \
  --disable plugins \
  --disable codex_hooks
```

Per-flag rationale (launch time):

| Flag | What it does |
|---|---|
| `app-server` | Run codex as a JSON-RPC 2.0 server over stdio. Per-session config (model, sandbox, baseInstructions) is set via `thread/start` params, not CLI flags. |
| `--listen stdio://` | Transport: stdio. Alternative `ws://IP:PORT` exists for remote but not used here. |
| `--disable plugins` | Disable the `plugins` feature; **critical** — without it, the user's `~/.codex/superpowers/` (or other plugins) auto-injects SKILL.md content and inflates per-turn input by ~30k+ tokens. |
| `--disable codex_hooks` | Belt-and-suspenders; `codex_hooks` is nominally off by default but we pin it. |

Per-session config (passed in `thread/start` params, not CLI flags):

| Param | Value | Rationale |
|---|---|---|
| `baseInstructions` | agent prompt | Codex's equivalent of system prompt. No dedicated `--system-prompt` CLI flag. |
| `ephemeral` | `true` | Don't persist session to `~/.codex/sessions/`. No cross-query state on disk. |
| `sandbox` | `"read-only"` | Sandbox: read-only file access. Prevents shell/file writes. |
| `model` | `"gpt-5.4-mini"` | Mid-tier ChatGPT-auth-compatible model (verified against `~/.codex/models_cache.json`), chosen for cost parity with Claude's Sonnet default. Escalation path: `"gpt-5.4"` (flagship) if mini proves insufficient. Note: `gpt-5-codex` / `gpt-5` / `gpt-5-mini` are **API-only**, rejected on ChatGPT auth. `gpt-5.3-codex` is ChatGPT-auth-compatible but superseded — `models_cache.json` marks it with `upgrade.model = "gpt-5.4"`; not recommended for new installs. |
| `approvalPolicy` | `"never"` | No human-in-the-loop approvals; daemon runs autonomously. |

Protocol sequence per worker (JSON-RPC 2.0, newline-delimited JSON over stdio):

| Step | Method | Params | Response |
|---|---|---|---|
| 1 | `initialize` | `clientInfo: {name, version}` | server info (`userAgent`, `codexHome`, `platformOs`) |
| 2 | `thread/start` | `baseInstructions` (agent prompt), `ephemeral: true`, `sandbox: "read-only"`, `model: "gpt-5.4-mini"`, `approvalPolicy: "never"` | `thread.id` |
| 3 | `turn/start` | `threadId`, `input: [{type: "text", text: "<context + question>"}]` | notifications stream (below), then `turn/completed` |
| 4+ | repeat step 3 for each job; `thread/compact/start` when context exceeds threshold; full re-`thread/start` when compact isn't enough |

Streaming notifications during a turn:
- `thread/status/changed`, `turn/started`
- `item/started` / `item/agentMessage/delta` (streaming assistant output) / `item/completed`
- `thread/tokenUsage/updated` (authoritative usage; `last.{inputTokens, cachedInputTokens, outputTokens, reasoningOutputTokens, totalTokens}` and `total.{...}` cumulative)
- `account/rateLimits/updated`
- `turn/completed` (terminal; check `turn.status == "completed"` vs `"failed"`)

**Empirical usage** (one thread, two turns, `gpt-5.4-mini`, minimal prompts):

| | `inputTokens` | `cachedInputTokens` | Uncached delta |
|---|---:|---:|---:|
| Turn 1 (cold) | 21,160 | 2,432 | ~18,728 |
| Turn 2 (warm) | 21,178 | 20,864 | **~314** |

Turn 2's uncached delta (~314 tokens) reflects just the new user input plus turn-marker tokens; prior turn content is served from OpenAI's prompt cache.

**Projected 15-query thread with realistic ~2k-per-turn context**: ~18.7k (turn 1 cold) + 14 × ~2.3k (turns 2–15, where the ~2k context block is fresh plus ~0.3k gap) ≈ **~51k uncached total**. This is substantially higher than Claude's warm path (~1k per turn) but meaningfully lower than fresh-per-job `codex exec` (~315k for 15 calls) — app-server is the only codex path worth supporting.

**Caveats:**
- Marked `[experimental]` in the codex CLI (v0.118.0). Protocol may change across codex versions. Pin behavior tests against the JSON-RPC methods we use; surface a clear error if `initialize` or `thread/start` fails. If the user's codex version doesn't support `app-server`, daemon reports `agent_unavailable` and user must switch `daemon.worker.backend` (no fresh-exec fallback).
- OpenAI's prompt cache has a TTL (~5–10 min, per OpenAI docs). Sparse traffic causes re-paying the ~18.7k cold cost when the cache expires. The amortization assumes steady queries.
- Thread context grows with each turn's assistant output. Primary mitigation: context-based restart (~70% of `lookup_model(model).max_input_tokens`) tracks cumulative uncached input tokens from `thread/tokenUsage/updated` deltas and triggers `fresh_thread`. Secondary: `restart_after_jobs` (default 100) as a count-based fallback for non-context reasons (memory leaks, stale state).
- Same cross-query contamination risk as Claude's persistent pattern (documented under Risks).

**Alternatives considered, not supported:**

- `codex exec` fresh-per-job: ~21-26k uncached tokens per job, even after sanitation. Too bloated for a production default when app-server exists.
- `codex exec resume <session-id>`: works non-interactively but re-sends full conversation history each turn (first turn measured at 26k / 3.5k cached; second turn resume at 53k / 30k cached → ~23k fresh per turn). No real savings vs fresh-exec. Superseded by app-server.
- `codex mcp-server`: exposes codex as an MCP server. Not measured. Theoretically equivalent to app-server (same prompt-cache mechanism) but via a more standardized protocol (MCP vs codex-proprietary JSON-RPC). Candidate for evaluation if app-server destabilizes.

**Auth**: inherited from `~/.codex/auth.json` (populated by `codex login` — ChatGPT OAuth by default). JWT `id_token` expires at 1 hour, `access_token` at 10 days; codex auto-refreshes silently during `app-server` sessions. If both are expired, `app-server` returns an auth error on the first `turn/start` and exits — surfaces as an `auth_failed` error in our error mapping. Falls back to `OPENAI_API_KEY` env var if set.

**Sanitation caveats**: codex exposes fewer knobs than Claude. The superpowers plugin auto-loading (via `~/.codex/superpowers/hooks/hooks.json`) is the single biggest inflater; `--disable plugins` handles it. No equivalent to `--setting-sources ""`. Document `--disable plugins` + `--disable codex_hooks` as required in the install guide.

### `gemini` (persistent via --acp)

Verified against gemini CLI v0.37.1. **Empirical baseline: ~8.5k input tokens per turn in an ACP session** (`gemini-3-flash-preview`). Gemini's cache story is opaque — the ACP response's `_meta.quota.token_count` surfaces only `input_tokens` and `output_tokens`, with no `cached` field to verify prompt-cache hits. Token cost per turn is roughly flat across turns in a session (no accumulation visible), ~2k heavier than the `-p` baseline we measured earlier (~6.7k). The reason to adopt `--acp` is **not** token savings — it's **latency**: `--acp` skips Node.js startup + CLI init + auth/config load on every query, saving ~2-3s of wall-clock time per turn.

**`gemini --acp`** launches Gemini in [Zed's Agent Communication Protocol](https://github.com/zed-industries/agent-client-protocol) mode — JSON-RPC 2.0 over stdio. Daemon connects once per worker, issues `initialize` + `session/new`, then `session/prompt` per agent job (expansion or synthesis).

Launch command:
```
gemini --acp -e none
```

`-e none` disables gemini extensions (still applies at launch time; ACP inherits launch-time settings).

Protocol sequence per worker (JSON-RPC 2.0, newline-delimited JSON over stdio):

| Step | Method | Params | Response |
|---|---|---|---|
| 1 | `initialize` | `protocolVersion: 1`, `clientCapabilities: {fs: {readTextFile: true, writeTextFile: false}, terminal: false}` | `agentInfo`, `agentCapabilities`, `authMethods` |
| 2 | `session/new` | `cwd: "~/.memex"`, `mcpServers: []` | `sessionId`, `modes` (default/autoEdit/yolo/plan), `models` (available model slugs) |
| 3 | `session/set_mode` | `sessionId`, `modeId: "plan"` | acknowledgement (read-only, no tool execution) |
| 4 | `session/set_model` | `sessionId`, `modelId: "gemini-3-flash-preview"` | acknowledgement |
| 5 | `session/prompt` | `sessionId`, `prompt: [{type: "text", text: "<agent prompt + context + question>"}]` | notifications stream (below), then final response with `stopReason: "end_turn"` and `_meta.quota.token_count` |
| 6+ | repeat step 5 for each job |

Streaming notifications during a prompt:
- `session/update` with `update.sessionUpdate` kind:
  - `agent_thought_chunk` — streaming reasoning tokens (display-only; don't parse as answer)
  - `agent_message_chunk` — streaming assistant output; concatenate `content.text` for the final answer
  - `available_commands_update` / `current_mode_update` — IDE-oriented notifications; daemon ignores
- Final response on the `session/prompt` request ID contains `result.stopReason` + `result._meta.quota.token_count`

**Empirical usage** (4-turn session, `gemini-3-flash-preview`):

| Turn | Prompt | `input_tokens` | Elapsed |
|---|---|---:|---:|
| 1 (cold) | "Reply with only OK." | 8,679 | 1.7s |
| 2 (warm, short) | "Reply with only HI." | 8,495 | 1.7s |
| 3 (warm, +~1k context) | "Context: item × 500 … question?" | 9,510 | 7.2s |
| 4 (warm, short) | "Reply with only DONE." | 9,534 | 2.0s |

Observations:
- Per-turn input is flat (8.5k base) + the new prompt content. Turn 2 wasn't larger than turn 1, so prior-turn history doesn't appear to accumulate in the billable input count.
- Turn 3's +~1k delta matches the added context, confirming input is counted per-turn.
- Compared to fresh `gemini -p`: ~2-3s faster per query (avoids process spawn + init).

**System prompt**: no CLI flag, no session-level parameter. Prepend the agent prompt to every `session/prompt` user input. Alternative considered: drop a `GEMINI.md` in the daemon's cwd — works but less explicit; prefer the inline approach.

**Auth**: three modes, in precedence order: `GEMINI_API_KEY` env var; Google OAuth via `~/.gemini/oauth_creds.json` (auto-refreshed by google-auth, `expiry_date` field tracks freshness); Vertex AI via `GOOGLE_APPLICATION_CREDENTIALS` + `GOOGLE_GENAI_USE_VERTEXAI=true`. Daemon passes whichever the user has configured through the env allowlist.

**Caveats:**
- `--acp` is marked as stable in the `--help` output (the deprecated alias `--experimental-acp` remains for compat), but the Zed ACP protocol itself is young and may evolve. Pin behavior tests against the JSON-RPC methods we use; surface a clear error if `initialize` or `session/new` fails.
- No `cached_tokens` telemetry. If Gemini's backend prompt cache does hit across turns, we can't verify it. Cost projections must assume no amortization until Google surfaces the signal.
- Latency win is the real justification. Per-turn token cost is ~2k heavier than fresh `-p`; the tradeoff is ~2-3s faster wall-clock time per query.
- Same cross-query contamination risk as Claude/Codex persistent patterns.

**Sanitation caveats**: Gemini exposes the fewest knobs. No `--setting-sources` analog. `-e none` still applies at launch time (disables extensions). No per-invocation `hooks disable` — only a persistent `gemini skills disable <name>` / `gemini hooks disable <name>` that affects the user's interactive Gemini too. If the user has minimal installed skills/hooks, this is fine; if they have a lot, daemon invocations inherit them.

**Alternatives considered, not supported:**

- Fresh `gemini -p` per job: ~2-3s slower per query (Node.js + CLI init paid each time). Latency penalty unacceptable for interactive daemon.
- `gemini --resume` + `-p`: session IDs load but prior conversation content doesn't reliably transfer (empirical test returned "I do not have access to conversation history"). Not a viable persistent pattern.
- `gemini mcp` subcommand: manages MCP **client** config only (`add/remove/list/enable/disable`). Gemini has no `mcp-server` subcommand, so that codex-style pattern isn't available.

### Cross-provider comparison

Empirical measurements on one dev machine (codex v0.118.0, gemini v0.37.1, claude CLI). All sanitized configs:

| Property | `claude` | `codex` | `gemini` |
|---|---|---|---|
| Sanitized baseline (tokens per cold job) | **~1k** | **~19k** uncached turn 1 (app-server) | **~8.5k** per turn (acp) |
| Persistent warm pattern | stream-json stdin (native) | `app-server` JSON-RPC (thread/turn model; ~6× uncached savings across 15-turn thread) | `--acp` JSON-RPC (Zed ACP; no token amortization demonstrated, but ~2-3s latency savings per query by skipping CLI init) |
| Multi-turn over stdio | ✅ (stream-json) | ✅ (app-server JSON-RPC) | ✅ (ACP JSON-RPC) |
| Amortization type | token (prompt cache + warm subprocess) | token (OpenAI prompt cache; explicit `cachedInputTokens`) | latency only (no cache telemetry) |
| Cold-start amortization | ~15 jobs per restart | ~15 jobs per `thread/start` | per-process (spawn once per worker, many session/prompts) |
| Full context strip via CLI flags | ✅ | partial (plugins/hooks off); floor is codex's own scaffolding | minimal (`-e none` at launch); floor is Gemini's scaffolding + ACP scaffolding |
| Cross-query contamination risk | yes (see Risks) | yes — thread retains state across turns | yes — session retains state (though billable input suggests per-turn, worth verifying) |
| Auth at subprocess time | OAuth via `~/.claude/.credentials.json` | OAuth via `~/.codex/auth.json` (auto-refreshed) or `OPENAI_API_KEY` | `GEMINI_API_KEY` env var, OAuth, or Vertex AI |
| Default model (spec) | `sonnet` (mid-tier; `opus` for flagship) | `gpt-5.4-mini` (mid-tier; `gpt-5.4` for flagship) | `gemini-3-flash-preview` (mid-tier; `gemini-3-pro-preview` for flagship) |
| JSON output shape | `stream-json` events | JSON-RPC 2.0 (app-server): `thread/tokenUsage/updated` for usage, `item/agentMessage/delta` for streaming, `turn/completed` for termination | JSON-RPC 2.0 (ACP): `session/update` for streaming (`agent_message_chunk`), `session/prompt` final response with `_meta.quota.token_count` |

**Per-job token cost estimate** (sanitized input only, before context block):

| Provider | Per-job input floor | Per-job dollar cost (rough, @ public pricing, ~1k output) |
|---|---|---|
| `claude` warm job 1 (sonnet, default) | ~1k | ~$0.005 |
| `claude` warm job 15 (sonnet, default) | ~140k (cache reads) | ~$0.03 |
| `codex` app-server turn 1 (gpt-5.4-mini, default) | ~18.7k uncached + ~2.4k cached | ~$0.015 |
| `codex` app-server turn 2+ (gpt-5.4-mini, default; ~2k context) | ~2.3k uncached + ~21k cached reads | ~$0.004 |
| `codex` app-server turn 2+ (gpt-5.4, flagship escalation; ~2k context) | ~2.3k uncached + ~21k cached reads | ~$0.015 |
| `gemini` acp turn 1+ (3-flash-preview, default) | ~8.5k per turn (no cache signal) | ~$0.01 (estimated; verify pricing) |
| `gemini` acp turn 1+ (3.1-flash-lite-preview, budget) | ~8.5k per turn | ~$0.003 |

Three honest observations from the measurements:

1. **Claude's warm-subprocess advantage is the largest** — the ~1k baseline is 8-19× smaller than competitors on cold turns, and prompt caching amortizes subsequent turns almost to zero uncached. The persistent stream-json stdin pattern is unique to Claude.

2. **Codex app-server is the only viable codex path.** Fresh `exec` at ~21k uncached per job is too bloated; `app-server` drops turn 2+ to ~0.3k uncached via OpenAI prompt cache. Without app-server, codex wouldn't be worth supporting. With it, codex is comparable to Claude on warm turns.

3. **Gemini --acp buys latency, not tokens.** Per-turn cost stays at ~8.5k (no explicit cache signal from Google), but --acp skips ~2-3s of Node.js + CLI init per query. Fresh `gemini -p` is cheaper per turn (~6.7k vs 8.5k) but slower per query — the interactive-daemon workload favors the latency win, so --acp is the supported path.

**Recommendation for v1**: Claude is the default because (a) warm-subprocess amortization is best-in-class, (b) persistence is native (stream-json), (c) sanitation controls are most complete. Codex and Gemini are supported as alternatives — each is gated on its respective persistent pattern (`app-server` / `--acp`), with no fresh-per-job fallback since both fallbacks are too bloated or slow for an interactive daemon. Users with an OpenAI subscription pick `codex`; users with a Google account / API key pick `gemini`; everyone else (or budget-insensitive) picks `claude`.

**Model-tier caveat**: all three defaults are **mid-tier** (Sonnet / gpt-5.4-mini / gemini-3-flash-preview), chosen for cost parity rather than measured quality. Synthesis quality at mid-tier has not been benchmarked per-provider. Claude Sonnet-vs-Haiku comparison has some empirical grounding (Haiku was less reliable on rank-weighted synthesis), but no equivalent codex mid-vs-flagship or gemini flash-vs-pro comparison exists yet. Users observing degraded rank-weighted reconciliation should escalate via `daemon.worker.model` to the per-provider flagship.

**Retrieval is centralized in the actor.** The retrieval actor (see above) serializes embedding + read operations through a single task that owns the ONNX session and the current SQLite connection. This is intentional — embedding is 50-100ms and SQLite lookup dominates, so serializing doesn't cap realistic throughput, and it eliminates ORT thread-safety concerns and keeps ONNX memory bounded to one copy. If retrieval throughput ever becomes the bottleneck, a pool of actors or a thread-safe ORT upgrade is a follow-up optimization.

**Auth (per-provider summary):**

- `claude`: OAuth via `~/.claude/.credentials.json` or `CLAUDE_CODE_OAUTH_TOKEN`. No Anthropic API key required.
- `codex`: OAuth or API key via `~/.codex/auth.json` (populated by `codex login`). Falls back to `OPENAI_API_KEY` if set.
- `gemini`: `GEMINI_API_KEY` env var. Or Vertex AI via `GOOGLE_APPLICATION_CREDENTIALS` + `GOOGLE_GENAI_USE_VERTEXAI=true`.

Each provider's required env vars are added to the subprocess environment allowlist when `daemon.worker.backend` is set accordingly.

### 3. IPC protocol

Unix socket at `~/.memex/daemon.sock`. Newline-delimited JSON messages.

**Request:**
```json
{"op": "query", "v": 1, "question": "...", "raw": false, "top_k": 5, "memex_root": "/abs/path"}
```

`v` is the protocol version (currently `1`). The daemon supports a set of versions; unknown `v` returns a `version_mismatch` error with `supported: [...]`.

**Response stream (newline-delimited, one JSON object per line):**

Optional progress event (sent only when a queued job waits > 500ms before a worker picks it up):
```json
{"type": "queued", "ahead": 2}
```

Synthesized mode:
```json
{"type": "answer", "text": "...", "citations": ["auth-migration-timeline"]}
{"type": "done", "status": 0}
```

Raw mode:
```json
{"type": "context", "pages": [{"docid": "...", "stem": "...", "doc_type": "wiki", "rank": 1, "signal": "strong", "body": "..."}]}
{"type": "done", "status": 0}
```

**Errors (typed):**
```json
{"type": "error", "code": "<code>", "message": "<human readable>", "status": <exit_code>}
```

See the **Exit code contract** section below for the consolidated table of all exit codes across the memex CLI, the daemon, and the parallel-access layer.

Rationale for JSON-over-socket (vs. HTTP): simpler dependency footprint, avoids localhost port conflicts, permissions enforced by socket file mode (0600).

## Exit code contract

The `memex` binary — across all subcommands (`search`, `read`, `write`, `lint`, `delete`, `query`, `daemon`) — shares one exit-code scheme. Scripts inspecting `$?` get a uniform contract regardless of which subcommand ran. The exit code answers one question: **"can I retry, and if so how?"** Programmatic detail (what category of error, human message) lives in the CLI output: structured text for the core CLI, typed JSON `{"type": "error", "code": "...", ...}` for the daemon path.

### Exit codes

| Exit | Meaning | Produced by | Script action |
|---:|---|---|---|
| 0 | success | any subcommand | proceed |
| 1 | terminal error — don't retry without user action | core CLI (generic), daemon (generic, `bad_request`, `version_mismatch`, `retrieval_empty`, `agent_unavailable`, `internal`) | log and fail |
| 2 | `LockTimeout` — retry-worthy after brief backoff | core CLI (`write`, `lint --fix`, `delete`) | retry with backoff |
| 3 | `FileOpExhausted` — retry-worthy after brief backoff | core CLI (`write`, etc.) | retry with backoff |
| 4 | daemon transient failure — retry-worthy | daemon (`subprocess_timeout`, `subprocess_crashed`) | retry with backoff |
| 5 | auth failure — user must re-authenticate | daemon (`auth_failed`) | prompt user to run provider's login (`claude login` / `codex login` / `gemini` re-auth), then retry |

Authority for codes `1-3`: `2026-04-16-parallel-access-design.md`. Authority for codes `4-5`: this spec.

### JSON error taxonomy (programmatic surface)

Exit code `1` covers many distinct failure categories. Scripts that want to branch on category inspect the JSON `error.code` field in the daemon response, or parse structured stderr for the core CLI. The daemon uses these code strings:

- `bad_request` — malformed JSON, missing fields, unknown `op`
- `version_mismatch` — protocol `v` not supported (response includes `supported: [...]`)
- `retrieval_empty` — wiki has no indexed content for the requested `memex_root`
- `internal` — unexpected daemon-side error (bug)
- `subprocess_timeout` — agent subprocess hung past `daemon.worker.timeout_sec`
- `subprocess_crashed` — agent subprocess crashed twice (initial + retry)
- `auth_failed` — agent subprocess reported auth error after one retry
- `agent_unavailable` — the configured `daemon.worker.backend`'s persistent pattern is not available on this system (e.g., `codex app-server` method not recognized on older codex; `gemini --acp` flag rejected on older gemini). User must switch `daemon.worker.backend` or upgrade the CLI.

Mapping: `subprocess_timeout` / `subprocess_crashed` → exit 4. `auth_failed` → exit 5. `agent_unavailable` → exit 1 (terminal; requires user action). Everything else → exit 1. The core CLI continues to map `LockTimeout` → 2, `FileOpExhausted` → 3, everything else → 1 per parallel-access.

### Why this shape

This mirrors the parallel-access design's philosophy: exit codes classify **retry-ability**, not category. Scripts need to know "should I retry, and how" — they don't need a separate code for every failure reason. Category detail lives in the error body.

### Rules for future additions

- **New terminal errors** (user mistake, config issue, structural failure) → collapse to `1`. Add the category to the JSON `error.code` vocabulary and to the actionable-hint logic.
- **New retry-worthy transient errors** → use an existing transient code (`2` / `3` / `4`) if the semantic matches, or allocate a new code sparingly (only when scripts need to distinguish different retry strategies).
- **New errors requiring user action** (like re-auth) → use `5` or allocate a new code in the same spirit.
- **Never** introduce a code whose meaning is "this failed, no special handling" — that's what `1` is for.

## Data flow

### Synthesized query (default)

1. Agent: `memex query "When did the auth migration production rollout begin?"`
2. CLI → daemon: request with `raw: false`
3. Daemon retrieval: probe search returns wiki hit for `auth-migration-timeline` at rank 1.
4. Daemon reads the top page body.
5. Daemon sends the user message to the agent subprocess:
   ```
   [TASK: SYNTHESIZE]

   Context:
   ## [rank 1, wiki, signal strong] auth-migration-timeline
   - 2026-02-03: Team picked OAuth provider
   - 2026-03-05: Mobile release cut; migration unblocked
   - 2026-04-02: Staging cutover
   - 2026-04-16: Production rollout begins
   ...
   Question: When did the auth migration production rollout begin?
   ```
6. Agent subprocess responds: "Production rollout began on April 16, 2026. [[auth-migration-timeline]]"
7. Daemon → CLI: answer message, then done.
8. CLI prints the answer, exits 0.

**Main agent context growth**: ~400 tokens (the answer + tool_use shell).

### Raw query (`--raw`)

Same as above through step 4. At step 5, daemon skips synthesis and returns the retrieved pages directly. Agent receives structured data and synthesizes itself.

**Main agent context growth**: ~5-7k tokens (retrieved pages).

## Error handling

| Failure | Behavior |
|---|---|
| Daemon not running | CLI forks a detached `memex daemon start`, then retries connect in a loop (up to ~2s total, 50ms per attempt). |
| Concurrent daemon spawn (two CLIs racing) | Both CLIs fork daemons; both daemons race for the lifetime flock at `~/.memex/daemon.lock`. Whichever acquires first binds the socket and serves; the other sees flock busy and exits cleanly (exit 0). Both CLIs' connect loops eventually succeed against the winning daemon. |
| Daemon crash mid-request | CLI's socket read returns EOF → CLI exits with exit code 1 and `internal` error message. User re-runs. Next invocation finds no live daemon (flock released by OS), spawns a fresh one. |
| Agent subprocess dies | Worker detects via EOF on stdout, respawns subprocess, **retries the current job once** on the fresh subprocess. If the retry also fails → `subprocess_crashed` error (exit 4). Applies to all providers. |
| Agent subprocess hang | 60s read timeout fires (configurable via `daemon.worker.timeout_sec`). Worker kills subprocess, respawns, retries once. If retry also hangs → `subprocess_timeout` error (exit 4). Applies to all providers. |
| OAuth token expired / rotated | Agent subprocess reports auth failure. Worker treats this like a crash: respawns (picks up rotated creds from disk), retries once. If retry also fails → `auth_failed` error (exit 5) with a message pointing to the provider's login command (`claude login` / `codex login` / `gemini` re-auth). |
| Context overflow (before restart counter triggers) | Subprocess reports an overflow error. Worker treats as crash: respawn + retry once. |
| Socket file stale after crash | Resolved by the daemon lifetime flock: whichever process acquires the flock first unlinks any orphaned socket, then binds fresh. No race: the flock serializes the cleanup, and a crashed daemon's flock is released by the OS automatically. |
| Malformed client request | Daemon responds with `bad_request` error (exit 1). Connection closes cleanly. |
| Protocol version unsupported | Daemon responds with `version_mismatch` error (exit 1) including `supported: [...]`. |
| No indexed content for MEMEX_ROOT | Retrieval actor responds with `retrieval_empty` error (exit 1) without touching the agent job queue. |
| Queue saturated at bound | Producer (connection handler) blocks on `send().await`, applying backpressure. CLI sees silent wait; after 500ms of queue time, a `{"type":"queued","ahead":N}` event is emitted so the user sees progress. |

## Configuration

Two sources, in precedence order (highest first): **env vars → config file → built-in defaults**.

### Config file

Location: `~/.memex/config.toml` (the user-home memex config — same file the parallel-access design uses, since `~/.memex` is also the default `MEMEX_ROOT`). `$MEMEX_CONFIG` env var overrides the path.

The daemon reads this user-home config for its own lifecycle settings. The existing `[locking]` section (from `2026-04-16-parallel-access-design.md`) is loaded by `memex-core` per `MEMEX_ROOT` and continues to work unchanged. The daemon adds three new top-level sections that do not overlap with `[locking]`:

```toml
# existing from parallel-access design (memex-core reads per-MEMEX_ROOT):
[locking]
timeout_seconds = 120

# new, read only by the daemon:
[daemon]
idle_timeout_min = 15            # how long to stay alive without requests
log_file = "~/.memex/daemon.log"

[daemon.worker]
agent = "claude"                 # one of: "claude", "codex", "gemini"
model = "sonnet"                 # model slug for the selected agent (see per-provider notes for defaults and escalation paths)
max_count = 8                    # autoscale ceiling; default: num_cpus::get(). Pool starts at 1 worker, scales up to this under pressure.
idle_reap_sec = 600              # non-min workers exit after N sec with no job
restart_after_jobs = 100         # hard respawn fallback; context-based restart fires first at ~70% of model max_input_tokens
timeout_sec = 60                 # read timeout on subprocess output per job

[query]
top_k = 5                        # default top-K for retrieval (cross-command: applies to memex search and memex query)
```

Missing keys fall back to built-in defaults. Missing file is not an error. Malformed TOML → daemon refuses to start and surfaces a clear error (matching the parallel-access handling).

### Loader

Use **`config`** (crates.io/crates/config, aka config-rs) for config loading. Rationale: with ~8 settings across three sections, hand-rolling "read TOML, then check each env var, then fall back to default" is verbose and error-prone. `config-rs` handles layering declaratively, serde does the deserialization, and adding new keys costs one line on the struct. Actively maintained (latest release 2026-03-17) under the `rust-cli` GitHub org. `figment` was considered but has not received commits since late 2024.

```rust
use ::config::{Config as Loader, File, Environment};

#[derive(Deserialize)]
struct DaemonConfig {
    #[serde(default = "default_idle_timeout_min")] idle_timeout_min: u64,
    #[serde(default = "default_log_file")]          log_file: PathBuf,
    #[serde(default)]                               worker: WorkerConfig,
}
#[derive(Deserialize)]
enum Backend { #[serde(rename="claude")] Claude, #[serde(rename="codex")] Codex, #[serde(rename="gemini")] Gemini }

#[derive(Deserialize)]
struct WorkerConfig {
    #[serde(default = "default_backend")]            backend: Backend,
    #[serde(default)]                               model: Option<String>,    // None → per-backend default (sonnet / gpt-5.4-mini / gemini-3-flash-preview)
    #[serde(default = "default_max_count")]         max_count: usize,         // default: num_cpus::get()
    #[serde(default = "default_idle_reap_sec")]     idle_reap_sec: u64,       // default: 600
    #[serde(default = "default_restart_after_jobs")] restart_after_jobs: u32, // default: 100 (secondary; context-based restart is primary)
    #[serde(default = "default_timeout_sec")]       timeout_sec: u64,
}
#[derive(Deserialize)]
struct QueryConfig {
    #[serde(default = "default_top_k")]             top_k: usize,
}
#[derive(Deserialize)]
struct Config {
    #[serde(default)] daemon: DaemonConfig,
    #[serde(default)] query: QueryConfig,
}

fn load_config(config_path: &Path) -> Result<Config> {
    Loader::builder()
        .add_source(File::from(config_path).required(false))   // file (optional)
        .add_source(Environment::with_prefix("MEMEX").separator("__"))
        .build()?
        .try_deserialize::<Config>()
        .context("invalid memex config")
}
```

**Env var → config path mapping** uses `config-rs`'s `.prefix_separator("__").separator("__")`: `MEMEX__DAEMON__WORKER__MAX_COUNT` maps to `daemon.worker.max_count`, `MEMEX__DAEMON__WORKER__MODEL` to `daemon.worker.model`, and so on. Double-underscore is the path separator. Single-underscore (e.g., `MEMEX__DAEMON_WORKER_MODEL` (single-underscore separator inside)) would need a hand-rolled env-var parser to disambiguate underscores inside field names from section separators; double-underscore lets `config-rs` handle it declaratively.

For the one path-override env var that doesn't live inside the config struct:

| Env var | Purpose |
|---|---|
| `MEMEX_CONFIG` | path to the config file itself (default: `~/.memex/config.toml`) |

**Validation bounds** (enforced post-deserialize, matching parallel-access's approach for `timeout_seconds`):

| Field | Valid range |
|---|---|
| `daemon.idle_timeout_min` | 1..=1440 (1 min to 24 hr) |
| `daemon.worker.max_count` | 1..=64 |
| `daemon.worker.idle_reap_sec` | 30..=3600 |
| `daemon.worker.restart_after_jobs` | 1..=1000 |
| `daemon.worker.timeout_sec` | 1..=3600 |
| `query.top_k` | 1..=20 |

`[daemon]` groups everything daemon-only (lifecycle + its worker pool). `[query]` and `[locking]` stay at top level because they are cross-command — `query.top_k` applies to `memex search` as well as `memex query`; `[locking]` is read by memex-core per `MEMEX_ROOT`.

Out-of-range values → `bad config` error at startup, daemon exits 1 with a clear message.

Defaulting `daemon.worker.max_count` to `num_cpus::get()` mirrors the "one worker per core" heuristic. The cap is `max_count` itself — users set it lower for memory-constrained or API-rate-limited environments, or higher on large hosts where the per-worker subprocess cost (~100–300 MB RAM for claude/codex/gemini) is acceptable. Note that the pool starts at 1 worker and only grows under sustained queue pressure, so the ceiling is a worst-case bound, not an RAM floor.

Note: memex-core's parallel-access `[locking]` section stays on its hand-rolled loader for now (no coupling). Future work could migrate it to `config-rs` for consistency; out of scope here.

## Testing

Coverage is planned before implementation so tests ship alongside each unit, not as a follow-up.

### CLI (`memex query`)

**Unit:**
- `build_request(args, memex_root)` → `QueryRequest` round-trip: question, raw, top_k, v=1.
- stdout/stderr routing: `answer` → stdout, `queued` → stderr.

**Integration:**
- Happy path: spawn a mock daemon that returns canned responses; verify CLI prints expected output and exits 0.
- Daemon not running + auto-spawn: first invocation spawns daemon, subsequent invocations reuse.
- Spawn race: two CLIs race to spawn daemons; the lifetime flock ensures only one daemon binds the socket; the "losing" daemon process exits 0 on flock busy; both CLIs' connect loops succeed against the winning daemon.
- Stale PID file (PID points to dead process): CLI cleans up and spawns fresh.
- Socket retry exhaustion: daemon binary missing from PATH → CLI fails with a clear error.
- Malformed response from daemon (garbage JSON) → CLI surfaces `internal` error cleanly.

### Daemon startup & lifecycle

**Integration:**
- Normal start: ONNX loaded before socket bind; ready signal delays until load complete.
- Stale socket cleanup: socket file exists but bound-process is dead → daemon unlinks and proceeds.
- Lifetime flock busy: second daemon startup attempt while first is running → flock fails non-blocking → second daemon exits 0 cleanly (not an error; the running daemon owns the lock).
- Post-crash recovery: kill -9 the running daemon → OS releases the flock → spawn a new daemon → new daemon acquires flock, unlinks stale socket, binds fresh, serves.
- SIGTERM graceful shutdown: in-flight jobs complete, queue drains or rejects, no orphan agent subprocesses.
- Idle timeout: no connections for N minutes → daemon exits cleanly.

### Connection handler + protocol

**Unit:**
- JSON decode: valid / missing field / extra field / wrong type.
- Version check: `v=1` accepted; `v=2` → `version_mismatch` error with `supported:[1]`.
- Op dispatch: `query` handled; unknown op → `bad_request`.

**Integration:**
- CLI disconnects mid-synthesis: worker finishes work, `reply_tx` is closed, worker handles `send` failure silently.
- Queued event: job waits >500ms in queue → client receives `{"type":"queued","ahead":N}`; if picked up within 500ms, no event is sent. Fires for any queued job type (`ExpandJob` or `SynthJob`).

### Queue + worker pool

**Integration:**
- Concurrent requests: N CLIs (> workers) submit simultaneously; all complete; FIFO-ish ordering observed via reply ordering.
- Queue backpressure: fill queue to capacity, next producer blocks until a worker drains.
- Queue bounded: producer never OOMs even under sustained load.

### Worker restart

**Integration:**
- Counter-triggered restart: run N+1 agent jobs (any mix of `ExpandJob`/`SynthJob`) on one worker; verify subprocess PID changes exactly once, between jobs.
- Crash-triggered retry: kill the subprocess mid-request → worker respawns and retries the same job once; user sees success.
- Double crash: subprocess crashes twice → `subprocess_crashed` error surfaces; worker healthy for next request.
- Auth error: inject a 401 response from a mock agent binary → worker respawns, retries, succeeds on retry (new creds simulated).
- Subprocess hang: agent binary stub blocks forever → 60s timeout fires → `subprocess_timeout` error.
- Provider unavailability: launch `codex` binary that returns "unknown subcommand: app-server" (or `gemini` binary that rejects `--acp`) → daemon reports `agent_unavailable` (exit 1); no retry attempted.

### Retrieval actor

**Unit:**
- Context formatter: top-K pages with mixed wiki+source → each section tagged `[rank N, doc_type, signal strong|weak]` correctly.
- Rank assignment: RRF ordering preserved end-to-end.

**Integration:**
- Probe strong-signal path: query with known match → reads top-1, signal=strong.
- Probe weak-signal path: query with no clear match → still reads top-K, signal=weak.
- Empty wiki: daemon returns `retrieval_empty` error without touching the agent job queue.
- MEMEX_ROOT switching: two requests with different `memex_root` values share one daemon; each gets data from its own wiki.
- SQLite connection reuse: same `memex_root` on consecutive requests doesn't re-open the DB.

### Config

**Unit:**
- `resolve_config(env, file, default)` precedence: env beats file beats default.
- Malformed TOML → clear parse error; daemon refuses to start (exits non-zero).
- `worker.max_count` default is `num_cpus::get()`; `worker.idle_reap_sec` default is 600; `worker.restart_after_jobs` default is 100.

### Logging

**Integration:**
- Structured JSON lines contain expected fields for: spawn, queue-depth spike, worker restart, auth error, subprocess timeout.
- Size-based rotation: write > rotation threshold → old file renamed, new file created; bounded file count.

### Manual smoke test

- Run `memex query` against conv_0's wiki. Verify: answer correctness, citations present, queued event appears under load (kick off 5+ queries in parallel), daemon idles out after 15 min.

### Test framework

- Rust: `cargo test` for unit + most integration tests.
- Subprocess / daemon-lifecycle tests use `tokio::test` + tempdir for `MEMEX_ROOT` and `~/.memex` replacements.
- Mock agent binaries — three small Rust bins, one per provider, each speaking its provider's protocol and writing canned responses. Cover crash, timeout, auth-error, provider-unavailable, and success paths. Stored under `tests/fixtures/mock-claude/`, `tests/fixtures/mock-codex-app-server/`, `tests/fixtures/mock-gemini-acp/`.

## Out-of-scope decisions (deferred)

1. **Windows support.** Unix socket is Mac/Linux. Windows would require named pipes or TCP localhost; deferred until there's demand.
2. **MCP server.** Design 4 remains future work. The daemon can be wrapped as an MCP server later without changing the current IPC contract significantly.
3. **Reranker.** No local reranker (Qwen3 style). Relies on RRF fusion for ordering.
4. **Token-based restart trigger.** Current design uses query count. Token-count-based restart (monitor context growth from subprocess output) is a possible refinement.
5. **Authentication for the socket.** File mode 0600 suffices for single-user systems. Multi-user machines or system-wide deployment would need token-based auth.

## Future capabilities the daemon enables

The daemon exists primarily to answer queries, but once it's running we get a long-lived, OS-level process that can do things a short-lived CLI cannot. These are **not in scope for v1** but worth listing so the architectural value of the daemon isn't underestimated:

- **File-watch auto-reindex.** Watch `{memex_root}/wiki/` and `{memex_root}/source/` with the `notify` crate. On change: debounce ~200ms, re-embed, update SQLite. Fixes the current silent staleness when users edit wiki pages in their editor. Requires the daemon to become a Lock 1 writer — changes the "pure reader" invariant in the Relationship section.
- **Background wiki maintenance.** Periodic `memex lint` with broken-link alerts surfaced via the log or a status endpoint; periodic SQLite `VACUUM` / `ANALYZE` on idle; periodic re-embedding of hot pages to keep caches warm.
- **Incremental ingest from a dropbox.** Watch a configured inbox directory; auto-ingest new markdown files via the ingest skill. Turns memex into a continuous-capture system rather than a batch one.
- **Event stream for other tools.** A new `{"op": "subscribe"}` op lets UI tools, editor extensions, or scripts receive push notifications when the wiki changes or when a query completes.
- **Query telemetry.** Track hit/miss rates, weak-signal frequency, cold-start counts, p50/p95/p99 timing — surfaced via a `{"op": "stats"}` endpoint for tuning.
- **Worker warm-up on a schedule.** Pre-spawn a worker a few minutes before predicted user activity (e.g., morning work hours) to hide cold start entirely.
- **Cross-session context continuity** (if we ever decide we want it): currently we treat cross-query contamination as a bug, but the daemon is where that conversation state lives. If conversational mode becomes a product goal, the daemon is where that gets built.

Each of these is independent and incrementally addable; most share very little with the query path. The daemon's value compounds as these land.

## Success criteria

- `memex query "<question>"` returns a synthesized answer in under 3 seconds (warm daemon).
- First query after daemon start completes in under 10 seconds on claude, under 15 seconds on codex/gemini (cold start includes daemon spawn + agent subprocess warmup).
- Main-agent context growth per query is ≤ 1k tokens on average (synthesized mode).
- `--raw` mode preserves the current agent-driven workflow as an escape hatch.
- Daemon restarts subprocess automatically when context safety threshold is approached; no overflow errors visible to the CLI caller under normal operation.

## Risks

- **Provider CLI version drift** (claude / codex / gemini): each provider's protocol (claude stream-json, codex app-server JSON-RPC, gemini ACP JSON-RPC), flags, or model slugs may change. Codex ships in fast minor versions (0.118.0 tested) and `app-server` is marked `[experimental]`; gemini `--acp` is young and its 3.x models all carry `-preview` suffix. Mitigation: pin behavior tests against the protocol methods / flags we use per provider; surface `agent_unavailable` if the persistent subcommand isn't recognized; document model-slug fallbacks in the install guide.
- **OAuth credential scope**: each provider's credential file (`~/.claude/.credentials.json` / `~/.codex/auth.json` / `~/.gemini/oauth_creds.json`) is meant for the user's own interactive CLI session. Running a daemon that uses the same credential is within intended use per claude-mem precedent, but worth verifying for each provider: (a) no rate-limit conflict when user's regular session and the daemon both make calls; (b) auto-refresh doesn't race between the two processes and invalidate tokens; (c) for codex on ChatGPT auth, the 1-hour id_token TTL is short enough that concurrent refresh bugs would surface quickly.
- **Subprocess hang**: agent subprocess blocks without error → daemon's 60s read timeout fires → worker respawns, retries once → if still hung, `subprocess_timeout` error to user. Timeout configurable via `daemon.worker.timeout_sec`. Applies to all providers.
- **Cold start perceived as slow**: first query pays daemon spawn + persistent subprocess warmup + first-turn baseline. Claude: ~8-10s (subprocess + stream-json init + cold cache). Codex app-server: ~5-10s (subprocess spawn + `initialize` + `thread/start` + ~19k uncached turn 1). Gemini --acp: ~3-5s (subprocess spawn + `initialize` + `session/new` + ~8.5k turn 1). Real users might find this jarring. Mitigation: log a "Starting memex daemon..." message on spawn; document the behavior per provider.
- **Cross-query contamination (known, deferred)**: worker subprocesses accumulate conversation state across jobs (up to 14 prior jobs before the 15-query restart on all three providers). This can cause prior queries' retrieved content to leak into later queries' answers, inflate benchmark scores via in-session "memory", and enable prompt-injection persistence within a worker's lifetime window. The architectural cause is that each provider's persistent protocol appends to the session's conversation (claude's stream-json, codex's thread, gemini's ACP session) — by design. **Not addressed in this spec** — documented as a known defect to revisit. Likely fix is a user-message preamble instructing the model to treat each query as independent, plus an `isolation_mode = "fresh"` config knob that resets the subprocess session per job (claude: restart; codex: new `thread/start`; gemini: new `session/new`) for benchmark/correctness-critical workloads. Needs empirical verification of the preamble approach per provider before committing.
