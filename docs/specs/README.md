# Memex Design Docs

This folder is the **current, living design** of memex. It was consolidated on 2026-05-29 from a series of dated specs (each describing one increment of the system); those originals are retained in git history. Docs here describe the system **as implemented** — where the code and a description diverge, the code wins and the doc is a bug.

Memex is a personal/agent knowledge wiki built automatically from coding-session transcripts and ingested documents, with hybrid retrieval and LLM synthesis on top. Markdown bodies on disk are the source of truth; SQLite is a derived, rebuildable index; a daemon is the single writer and CLI commands are thin clients.

## Documents

| Doc | Covers |
|---|---|
| [`architecture.md`](architecture.md) | Filesystem-canonical principle, `$MEMEX_ROOT` layout, wiki/raw files, SQLite data model, docids, reconciliation, cross-linking summary |
| [`concurrency.md`](concurrency.md) | Daemon-single-writer model, locks, atomic-write protocol, config, error/exit codes |
| [`daemon.md`](daemon.md) | Daemon process & worker pool, IPC protocol, worker tasks, warm embedding model (shared by ingest and query) |
| [`query.md`](query.md) | Query path, hybrid retrieval (BM25 + vector + RRF), strong-signal gate, snippets/intent, `memex-query` skill |
| [`ingest.md`](ingest.md) | Full ingest pipeline: inputs, cleaning, chunking, EXTRACT (Mode A/B), MERGE, dedup, store, auto cross-linking, collections, interactive plan flow |
| [`packaging.md`](packaging.md) | Per-platform npm distribution, install/doctor/hook CLI, hooks, marketplace, release procedure |

## CLI commands

Top-level (`cli/src/main.rs`): `write`, `read`, `search`, `query`, `ingest`, `backfill`, `source` (`add`/`list`/`show`/`delete`/`plan`), `plan` (`show`/`apply`), `delete`, `lint [--fix]`, `rechunk`, `status`, `doctor`, `install`, `uninstall`, `hook ingest <agent>`, `daemon` (`start`/`stop`/`status`). `read` runs in-process; `search` and `query` route through the daemon (to reuse the warm embedder), as do all mutations.

## Skills

- **`memex-query`** — issues `Request::Query`, renders the synthesized answer (or raw context).
- **`memex-ingest`** — interactive ingest via the daemon plan flow (`source add` → `source plan` → review/edit → `plan apply`).
- **`memex-brainstorm`** — multi-LLM brainstorming via cross-CLI calls.

## Deferred / roadmap

Carried over from the old `TODO.md` and tracked follow-ups:

- **Merge Mode A and B in the EXTRACT task** — unify the transcript/document mode split.
- **`TaskKind::Rerank` for `--raw`** — a rerank pass on raw retrieval output.
- **Per-write journal for `plan apply` crash atomicity** — close the microsecond window where a daemon crash between a wiki write and the stdout flush leaves the wiki updated but the plan file showing the proposal as uncommitted (retry → double-merge). Daemon journals "about to commit slug X with body Y" before each write; reconcile on restart. Deferred until the failure mode is observed.
- **Per-account context probe** — replace the unconditional 200k context derate so accounts with usage credits get larger EXTRACT chunks.
- **Indexing-tail progress logging** — `embed_and_mark` + `maintain_backlinks_batch` can run for many minutes silently on a large transcript; surface progress.
- **Per-ingest concurrency cap** — a large chunk count can monopolize the worker pool and hit per-org throttling.
- **Slug drift across chunks** — the same subject under slightly different slugs escapes by-slug consolidation.

## Lineage

This set replaces these dated specs (kept in git history):
`2026-04-07-memex-design`, `2026-04-16-parallel-access-design`, `2026-04-17-memex-query-daemon-design`, `2026-04-19-daemon-ingestion-design`, `2026-04-22-document-collections-design`, `2026-04-24-web-page-ingestion-design`, `2026-04-25-filesystem-canonical-architecture`, `2026-04-30-memex-ingest-skill-redesign`, `2026-05-07-plugin-publish-fix-design`, `2026-05-21-chunked-ingest-design`, and `TODO.md`.
