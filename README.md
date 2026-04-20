# memex

A personal knowledge wiki that learns from your coding sessions.

memex watches your Claude Code, Codex, and Gemini CLI sessions, extracts
knowledge into wiki pages, and lets you query everything in natural
language. Retrieval uses BM25 + vector fusion with RRF. Synthesis runs
through a long-lived daemon that keeps agent subprocesses warm.

```bash
# Query your wiki
memex query "how does the auth migration work"

# Write a page manually
memex write auth-migration --force --quiet <<'EOF'
---
title: Auth Migration Timeline
tags: [auth, migration]
---
Production rollout began on 2026-04-16...
EOF

# Backfill from existing sessions
memex backfill claude-code
```

## How it works

1. **Session hooks** fire after each agent session ends, sending the
   transcript to the daemon via `memex ingest`.
2. The daemon **cleans** the transcript (strips tool results, metadata,
   system tags) and **extracts** wiki pages using an LLM call.
3. If extracted pages overlap with existing wiki content, a second LLM
   call **merges** them.
4. Pages are stored as Markdown files, indexed with BM25, and embedded
   for vector search.
5. `memex query` retrieves relevant pages and synthesizes an answer.

## Install

### Prerequisites

- **Unix**: macOS or Linux. Windows is not supported (Unix sockets + flock).
- **Rust toolchain**: 1.80 or newer (https://rustup.rs).
- **At least one agent CLI**: Claude Code, Codex, or Gemini CLI. Required
  for synthesis and session ingestion. `memex query --raw` and
  `memex search` work without an agent.
- **Optional**: ONNX embedding model at
  `~/.memex/models/embedding-gemma-300m.onnx`. Without it, memex falls
  back to hash-based embedding (BM25 still works, vector retrieval is
  degraded).

### Build from source

```bash
git clone <repo>
cd memex
cargo build --release
mkdir -p ~/.local/bin
cp target/release/memex ~/.local/bin/
```

### Plugin install (recommended)

The `plugin/` directory is an npm package that handles everything:
downloads the binary, embedding model, ONNX Runtime, and generates
session hooks for your agent.

```bash
cd plugin && npm install
```

## Configure

Create `~/.memex/config.toml` (optional, all defaults are sensible):

```toml
[daemon]
idle_timeout_min = 15

[daemon.worker]
agent = "claude-code"               # claude-code, codex, or gemini-cli
model = "claude-sonnet-4-6"         # agent-specific model name
max_count = 8                       # worker pool cap (default: num_cpus)
timeout_sec = 60                    # per-turn read timeout

[query]
top_k = 5
```

### Authenticate the agent CLI

memex invokes the agent CLI as a subprocess. The CLI handles its own auth.

- **Claude Code**: `claude /login` or set `CLAUDE_CODE_OAUTH_TOKEN`.
- **Codex**: `codex login` or set `OPENAI_API_KEY`.
- **Gemini CLI**: `gemini` (auth on first run), or set `GEMINI_API_KEY`.

## Daemon lifecycle

The daemon auto-spawns on first use (`memex query`, `memex ingest`,
`memex backfill`). Manual control:

- `memex daemon start` -- foreground mode.
- `memex daemon stop` -- SIGTERM + wait up to 10s.
- `memex daemon status` -- PID + ping health check.

The daemon exits after `idle_timeout_min` minutes without activity.

## Architecture

- `core/` -- `memex-core`: BM25 + vector search, embedding, storage, transcript parsing.
- `cli/` -- `memex-cli`: the `memex` binary (CLI + daemon).
- `plugin/` -- npm package for agent plugin installation and hooks.

## Troubleshooting

**`auth_failed`** -- Agent CLI OAuth is expired or missing. Run its login command.

**`subprocess_crashed`** -- Agent CLI crashed or exited non-zero. Check `~/.memex/daemon.log`.

**`retrieval_empty`** -- No indexed pages match. Check that `~/.memex/wiki/` has content.

**"lock busy"** -- Another daemon is already running. `memex daemon stop` to clear.

**Low-quality vector results** -- ONNX model not loaded. Download to
`~/.memex/models/embedding-gemma-300m.onnx` and restart the daemon.

## License

Dual-licensed under MIT OR Apache-2.0.
