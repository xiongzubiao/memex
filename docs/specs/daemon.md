# Daemon & IPC Protocol

**Status:** Living design doc — describes the system as implemented.
**Last updated:** 2026-05-29

Part of the consolidated memex design set ([`README.md`](README.md)). The daemon is **shared infrastructure** — every mutation (ingest, write, delete, `lint --fix`) and every query routes through it. Query/retrieval is documented in [`query.md`](query.md); the ingest pipeline in [`ingest.md`](ingest.md); locking in [`concurrency.md`](concurrency.md).

## The daemon

A persistent process that owns all mutations and hosts the warm embedding model and worker pool. CLI commands connect over a Unix socket (newline-delimited JSON); `connect_or_spawn` auto-starts the daemon if it isn't running. Code under `cli/src/daemon/`.

- **Single instance / single writer.** Guarded by `~/.memex/daemon.lock` ([`concurrency.md`](concurrency.md)).
- **Eager model load.** The embedding model loads at startup and is fatal if missing — every query and ingest reuses the one warm model rather than cold-starting ONNX per invocation.
- **Idle exit.** The daemon process exits after ~15 min idle (`idle_timeout_min`, default 15); the next CLI call respawns it.
- **Filesystem watcher.** Watches `$MEMEX_ROOT` for external edits (native OS notifications) and triggers reconciliation. On network filesystems, where native events are unreliable, it auto-detects and falls back to polling (`cli/src/daemon/watcher.rs`, `cli/src/daemon/fs_kind.rs`).

### Worker pool

Workers are subprocesses that run the LLM tasks. Backends (`cli/src/daemon/worker/`): `claude` (claude-code), `codex`, `gemini-cli`, and `openai-api`. The pool:

- Restarts a worker subprocess on `count` (after ~100 jobs), `context` (accumulated context near the model limit), or `fit_miss` (next prompt wouldn't fit alongside accumulated context — see [`ingest.md`](ingest.md)).
- Applies a per-job timeout (default 300 s); reaps an idle worker subprocess after ~600 s (`idle_reap_sec`).
- Sets `MEMEX_INTERNAL=1` on every worker subprocess so the session hook never re-ingests the daemon's own LLM sessions.

### Worker tasks

The worker system prompt (`cli/src/daemon/worker/prompt.txt`) defines four tasks, selected by a tag at the start of each message: **EXPAND** and **SYNTHESIZE** (query path — see [`query.md`](query.md)) and **EXTRACT** / **MERGE** (ingest — see [`ingest.md`](ingest.md)).

## IPC protocol

`cli/src/daemon/protocol.rs`. Requests are a tagged enum discriminated by `op`; there is **no protocol version field and no per-request `memex_root`** — a daemon is bound to a single root.

**Requests:** `Ping`, `Query { question, raw, top_k=10, collections, intent }`, `Write { title, content, source?, force }`, `Ingest { source: IngestSource, collections }`, `SourceAdd`, `SourceDelete`, `Delete { slug, force }`, `Search { title }`, `LintFix`, `SourcePlan { source_id }`, `PlanApply { plan_json }`.

**Events** (a request streams multiple): `Pong`, `Queued`, `Answer`, `Expansion`, `Context`, `Error`, `Done`, `Written`, `Deleted`, `LintResult` / `LintFixed` / `LintAlreadyFixed` / `LintRemaining`, `Parsing`, `Distilling`, `Stored`, `SourceAdded`, `SourceDeleted`, `SearchResult`, `PlanContent`, `EmptyExtract`, `PlanApplied`.

CLI mutation commands map 1:1 onto these (`write → Write`, `delete → Delete`, `lint --fix → LintFix`, `ingest → Ingest`, etc.). `search` and `query` are also daemon-routed (read-only, but they reuse the warm embedder rather than reloading ONNX per CLI call); only `read` stays in-process.

## Warm embedding model

`embedding-gemma-300m`, 768 dimensions, 2048-token context (`core/src/embed.rs`: `CURRENT_MODEL_NAME`, `EMBEDDING_DIM`, `EMBED_CONTEXT_SIZE`), run via ONNX runtime and kept warm in the daemon. Shared by both consumers: ingest embeds chunk bodies; query embeds the query/expansion text for vector search ([`query.md`](query.md)).
