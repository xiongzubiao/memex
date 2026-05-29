# Memex Architecture — Filesystem-Canonical, SQLite-Derived

**Status:** Living design doc — describes the system as implemented.
**Last updated:** 2026-05-29

This is part of the consolidated memex design set. See [`README.md`](README.md) for the index and the list of dated specs this replaces.

## Overview

Memex is a personal/agent knowledge wiki built automatically from coding-session transcripts and ingested documents, with hybrid retrieval and LLM synthesis on top. Two design principles run through the whole system:

1. **Filesystem-canonical, SQLite-derived.** Markdown bodies on disk are the source of truth. `index.db` (SQLite) is a *derived* index — embeddings, full-text, chunk offsets, job state — that can be rebuilt from disk at any time. No body text is duplicated into SQLite; chunk text is sliced from the on-disk file via byte `(pos, len)`.
2. **Daemon is the single writer; CLI commands are thin clients.** A persistent daemon owns all mutations (warm embedder, warm worker pool, single SQLite writer). `read` runs in-process; `search` and `query` route through the daemon to reuse the warm embedder (even though they're read-only), as do all mutations. See [`concurrency.md`](concurrency.md), [`daemon.md`](daemon.md), and [`query.md`](query.md).

## File layer — `$MEMEX_ROOT`

```
$MEMEX_ROOT/
  wiki/<slug>.md                 # curated subject pages (flat, depth 1)
  raw/<hash[..2]>/<hash[2..]>    # content-addressed raw sources (depth 2)
  index.db                       # derived SQLite index (local, gitignored)
  config.toml                    # locking + worker config
```

`wiki_dir` and `raw_dir` default to `wiki/` and `raw/` under the root and can be overridden in `config.toml` (`core/src/config.rs`). Path construction lives in `core/src/wiki.rs` (`wiki_path_for_slug`, `normalize_slug`) and `core/src/raw.rs` (`raw_path_for_hash`, two-char hash fanout).

### Wiki pages

A wiki page is a markdown file named by its slug. Frontmatter (`PageFrontmatter`, `core/src/types.rs`):

```yaml
---
title: <string>            # required
summary: <string>          # optional — omitted entirely when absent
collections: [<string>]    # default []
created_at: <ISO 8601 UTC> # preserved across merges
updated_at: <ISO 8601 UTC> # bumped on every write
sources:                   # provenance identifiers (source path / URL) each page draws from
  - sources/documents/<hash>.md
---
```

There is **no `tags` field** — collections replaced tags. The body is markdown organized into topical H2 sections, with an optional `## Timeline` H2 (mandatory for transcript-derived subject pages with dated action-events). Cross-references use `[[other-slug]]`. Page-authoring rules and the Timeline contract live in `cli/src/daemon/worker/prompt.txt`; the ingest flow is in [`ingest.md`](ingest.md).

### Raw documents

Raw sources are content-addressed: the file lives at `raw/<sha256[..2]>/<sha256[2..]>` and is assembled from a `RawFrontmatter` header plus the original content (`core/src/raw.rs`). Content addressing gives free dedup — the same content at a different path or with a different mtime resolves to the same file.

### Wiki vs. raw — different audiences

Wiki pages are LLM-distilled summaries optimized for retrieval handles (specific dates, names, numbers). Raw documents are the authoritative originals that retain every detail. At query time both are first-class evidence: a wiki page may omit a fact the raw source still contains (see [`query.md`](query.md), SYNTHESIZE).

### What does *not* live on disk

Chunk text (sliced from the body file via `(pos, len)`), embeddings, and full-text indexes all live only in `index.db` and are rebuildable.

## Data model — SQLite (`index.db`, derived)

Schema in `core/src/schema.rs`. `doc_type` is `wiki` or `raw` (a CHECK constraint enforces the pair).

| Table | Key columns | Purpose |
|---|---|---|
| `documents` | `id, doc_type, path, title, hash, source, mtime, size, embed_model, embedded_at` | One row per on-disk file. **No `tags` column.** |
| `chunks` | `hash, seq, pos, len` | Byte offsets of each chunk within its body file. **No chunk text stored.** |
| `chunks_vec` | `hash_seq, embedding` | 768-dim vector per chunk (sqlite-vec). |
| `titles_fts` | `title` | FTS5 over wiki titles — for `memex search <title>` / dedup lookups. |
| `chunks_fts` | `chunk_text, hash UNINDEXED, seq UNINDEXED` | FTS5 over chunk text — chunk-level BM25. |
| `llm_cache` | `hash, result, created_at` | Caches worker LLM results (keyed on model-pinned hash). 90-day prune (`ingest_jobs` is the one pruned at 30 days). |
| `ingest_jobs` | `job_id, job_type, source_path, agent, content_hash, collections, status, created_at, updated_at, error` | Durable ingest job tracking. |
| `collections`, `document_collections` | — | Named collections and the document↔collection join. |

The older single `documents_fts(path, title, tags, body)` table was retired in favor of `titles_fts` + `chunks_fts`. Chunk bodies are not stored; FTS5 `chunks_fts` is the only place chunk text is materialized, and even that is a derived index.

### Docids

Git-style short hashes: 7-char prefix of the content SHA-256 (`core/src/docid.rs`, `SHORT_LEN = 7`). A user-supplied prefix resolves via `WHERE hash LIKE ?||'%'`, returning `Ambiguous` with candidates on collision.

### Mutation ordering (crash safety)

Bodies on disk are truth, so the body file is written **before** the SQLite row is committed (`core/src/storage.rs::atomic_write`; ingest store ordering in `cli/src/daemon/handler/ingest.rs`). A crash between the two leaves an on-disk file with no index row, which reconciliation recovers. The reverse order would leave a row pointing at no file. See [`concurrency.md`](concurrency.md) for the atomic-write protocol.

## Reconciliation (disk ↔ index)

`core/src/reconcile.rs` rebuilds/repairs the index from disk.

- **When it runs:** daemon startup, filesystem-watcher events, a periodic interval (`daemon.reconcile_interval_sec`, default 1 h), and `memex lint --fix`.
- **Algorithm:** walk `wiki/` (depth 1) and `raw/` (depth 2); for each file compare `(mtime, size)` to the indexed row and re-hash only on change; upsert changed/new files, delete index rows whose file is gone.
- **Safety threshold:** abort the pass if the deletion set exceeds `max(10, 0.5 × existing_count)` — a guard against wiping the index when the root is transiently empty or mis-pointed.
- **Symlinks:** the root may be a symlink (resolved once); internal symlinks are skipped.
- **Recovery:** a missing or corrupt `index.db` is rebuilt from disk wholesale — the derived index is disposable.

## Lint & health

`memex lint` runs deterministic disk↔index checks (`core/src/lint.rs`) and reports seven issue kinds:

- **Auto-repaired by `lint --fix`:** `StaleIndex` (on-disk body changed), `UntrackedFile` (file with no index row), `MissingFile` (row with no file), `OutdatedEmbedding` (chunks embedded by an older model), `RawHashMismatch` (a raw file's body no longer matches its content-addressed name).
- **Report-only:** `DanglingLink` (`[[slug]]` pointing at a missing page) and `MissingLink` (a body mentions another page's title without linking it). Lint never auto-edits links — choosing between rewrite, re-point, restore, or create needs LLM judgment, so that belongs to write/ingest, not the deterministic fixer.

`lint --fix` is passive sync only (no auto-cross-linking); it routes through the daemon and applies each fix under the writer lock (see [`concurrency.md`](concurrency.md)).

## Cross-linking (summary)

On write/ingest the daemon maintains the wiki link graph deterministically (no LLM): `forward_link` (link existing subjects mentioned in a new body), `maintain_backlinks_batch` (add backlinks on existing pages that mention a new slug), and `suggest_create` (surface dangling `[[ ]]` targets). All gated by `auto_link_eligible` — only multi-token stems link, to avoid false positives on common single words. The filesystem watcher and `lint --fix` do **not** auto-link. Mechanics live in [`ingest.md`](ingest.md).
