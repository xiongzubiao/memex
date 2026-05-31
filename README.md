<div align="center">

# 🧠 memex

**A personal knowledge wiki that learns from your coding sessions.**

Your Claude Code, Codex, and Gemini sessions become a searchable, self-distilling wiki you can ask in plain English.

[![npm](https://img.shields.io/npm/v/@xiongzubiao/memex?color=cb3837&logo=npm)](https://www.npmjs.com/package/@xiongzubiao/memex)
&nbsp;![platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-555)
&nbsp;![built with Rust](https://img.shields.io/badge/built%20with-Rust-dea584?logo=rust&logoColor=white)
&nbsp;![extra API key](https://img.shields.io/badge/extra%20API%20key-not%20required-2ea44f)

</div>

---

You solve something hard in a session, and a week later it's gone. memex fixes that: it watches your agent sessions, distills what you worked out into Markdown wiki pages, and answers questions over them — driven by the agent CLI you already use, so there's nothing new to pay for and no key to manage.

```bash
memex query "how does the auth migration work"
```

## ✨ Why memex

- **🪄 Learns while you work.** A session-end hook turns each finished Claude Code / Codex / Gemini session into wiki pages automatically. Zero copy-paste.
- **🔑 No extra API key, no new bill.** The daemon drives your existing `claude` / `codex` / `gemini` CLI (`claude -p`) for extraction and synthesis. It rides the login you already have.
- **🔎 Answers, not ten blue links.** `memex query "..."` runs BM25 + vector retrieval (RRF fusion) and synthesizes a *cited* answer from your pages.
- **📄 Plain Markdown you own.** Every page is a file on disk; SQLite is just a rebuildable index. Grep it, commit it, hand-edit it.
- **⚡ Warm and local.** A long-lived daemon keeps the embedder and agent subprocesses hot, talking over a Unix socket. Nothing leaves your machine except the LLM calls your agent CLI already makes.
- **🗂️ Collections.** Scope ingest and queries to a project, team, or incident.

## 🚀 Quickstart

```bash
npm install -g @xiongzubiao/memex   # binary + embedding model + ONNX runtime
memex install                       # wire session hooks into your installed agents
memex doctor                        # verify everything's ready
```

Now just work. When a session ends, memex ingests it in the background. Ask anytime:

```bash
memex query "how does the auth migration work"
memex query "auth rollout date" --collection team-a --collection incidents
```

Already have history? Backfill it:

```bash
memex backfill claude-code
memex backfill codex --collection team-a
```

## 📖 Usage

```bash
# Write a page by hand (Markdown on stdin)
memex write auth-migration --force --quiet <<'EOF'
---
title: Auth Migration Timeline
---
Production rollout began on 2026-04-16...
EOF

# Ingest one transcript (hooks do this automatically for new sessions)
memex ingest --agent codex /abs/path/session.jsonl --collection team-a
memex ingest /abs/path/session.jsonl              # infer the agent from content

# Ingest a web page or document via any Markdown converter
markitdown https://example.com/post | memex ingest --source https://example.com/post

# Interactive ingest: propose pages, approve, write each
# /memex-ingest https://example.com/post
```

**Collections** organize documents. Pass `--collection <name>` (repeatable) to `ingest`, `backfill`, and `query`. With none: `ingest`/`backfill` assign `default`; `query` searches `default`.

## 🔧 How it works

1. **Hook.** When an agent session ends, a hook ships the transcript to the daemon (`memex ingest`).
2. **Clean.** The daemon strips tool output, metadata, and system tags down to the actual signal.
3. **Extract.** It splits the transcript into model-sized chunks and asks the worker LLM to distill each into wiki pages — one page per subject, a page per person, dated events collected into a `## Timeline`.
4. **Merge.** Pages that overlap existing wiki content get folded in by a second, per-page LLM pass.
5. **Store + index.** Pages are written as Markdown, indexed with BM25, and embedded for vector search. The Markdown is canonical; the SQLite index rebuilds from it.
6. **Query.** `memex query` retrieves with BM25 + vector + RRF fusion and synthesizes a cited answer.

The worker LLM is **your agent CLI** — `claude -p`, `codex`, or `gemini`, run headless. memex needs no model API key of its own; the CLI handles auth.

## 📦 Install

### npm (recommended)

```bash
npm install -g @xiongzubiao/memex   # binary + model + ORT
memex install                       # register hooks with detected agents
memex doctor                        # verify
```

`npm install -g` puts the platform binary on PATH and downloads the embedding model, tokenizer, and ONNX Runtime into `~/.memex/`. `memex install` auto-detects which of `~/.claude`, `~/.codex`, `~/.gemini` exist, merges hooks into each agent's settings, and (for Claude Code) copies skills into `~/.claude/skills/`. Re-running is idempotent. Target one agent with `memex install --agent claude-code`.

### Build from source

```bash
git clone https://github.com/xiongzubiao/memex.git   # private repo; auth required
cd memex
cargo build --release -p memex-cli
mkdir -p ~/.local/bin && cp target/release/memex ~/.local/bin/
memex install
# Fetch the embedding model + tokenizer + ONNX Runtime via the plugin postinstall
# (cd plugin && npm install), or place them under ~/.memex/models/ manually.
```

### Prerequisites

- **macOS or Linux.** Windows isn't supported yet (the daemon needs Unix sockets + flock) — run inside WSL2.
- **Rust 1.80+** — only to build from source ([rustup.rs](https://rustup.rs)).
- **At least one agent CLI** (Claude Code, Codex, Gemini CLI, OpenClaw, Hermes, or OpenCode) for synthesis and ingestion. `memex query --raw` and `memex search` work without one.
- **Optional — embedding model.** `~/.memex/models/embedding-gemma-300m.onnx` plus its tokenizer. Without the model, memex falls back to hash-based embedding (BM25 still works; vector search degrades). The npm postinstall fetches both.
- **Optional — a Markdown converter** for ingesting web pages / PDFs / docs, e.g. [`markitdown`](https://github.com/microsoft/markitdown) (`uv tool install 'markitdown[all]'`). memex expects clean Markdown on stdin; it doesn't fetch or convert.
- **Optional — `agent-browser`** for JS-rendered or login-walled pages (used by the `/memex-ingest` skill when a converter returns empty output).

## ⚙️ Configure

`~/.memex/config.toml` is optional — every default is sensible:

```toml
[daemon]
idle_timeout_min = 15

[daemon.worker]
backend = "claude-code"      # claude-code, codex, or gemini-cli
model = "claude-sonnet-4-6"  # agent-specific model name
max_count = 8                # worker pool cap (default: CPU count)
timeout_sec = 300            # per-turn read timeout

[query]
top_k = 10
```

### Authenticate the agent CLI

memex runs the agent CLI as a subprocess; the CLI owns its auth, so memex needs no key of its own.

- **Claude Code** — `claude /login` (or `CLAUDE_CODE_OAUTH_TOKEN`)
- **Codex** — `codex login` (or `OPENAI_API_KEY`)
- **Gemini CLI** — `gemini` (auth on first run) (or `GEMINI_API_KEY`)

## 🔄 Daemon lifecycle

The daemon auto-spawns on first use (`query`, `ingest`, `backfill`) and exits after `idle_timeout_min` minutes idle. Manual control:

- `memex daemon start` — foreground
- `memex daemon stop` — SIGTERM, waits up to 10s
- `memex daemon status` — PID + ping health check

## 🧱 Architecture

- `core/` — `memex-core`: BM25 + vector search, embedding, storage, transcript parsing.
- `cli/` — `memex-cli`: the `memex` binary (CLI + daemon).
- `plugin/` — npm package for agent plugin installation and hooks.
- `docs/specs/` — the design docs.

## 🩺 Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `auth_failed` | Agent CLI login expired or missing — run its login command. |
| `subprocess_crashed` | Agent CLI crashed or exited non-zero — check `~/.memex/daemon.log`. |
| `retrieval_empty` | No indexed pages match — confirm `~/.memex/wiki/` has content. |
| `lock busy` | Another daemon is already running — `memex daemon stop`. |
| Low-quality vector results | ONNX model not loaded — fetch it to `~/.memex/models/` and restart the daemon. |

## ✅ Tests

```bash
cargo test --workspace --release
```

Tests that need the embedding model skip cleanly when it's absent, so a fresh clone runs the suite without setup. Install the model (via the plugin postinstall) to exercise the embedding paths.
