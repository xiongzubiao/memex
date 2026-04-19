# Installing memex

Five-minute walkthrough from empty checkout to a working `memex query`.

## Prerequisites

- **Unix**: macOS or Linux. Windows is not supported (the daemon uses
  Unix sockets and flock).
- **Rust toolchain**: 1.80 or newer. Install via https://rustup.rs.
- **jq**: required by the hook installer. `brew install jq` (macOS) or
  your distro's package manager.
- **At least one agent CLI**: `claude`, `codex`, or `gemini`. Required
  only for synthesis (answer generation). The `memex query --raw` path
  doesn't need an agent.
- **Optional**: ONNX embedding model at
  `~/.memex/models/embedding-gemma-300m.onnx`. Without it, memex falls
  back to a hash-based embedding (BM25 still works; vector retrieval is
  degraded).

## Build from source

```bash
git clone <repository-url>
cd memex
cargo build --release
```

The binary lands at `target/release/memex`. Copy or symlink it onto your
`$PATH`:

```bash
mkdir -p ~/.local/bin
cp target/release/memex ~/.local/bin/
# Make sure ~/.local/bin is on $PATH in your shell config.
```

> Note: packaged distributions (npm, GitHub Releases) are planned as a
> follow-up. For now, build from source.

## Configure

Create `~/.memex/config.toml` (optional — all defaults are sensible):

```toml
[daemon]
idle_timeout_min = 15               # daemon exits after N minutes idle
log_file = "~/.memex/daemon.log"

[daemon.worker]
agent = "claude"                    # one of: claude, codex, gemini
model = "sonnet"                    # agent-specific model slug
max_count = 8                       # pool autoscales up to this many workers (default: num_cpus)
idle_reap_sec = 600                 # non-min workers exit after N sec idle
restart_after_jobs = 100            # hard respawn fallback; context-based restart (70% of model max_input) fires first
timeout_sec = 60                    # per-turn read timeout

[query]
top_k = 5                           # retrieval top-K default
```

Plan 5 will add Codex + Gemini worker implementations. Today, only
`worker.agent = "claude"` actually runs; other values are rejected.

## Register SessionStart hooks (auto-warm)

```bash
scripts/install-hooks.sh
```

The script detects which agent CLIs you have installed (`~/.claude`,
`~/.codex`, `~/.gemini`) and merges the memex SessionStart hook into each
one's settings. Idempotent — re-run any time.

After registration, opening a new agent session pre-warms the daemon, so
your first `memex query` of the session is as fast as subsequent ones
(~1s instead of ~8-10s cold).

If you skip this step, that's fine — the daemon auto-spawns on the first
`memex query`. You just pay the cold-start once per daemon lifetime
(default 15min idle timeout).

## Authenticate the agent CLI

Whichever agent you use for synthesis needs to be logged in. memex just
invokes the CLI as a subprocess; the CLI handles its own OAuth / API
keys.

- **Claude Code**: `claude /login` — OAuth flow, writes to
  `~/.claude/.credentials.json`. Or set `CLAUDE_CODE_OAUTH_TOKEN`.
- **Codex CLI**: `codex login` — writes to `~/.codex/auth.json`. Or set
  `OPENAI_API_KEY`.
- **Gemini CLI**: `gemini` (walks through auth on first run), or set
  `GEMINI_API_KEY`, or configure Vertex AI.

## First query

Write a page:

```bash
mkdir -p ~/.memex/wiki
cat > /tmp/auth-migration.md <<'EOF'
---
title: Auth Migration Timeline
tags:
  - entity
created_at: 2026-04-10T00:00:00Z
updated_at: 2026-04-10T00:00:00Z
sources: []
---

- 2026-04-16: Production rollout begins
EOF

memex write auth-migration --force --quiet < /tmp/auth-migration.md
```

Query it:

```bash
memex query "when did production rollout begin"
```

Expected output (real content varies by agent):

```
Expansion:
  lex: production deployment
  vec: when did the production release happen
  hyde: The production rollout began on [date].

Production rollout began on 2026-04-16. [[auth-migration]]

Citations:
  [[auth-migration]]
```

For structured retrieval without synthesis (no LLM call):

```bash
memex query --raw "when did production rollout begin"
```

## Daemon lifecycle

- `memex daemon start` — run in foreground (auto-spawn does this detached).
- `memex daemon stop` — SIGTERM + wait up to 10s.
- `memex daemon status` — PID + ping health check.

The daemon exits after `daemon.idle_timeout_min` minutes without queries.
It's also restarted automatically per `daemon.worker.restart_after_jobs`
(the worker's Claude subprocess, not the daemon itself).

## Troubleshooting

**`memex query` reports `auth_failed`**
  The agent CLI's OAuth is expired or missing. Run its login command:
  `claude /login`, `codex login`, etc.

**`memex query` reports `subprocess_crashed`**
  The agent CLI crashed or exited non-zero. Check the daemon log at
  `~/.memex/daemon.log` for details. Common causes: missing agent binary
  on PATH, missing CLI flags (newer CLI version). Retry once fires
  automatically; a second crash means something is systemically broken.

**`memex query` reports `retrieval_empty`**
  No indexed pages match any of the query's terms (lex or vector). Check
  that `~/.memex/wiki/` has content and that `memex search <q>` returns
  results.

**Daemon won't start: "lock busy"**
  Another memex daemon is already running. `memex daemon status` confirms.
  If that's stale, `memex daemon stop` or manually delete
  `~/.memex/daemon.lock`.

**Vector search returns garbage / low-quality results**
  ONNX model isn't loaded. Check `~/.memex/daemon.log` for "ONNX model not
  available". Download the model to
  `~/.memex/models/embedding-gemma-300m.onnx` and restart the daemon.

**Hook doesn't fire on session start**
  Verify the hook is registered: `jq '.hooks.SessionStart' ~/.claude/hooks.json`
  (or equivalent). Re-run `scripts/install-hooks.sh` if missing. For
  Codex CLI specifically, also confirm `[features] codex_hooks = true`
  is set in `~/.codex/config.toml`.
