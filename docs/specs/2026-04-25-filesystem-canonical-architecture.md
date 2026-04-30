# Memex Architecture: Filesystem-Canonical, SQLite-Derived

**Status:** DRAFT (complete rewrite — supersedes prior architecture)
**Date:** 2026-04-25, last revised 2026-04-26

## Overview

Memex's storage model becomes filesystem-canonical, SQLite-derived. The filesystem under `$MEMEX_ROOT` is the source of truth; SQLite is a rebuildable index of metadata, full-text, and embeddings.

Two concrete file trees:

```
$MEMEX_ROOT/
  wiki/                          # curated subject pages (LLM-distilled or hand-edited)
    auth-tokens.md
    rest-patterns.md
    ...
  raw/                           # raw ingested documents, hash-addressed
    ab/
      cdef0123456789...          # filename = sha256(body), no extension
      f9012345678abc...
    7e/
      abcdef0123456...
  index.db                       # SQLite: metadata + FTS5 + sqlite-vec
```

This rewrite consolidates several improvements derived from comparing memex with [QMD](https://github.com/tobi/qmd) (HEAD `e8de7ca`, mirrored at `/data/MemVerge/qmd`):

1. **Focused snippets + intent.** Replace memex's "snippet = full chunk text" with QMD-style query-aware focused excerpts (~300 chars, line-numbered, diff-header-prefixed). Add an `intent` parameter that disambiguates queries through expansion, rerank, and snippet scoring.
2. **Ranged read.** Add `memex read --from-line N --max-lines M` for single-doc slice retrieval. Doc-relative line numbers from focused snippets feed directly into `--from-line`.
3. **Chunking-quality refactor.** Replace per-byte break-scoring walk with QMD's pre-scanned break points + backward-only search window + squared-distance decay. Bumps `CHUNK_ALGO_VERSION` and triggers re-embed via `lint --fix`.
4. **LLM result cache.** Adopt QMD's `llm_cache` table to dedupe LLM API calls.

These all sit on top of the filesystem-canonical storage foundation in Sections 1-6.

## Design lineage

This spec adopts QMD's retrieval improvements (focused snippets, intent disambiguation, RRF, llm_cache schema — see References). The filesystem-canonical storage model follows Karpathy's "LLM wiki" idea (markdown files as the canonical record; agents distill knowledge into them) and is mentioned in memex's original design spec (`docs/specs/2026-04-07-memex-design.md`). Architecturally similar to Obsidian (vault + cache) and Org-roam (.org files + SQLite metadata).

Memex's distinctive contribution: agentic ingest pipeline (daemon-driven Extract+Merge from session transcripts, web pages, etc.) producing wiki bodies from raw source material. That pipeline lives in earlier specs and is out of scope here.

## Goals

- **Reduce per-query token cost** of the search response by ~12× via focused snippets (~300 char excerpts vs. ~3600 char full chunks).
- **Improve query-relevance focus** — agent sees the lines most likely to match the query.
- **Add disambiguating context** via `intent` so queries like "performance" can be steered toward the intended sense.
- **Allow agent-driven snippet expansion** by passing the doc-relative `@@ -N,M @@` line range from a search snippet directly into `memex read --from-line N --max-lines M`.
- **Improve chunk boundary alignment** with structural breakpoints; bound chunk size predictably from above.
- **Wiki pages editable in external editors** (Obsidian, vim, etc.) — wiki/ is human-readable Markdown with frontmatter; the daemon picks up external edits via filesystem watcher.
- **Cloud-sync friendly for wiki/ and raw/** — these directories can be synced via Syncthing/Dropbox/iCloud independently; each machine derives its own `index.db`.
- **Network-drive friendly for wiki/ and raw/** — these paths can live on NFS/SMB. `$MEMEX_ROOT` itself (which contains `index.db`) stays local.
- **Cache LLM API calls** to avoid re-paying for identical prompts.

## Non-goals

- **Body inclusion in search response.** QMD's MCP query tool ships no body to the agent (`src/mcp/server.ts:321-361`). Memex matches: search returns refs + snippets; bodies fetched via `memex read` if needed.
- **AST-aware chunking.** No plan to ingest source code yet.
- **Time-decay RRF, exact-keyword boost, source-first retrieval rebalancing.** Independent retrieval-quality items, separate specs once benchmarks justify them.
- **Migration from older memex installs.** Memex is in initial development; existing installs re-ingest from disk or start fresh. No `PRAGMA user_version`, no schema_version checks, no migration scripts.
- **Promotion from raw → wiki.** Wiki pages are produced by the LLM Extract pipeline from raw source material (or written by the user explicitly). There is no user-facing "promote this raw doc to a wiki page" workflow — that's what `memex ingest` does.
- **Embedding-model switching.** This spec assumes one embedding model (`CURRENT_MODEL_NAME`). Cost-bounded rebuild and per-provider tuning are out of scope.
- **`$MEMEX_ROOT` on network drives.** `index.db` (SQLite) doesn't tolerate network filesystems reliably (WAL semantics, locking). `$MEMEX_ROOT` stays local. `wiki/` and `raw/` paths can be configured independently to live on network drives if desired.

---

## Section 1 — File layer

### 1.1 Layout under `$MEMEX_ROOT`

```
$MEMEX_ROOT/                        # local-only (contains index.db)
  wiki/                             # default: curated subject pages
    <slug>.md                       # flat layout (no subdirs in v1)
  raw/                              # default: raw ingested documents
    <hash[..2]>/                    # 2-char prefix subdir for filesystem fanout
      <hash[2..]>                   # filename = sha256(body); file = frontmatter + body
  index.db                          # SQLite: documents, chunks, FTS5, sqlite-vec, llm_cache
```

**`$MEMEX_ROOT` is local.** Default `~/.memex/`. `[storage] memex_root = "..."` in `~/.memex/config.toml` overrides. **Must be on a local filesystem** because `index.db` (SQLite) is here, and SQLite's WAL + locking semantics break on NFS/SMB.

**`wiki/` and `raw/` paths are independently configurable.** Defaults are `$MEMEX_ROOT/wiki/` and `$MEMEX_ROOT/raw/`. Override in `config.toml`:

```toml
[storage]
memex_root = "~/.memex"            # local; holds index.db
wiki = "/Users/me/Notes/wiki"      # can be on iCloud/Syncthing/Obsidian-vault for cross-machine sync
raw = "/mnt/nas/memex-raw"         # can be on NFS/SMB for shared archive
```

When wiki or raw is configured to a non-default location, `$MEMEX_ROOT/wiki` and `$MEMEX_ROOT/raw` aren't created — the configured paths are used directly.

This split lets users sync wiki/ for editorial workflows (edit in Obsidian on Mac, query on Linux) while keeping `index.db` local where SQLite works correctly.

### 1.2 Wiki pages

Wiki pages live at `$MEMEX_ROOT/wiki/<slug>.md`. Slugs are kebab-case, ASCII, no extension in path lookups but `.md` on disk.

Format: standard YAML frontmatter + Markdown body.

```markdown
---
title: Auth Tokens
tags:
  - concept
created_at: 2024-01-01T00:00:00Z
updated_at: 2024-01-01T00:00:00Z
sources:
  - "#abc123"            # optional: docids of raw documents this page distilled from
summary: "How bearer tokens are issued and validated."
---

# Auth Tokens

Body content...
```

Memex `write` and the `/memex-ingest` skill produce these files. Users may also edit them with any text editor; memex re-indexes on the next reconciliation pass (Section 5).

### 1.3 Raw documents

Raw documents are content piped through a converter (markitdown, pandoc, etc.) and ingested via `memex source add` or `memex ingest --source`. They live at `<raw_dir>/<hash[..2]>/<hash[2..]>`.

**YAML frontmatter (write-once) + body bytes.** The frontmatter holds provenance metadata; the body is the converter's output verbatim. Frontmatter and body are separated by the standard `---` fence.

```markdown
---
source: https://example.com/articles/auth-tokens-explained
source_kind: url                          # url | path | transcript | other
ingested_at: 2026-04-26T10:30:00Z
converter: markitdown
title: Auth Tokens Explained
---

# Auth Tokens Explained

Article body...
```

For transcripts the frontmatter additionally carries `agent` and `session_id`:

```markdown
---
source: claude-code-session-2026-04-26-7c3e
source_kind: transcript
ingested_at: 2026-04-26T11:15:00Z
agent: claude-code
session_id: 7c3e...
title: Auth implementation discussion
---

# Conversation transcript...
```

**Hash semantics:**
- **Filename hash** (`<hash[..2]>/<hash[2..]>`) = `sha256(body)` where `body` is the post-frontmatter content. Filename hash ≠ `sha256(file_bytes)` because the file also carries the frontmatter prelude. Document this explicitly: `memex verify` computes the body hash; generic `sha256sum file` does not match the filename.
- The 2-char prefix subdir avoids filesystem fanout problems for ~256K+ raw docs.

**Write-once invariant.** All frontmatter fields (including `ingested_at`) are written exactly once at first ingest. Re-ingesting an identical body is a strict no-op: filename already exists, file is left untouched. This keeps raw files write-once-per-body so backup/sync tools (rsync, restic, Dropbox) don't see spurious churn. If a user explicitly re-fetches a source and the body has changed (different hash), it's a different file at a different path; the old file is left as-is or removed by reconciliation.

### 1.4 Wiki vs raw — different audiences

The split reflects two different audiences and editorial models:

**`wiki/` is human-readable, human-editable.** Slug-named filenames, YAML frontmatter (title, tags, summary, sources, dates), Markdown body. Users open these in Obsidian, vim, or VS Code and edit them like any other notes file. The daemon picks up external edits via filesystem watcher (Section 5).

**`raw/` is LLM-fed.** Hash-named filenames, frontmatter holds provenance, body is the artifact. The LLM reads these files for snippet extraction; `fetch_body` strips frontmatter so metadata never enters the context window (Section 6.3). The Extract pipeline reads them when distilling a wiki page. Users don't navigate `raw/` directly — they use `memex source list` / `memex source show <docid>` to inspect — but if they need to fix a bad source URL by hand, opening the file works.

Frontmatter on raw lets `cp -r raw/ ~/backup/` carry provenance with bodies as a single tree. No companion catalog file to forget.

### 1.5 What does NOT go in the file layer

- **Embeddings.** Stay in SQLite (`chunks_vec` virtual table). Reasoning: embeddings are large binary blobs, awkward in cloud-sync tools, and the DB rebuild story (Section 5.4) acknowledges that re-embedding is the price of DB loss. Cost: ~10 ms per chunk × N chunks; paid automatically when the daemon detects a missing or corrupt `index.db` at startup and rebuilds from the filesystem.
- **FTS5 inverted index.** Stays in SQLite. Trivially rebuildable from filesystem.
- **Indexed metadata** (title, tags). Stays in SQLite — needed at query time without re-parsing frontmatter.

The filesystem holds **bytes humans care about** (and the metadata to identify them). SQLite holds **derived structures the daemon needs for fast retrieval**.

---

## Section 2 — SQLite schema

### 2.1 Tables

```sql
-- Documents (wiki + raw), keyed by file location
CREATE TABLE IF NOT EXISTS documents (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_type     TEXT NOT NULL CHECK (doc_type IN ('wiki', 'raw')),
    path         TEXT NOT NULL,          -- relative path under $MEMEX_ROOT (or absolute when wiki/raw is configured outside)
    title        TEXT NOT NULL,          -- from frontmatter or first H1 of body
    hash         TEXT NOT NULL,          -- sha256 of post-frontmatter body (matches raw filename for raw docs)
    tags         TEXT NOT NULL DEFAULT '',  -- comma-separated; from wiki frontmatter (empty for raw)
    source       TEXT,                   -- raw only: from frontmatter; NULL for wiki
    mtime        TEXT NOT NULL,          -- file mtime at last index pass (change detection)
    size         INTEGER NOT NULL,       -- file size at last index pass (change detection)
    embed_model  TEXT,                   -- NULL until embedded
    embedded_at  TEXT,                   -- NULL until embedded
    UNIQUE(doc_type, path)
);

CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(hash);
CREATE INDEX IF NOT EXISTS idx_documents_path ON documents(path);

-- Collections (M:N tag-style)
CREATE TABLE IF NOT EXISTS collections (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    name    TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS document_collections (
    document_id     INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    collection_id   INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    PRIMARY KEY(document_id, collection_id)
);

-- Chunk metadata (slice coordinates only)
CREATE TABLE IF NOT EXISTS chunks (
    hash    TEXT NOT NULL,
    seq     INTEGER NOT NULL,
    pos     INTEGER NOT NULL,           -- BYTE offset of chunk start in body (not character offset)
    len     INTEGER NOT NULL,           -- BYTE length of the chunk
    PRIMARY KEY(hash, seq)
);

-- sqlite-vec virtual table for embeddings
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    hash_seq TEXT PRIMARY KEY,
    embedding float[768] distance=cosine
);

-- FTS5 inverted index (contentless — bodies live in files)
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    path, title, tags, body,
    content='',
    tokenize='porter unicode61'
);

-- LLM result cache
CREATE TABLE IF NOT EXISTS llm_cache (
    hash        TEXT PRIMARY KEY,        -- sha256(model + task + system + user)
    result      TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

-- Daemon ingest job queue (async LLM ingest pipelines: transcripts + documents)
CREATE TABLE IF NOT EXISTS ingest_jobs (
    job_id        TEXT PRIMARY KEY,
    job_type      TEXT NOT NULL CHECK (job_type IN ('transcript', 'document')),
    source_path   TEXT NOT NULL,
    agent         TEXT,
    content_hash  TEXT NOT NULL,
    collections   TEXT NOT NULL DEFAULT '[]',
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'processing', 'completed', 'failed')),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    error         TEXT
);
```

**`ingest_jobs` retention.** Daemon startup prunes terminal rows: `DELETE FROM ingest_jobs WHERE status IN ('completed', 'failed') AND updated_at < datetime('now', '-30 days')`. Without this, the table grows unboundedly with each ingest. Pending and processing rows are never pruned (they're in-flight work). The 30-day window keeps a useful audit trail for debugging recent failures while bounding growth.

**Stuck-job recovery.** Also at daemon startup, after the prune: any rows still in `pending` or `processing` were left there by a crash (or a kill -9) before the worker handed back a result. Mark them `failed` with `error = 'interrupted by daemon restart'` and `updated_at = datetime('now')`. The `updated_at` bump is intentional — it resets the 30-day prune window for the recovered row, so a user investigating a crash that happened ten minutes ago has the full debugging window starting now, not "the row's already 28 days old, you have 2 days left." Cost: failed-by-recovery rows persist for up to 30 more days. For typical usage (rare crashes) this is bounded; for a flapping daemon (worst case) the table grows by `~recovery_count_per_30_days × in_flight_per_crash` rows, still bounded.

The recovery does NOT re-dispatch the work. Rebuilding the worker payload would require re-reading the source path (which may be gone, moved, or inaccessible) and re-running parse + render, which can fail in different ways than the original; the user can re-run `memex ingest` if they still want the work done.

### 2.2 What's gone vs. today's memex

| Removed | Why |
|---|---|
| `content` table (and `content.doc`) | Bodies live as files in `$MEMEX_ROOT/wiki/` and `$MEMEX_ROOT/raw/`. The other columns (`model`, `embedded_at`) move to `documents` as `embed_model` / `embedded_at`. The hash-keyed dedup property is preserved implicitly: chunks/chunks_vec are still keyed by hash, so identical-body documents share embeddings. |
| `chunks.chunk_text` | Slice the file at query time using `pos` + `len`. |
| `chunks.model`, `chunks.embedded_at` | Per-body invariants. Moved to `documents.embed_model` / `documents.embedded_at`. |
| `documents.active` | Memex doesn't soft-delete; deletes are hard. |
| `documents.summary` | Not on any query path; FTS searches title/tags/body, snippet comes from focused extraction. Extract on demand from frontmatter if a future feature needs it. |
| `documents.source_kind` | Derivable from `documents.source` (URL prefix, absolute path, etc.). |
| `documents.created_at`, `documents.updated_at` | Redundant with `mtime` (filesystem-canonical) and frontmatter dates. No query filters by these columns. |
| `documents.docid` (and `idx_documents_docid`) | Replaced with git-style on-demand hash-prefix resolution. The hash itself is the identity; short forms are computed when displaying and resolved by `LIKE 'prefix%'` when looking up. See Section 2.4. |
| `ingest_jobs.memex_root` | Multi-root daemon support is dropped. Each daemon binds to one `$MEMEX_ROOT` (its socket lives at `<memex_root>/daemon.sock`); all jobs in its queue belong to that root by construction. The column was redundant; the protocol field carrying it is also removed. |

### 2.3 What `documents.path` looks like

For wiki: `wiki/auth-tokens.md` (relative to `$MEMEX_ROOT`).

For raw: `raw/ab/cdef0123456789...` (relative to `$MEMEX_ROOT`, no extension).

Path is canonical and uses forward slashes regardless of OS. The platform-native separator is used only when joining with `$MEMEX_ROOT` for actual filesystem operations.

### 2.4 Document identifiers (git-style)

The `hash` column is the document's identity. There is no separate stored docid; short user-facing forms are computed on demand, the way git handles commit hashes.

**Display.** When printing a docid for a document — in `memex source list`, search results, etc. — output `hash[..7]`. Seven hex characters (16⁷ = 268M slots) gives effectively zero collision risk at personal-wiki scale. This matches git's `core.abbrev` default.

**Lookup.** When the user passes a prefix (`memex source show abc1234`):

```sql
SELECT id, hash FROM documents WHERE hash LIKE ? || '%' LIMIT 2
```

- 0 rows → "no document matching `abc1234`"
- 1 row → resolved
- 2 rows → ambiguous; respond with the two matching full hashes and ask the user for a longer prefix

The 7-char display default keeps prefixes short in the common case; the lookup accepts any length from one prefix character (silly but legal) up to the full 64-char hash. A full hash is unambiguous by construction.

**Stability.** A 7-char prefix that worked yesterday could become ambiguous tomorrow if a newly-ingested doc collides on the prefix (same UX as `git log abc1234` after a year of new commits). At memex scale this is rare but possible. The remediation — re-run with a longer prefix — is the same as git's.

**Implementation.** `core/src/docid.rs` exposes `resolve_prefix(conn, prefix) -> Result<DocRef, ResolveError>` and a `short(hash) -> &str` helper that returns `hash[..7]`. The old `allocate_docid` is deleted.

---

## Section 3 — Ingest paths

### 3.1 Wiki write (`memex write <slug>`)

```
1. Read body from stdin or $EDITOR
2. Normalize slug: kebab-case, ASCII, no .md extension in arg
3. Auto-link forward (multi-token slugs only): scan body for mentions
     of existing wiki page titles/stems whose slug contains '-';
     replace the first un-linked occurrence with [[<stem>]]. See §3.7
     for the full auto-cross-link rule.
4. Compute body hash: sha256(linked body bytes, post-frontmatter)
5. Construct frontmatter:
     - title, tags          -- memex indexes these
     - sources              -- backlink list (raw docids the page distilled from)
     - created_at, updated_at  -- Obsidian/editor conveniences; not indexed
     (summary or other fields the user adds by hand are preserved; memex ignores them)
6. Write file: $MEMEX_ROOT/wiki/<slug>.md  (atomic — see "Atomic write protocol" below)
7. Daemon indexes (Section 4)
8. Auto-link backward (multi-token slugs only): sweep other wiki
     bodies for mentions of the new title/stem-as-words; rewrite the
     first un-linked occurrence with [[<new-stem>]] (body-only —
     frontmatter, including updated_at, preserved verbatim) and
     re-embed each rewritten page under the same writer pass. See §3.7.
```

### 3.2 Raw source add (`memex source add <source-id>`)

```
1. Read body from stdin (pre-converted, e.g., from `markitdown $url`)
2. Compute body_hash = sha256(body bytes)
3. Path: <raw_dir>/<body_hash[..2]>/<body_hash[2..]>
4. Check if file exists at that path:
   - If yes: strict no-op. File is left untouched (frontmatter, including ingested_at, is write-once).
   - If no: derive title (rules below); build frontmatter (source, source_kind, ingested_at, converter, title); write file = frontmatter + body via atomic .tmp + fsync + rename + fsync(parent_dir).
5. Print docid to stdout
6. Signal daemon: scan the new path
7. Daemon indexes (Section 4)
```

**Title derivation** (mirrors today's `derive_source_title` at `cli/src/daemon/handler.rs:968`):

1. **First H1 of body.** If the body's first non-empty line is `# Heading`, use the heading text. Markitdown and most converters produce this.
2. **URL slug.** If `source` matches `http(s)://...`, use the slugified last non-empty path component (`.../auth-tokens-explained` → `auth-tokens-explained`).
3. **File stem.** If `source` is a filesystem path, use the basename without extension.
4. **Source string verbatim.** Last resort.

The title is written into frontmatter once at first ingest and never updated by re-ingest (write-once like `ingested_at`). If the user wants a different title, they can edit the frontmatter directly — reconciliation re-reads it (frontmatter-only edit, no re-embed) and updates `documents.title`.

The filename is `sha256(body)`, not `sha256(file_bytes)`. The file IS frontmatter + body, so generic `sha256sum file` returns a different value. `memex verify` does the right thing (strip frontmatter, hash body, compare to filename).

### 3.3 Daemon-driven ingest (`memex ingest --source <id>`)

The existing daemon ingest pipeline (transcript + document Extract+Merge) runs unchanged at the LLM step. Outputs:

- **Raw artifact (the cleaned source).** For transcripts: the redacted/cleaned conversation Markdown (post-secret-redaction pass). For documents: the converter's output. Written to `<raw_dir>/<hash[..2]>/<hash[2..]>` where `hash = sha256(cleaned_body)`. The file is frontmatter + cleaned_body. For transcripts the frontmatter carries `source_kind: transcript`, `agent`, `session_id`.
- **Wiki pages (Extract output).** Written to `<wiki_dir>/<slug>.md` with frontmatter including `sources: [<raw-docid>, ...]` referencing the raw artifacts they distilled from.

Same atomic-write + daemon-scan pattern as direct `memex source add`. ingested_at on raw is write-once.

### 3.4 Atomic write protocol (shared by wiki write, raw add, transcript ingest)

Every file write goes through this sequence:

```
1. Open <dir>/.<basename>.tmp  (hidden tmp file alongside target)
2. Write contents
3. fsync(tmp_fd)              -- contents durable on disk
4. close(tmp_fd)
5. rename(tmp_path, target)   -- POSIX-atomic, same filesystem by construction
6. fsync(<dir>)               -- rename durable in parent dir
```

Step 3 (fsync tmp) and step 6 (fsync parent) are both required: the first guarantees content durability, the second guarantees the directory entry pointing to that content survives a crash. Skipping either leaves a window where the file content or its visibility is undefined after power loss.

Tmp lives in the target's parent directory by construction, so the rename is always within a single filesystem and `EXDEV` is unreachable. (Earlier drafts of this spec proposed an EXDEV fallback for cross-mount `wiki/` or `raw/`; that fallback would never fire under the tmp-alongside-target placement and was dropped during implementation.) If a future change centralizes tmp under `$MEMEX_ROOT/tmp/` or similar, this analysis must be revisited and an `EXDEV` fallback added.

### 3.5 External edits

`memex write` and `memex ingest` are the canonical write paths. Manual edits to `<wiki_dir>/<slug>.md` (vim, Obsidian, etc.) are supported but not first-class: the watcher passively re-indexes the body (no auto-link, no suggest-create — those belong to the LLM-driven write path per §3.7), and any drift surfaces via `memex lint` (stale-index, dangling, missing cross-ref).

External edits to `<raw_dir>/` are similarly non-first-class (raw files are LLM-fed; the only expected hand-edit is fixing a bad source URL in frontmatter). If a raw file's body is modified externally, reconciliation re-computes `body_hash = sha256(body)` and compares to the hash encoded in the file's path (the `<H[..2]>/<H[2..]>` part — that hash IS the file's content-addressed identity). On mismatch (`body_hash != path_hash`), the file no longer matches its name; the daemon logs a warning, skips re-indexing that file, and surfaces it to `memex lint`. `lint --fix` can rename the file to its new body hash (drops the old chunks/embeddings, re-embeds at the new hash). Frontmatter-only edits (e.g., correcting `source:`) leave the body hash unchanged → daemon re-reads frontmatter and updates `documents.source` / `documents.title` without re-embedding.

### 3.6 Wiki delete (`memex delete <slug>`)

Removes a wiki page from `<wiki_dir>/<slug>.md` and from the index. Behavior:

```
1. Resolve <slug> to a documents row (must be doc_type='wiki').
2. Find backlinks: scan wiki bodies for [[<slug>]] occurrences.
   Implementation: SELECT path FROM documents
                   WHERE doc_type='wiki' AND id IN (
                     SELECT rowid FROM documents_fts
                     WHERE documents_fts MATCH '"[[<slug>]]"'
                   );
3. If backlinks exist AND --force is not set:
   - Print the list of referencing pages, exit non-zero:
       Error: 2 wiki pages link to [[<slug>]]:
         wiki/auth-flow.md
         wiki/api-routes.md
       Re-run with --force to delete anyway.
       Dangling links will be reported by `memex lint`.
4. If --force, or no backlinks: existing TTY confirmation prompt, then:
5. Delete file: $MEMEX_ROOT/wiki/<slug>.md
6. Reconciliation removes the documents row, chunks (when no other doc
   shares the hash), chunks_vec, and FTS entry on the next pass.
```

`memex delete --force <slug>` skips both the backlink check and the TTY prompt. The `--force` flag was already wired for the prompt; this extends it to also override the backlink guard.

External `rm` of a wiki file bypasses this check entirely — reconciliation simply removes the row, and `memex lint` reports the resulting dangling `[[<slug>]]` references (existing behavior). The backlink guard is a `memex delete` UX feature, not a constraint enforced by the index.

### 3.7 Auto-cross-linking (forward, backward, suggest-create)

`memex write` and `memex ingest` (the LLM Extract+Merge bulk producer) both run cross-link maintenance over the bodies they emit. The watcher's `Touch` path and reconcile do **not** auto-link — they only reflect disk state. This asymmetry is intentional: the LLM-driven write path produces content with full daemon context, while external edits stay untouched so editor sessions don't see files mutate underneath them.

**Eligibility — multi-token only.** A stem qualifies for auto-linking iff `stem.contains('-')`. Single-token stems — short ones (`api`, `auth`, `bob`, `alice`) and long ones alike (`caching`, `kubernetes`, `performance`, `database`) — are always skipped. Reason: case-insensitive title matching can't tell a navigation cue from generic English prose for single-token names. The LLM is responsible for typing `[[stem]]` explicitly when referring to single-token entities; the `/memex-ingest` skill prompt covers that path.

**Forward link** (in `handle_write` and `store_extracted_pages`): build the candidate set as `(existing wiki pages, by stem) ∪ (new pages in this batch)`, filter by `auto_link_eligible`, scan the body, replace the first un-linked occurrence per target with `[[stem]]`. Match longer titles before shorter ones; skip self-links.

**Backward link** (after the new page is written and indexed): if the new slug is itself eligible, walk `<wiki_dir>/*.md` and apply the same matching logic per existing page against the *new* title/stem. For each match: rewrite the body in place via `replace_body_preserving_frontmatter` (frontmatter, including `updated_at`, preserved verbatim — adding a backlink is not a knowledge-content change), then reindex *and re-embed* under the same writer pass. The re-embed is required for correctness: `commit_doc` drops chunks on hash change, so skipping the embed step would leave the page invisible to vector search until the next reconcile pass.

**Suggest-create**: extract `[[stem]]` references from the (post-forward-link) body, filter against `(existing slugs ∪ new slugs ∪ self)`. Anything left is a typo or a pending-creation gap. Pure information — no body mutation. Reported via `Event::Written.suggest_create` for `memex write`; not surfaced through `Event::Stored` for ingest (per-page list would be noisy in bulk).

**LLM responsibilities**, complementing the daemon's mechanical layer:

- Type `[[stem]]` explicitly for ineligible (single-token) references.
- React to `suggest-create:` in write output — typo, missing page, or intentional placeholder.
- React to `memex lint` findings — `missing-link:` entries that the LLM judges to be genuine navigation cues become explicit `[[stem]]` rewrites; `dangling:` entries get fixed (point at correct slug, remove, or restore the deleted page).
- Semantic linking the daemon's keyword match doesn't cover (synonyms, pronoun coreferences).

The `/memex-ingest` skill prompt spells out title-choice implications (prefer multi-token) and the response paths for each daemon-emitted line.

---

## Section 4 — Indexing pipeline

When a file is detected (newly written, modified, or external edit), the daemon branches on `doc_type` (derived from which configured directory the file lives in):

**Wiki files (`<wiki_dir>/<slug>.md`):**

```
1. Read file bytes from disk
2. Parse YAML frontmatter; extract title, tags
3. body = file content post-frontmatter
4. hash = sha256(body)
5. Compare with existing documents row keyed by path:
   - New: INSERT documents (doc_type='wiki', path, hash, title, tags, source=NULL, mtime, size, embed_model=NULL, embedded_at=NULL) — no docid; identity is hash
   - Hash differs: UPDATE title/tags/hash/mtime/size; delete chunks WHERE hash=old; embed new body
   - Hash matches (mtime/size changed, content same): UPDATE mtime/size only
6. INSERT into documents_fts (path, title, tags, body) — explicit insert in daemon, not via SQL trigger
7. If hash is new (no chunks rows for this hash): chunks = chunk_text(body, 900, 0.15); embed each; INSERT chunks + chunks_vec; UPDATE documents SET embed_model = ?, embedded_at = ? WHERE id = ?
```

**Raw files (`<raw_dir>/<hash[..2]>/<hash[2..]>`):**

```
1. Read file bytes from disk
2. Parse YAML frontmatter; extract `source` and `title` for the documents row. Other frontmatter fields (`source_kind`, `ingested_at`, `converter`, `agent`, `session_id` for transcripts) stay in the file as write-once audit metadata; the index doesn't store them.
3. body = file content post-frontmatter
4. body_hash = sha256(body)
5. Verify body_hash matches the hash encoded in the file's path (integrity check — for content-addressed raw files, this is the canonical "is the file still self-consistent?" test). On mismatch: log warning, skip indexing, surface via `memex lint`.
6. Compare with existing documents row keyed by path:
   - New: INSERT documents (doc_type='raw', path, hash=body_hash, title, tags='', source, mtime, size, embed_model=NULL, embedded_at=NULL) — no docid; identity is hash
   - Same hash already present at a different path (path moved or sync race): UPDATE path
   - Frontmatter changed but body unchanged (e.g., source corrected by hand): UPDATE title/source only
7. INSERT into documents_fts (path, title, tags='', body) — explicit insert
8. If hash is new (no chunks rows for this hash): chunks = chunk_text(body, 900, 0.15); embed each; INSERT chunks + chunks_vec; UPDATE documents SET embed_model = ?, embedded_at = ? WHERE id = ?
```

The FTS5 trigger from today's schema (`core/src/schema.rs:91-115`) is replaced by explicit FTS writes in the daemon's index transaction. The trigger referenced `content.doc` which no longer exists.

**Byte offsets, not character offsets.** `chunks.pos` and `chunks.len` are byte offsets into the body. Rust `&str` slicing operates on bytes; using character offsets would either panic or mis-slice on multibyte content (Chinese, emoji, accented Latin). `chunk_text` produces byte coordinates; `extract_focused_snippet` consumes byte coordinates. A non-ASCII regression test fixture must exercise the full query → snippet path (Section 12.2).

Atomicity: the file write happens before the index update. If the index update fails or crashes mid-way, the next reconciliation catches up. The filesystem is canonical — files don't need DB rows to be valid.

---

## Section 5 — Reconciliation

### 5.1 When reconciliation runs

- **At daemon startup.** Walk `<wiki_dir>` and `<raw_dir>` once before serving queries. If `index.db` doesn't exist, daemon creates the schema and runs a full reconciliation (which becomes a full index build from scratch). If `index.db` is present, the walk catches up on changes that happened while the daemon was off.
- **Filesystem watcher (live).** Daemon registers a watch on `<wiki_dir>` and `<raw_dir>` using platform-native APIs:
  - macOS: FSEvents (via `notify` crate)
  - Linux: inotify (via `notify` crate)
  - Windows: ReadDirectoryChangesW (via `notify` crate)
  - Network drives (NFS/SMB): native watchers don't work reliably; fall back to periodic polling at 5-minute intervals (configurable via `[storage] poll_interval_sec`). Detect "is this a network filesystem" via `statfs` on Linux/macOS or drive type on Windows.
- **On explicit `memex lint --fix`.** Manual reconciliation trigger for the rare cases the watcher and startup-reconcile missed (daemon was off, sync client misbehaved, occasional manual edit per §3.5). `--fix` runs the same reconciliation algorithm to repair drift (stale-index, untracked, missing-file, raw hash-mismatch), plus the audit-only fixes (re-embed outdated). No separate `memex update` verb.
- **NOT on every query.** Queries run against current DB state.

### 5.2 Reconciliation algorithm

```
1. Resolve <wiki_dir> and <raw_dir> via canonicalize() — the configured
   root MAY be a symlink (e.g., wiki = "~/.memex/wiki" → /Users/me/Notes/wiki).
   Walk recursively from the canonical root with follow_links(false). For
   each entry: skip if entry.file_type().is_symlink(). Reject any path
   whose canonical form escapes the resolved root, or whose components
   contain "..". Build seen_paths set.
2. For each file in seen_paths:
   - Stat the file (mtime, size)
   - Look up existing documents row by path
   - If row exists AND row.size == file.size AND row.mtime == file.mtime:
     SKIP (file unchanged since last index)
   - Else: index the file (Section 4)
3. Compute deletion set: docs_in_db_not_seen = SELECT path FROM documents WHERE path NOT IN (seen_paths)

   SAFETY THRESHOLD:
   If deletion_set.size > max(10, existing_docs.count() * 0.5):
     ABORT reconciliation. Print actionable error:
       error: reconciliation would delete N documents (X% of total). Refusing.
              Possible causes: <wiki_dir> or <raw_dir> unmounted, sync moved files away, accidental rm.
              Investigate <wiki_dir> and <raw_dir> contents before retrying.
              If the deletion is intentional: stop the daemon, delete `index.db`,
              and restart — §5.4 auto-rebuild will repopulate from the
              filesystem (which by then matches the user's intent).
     EXIT non-zero. The DB is left in its previous state; queries continue working against pre-reconciliation rows.

   If under threshold (the in-process `force` option in `ReconcileOptions` also
   bypasses; no CLI surface for it today):
     For each path in deletion_set:
       - DELETE documents row (cascades to document_collections)
       - DELETE chunks WHERE hash = ?
       - DELETE chunks_vec WHERE hash_seq LIKE ?_%
```

**Symlink policy.** The configured `wiki_dir` and `raw_dir` themselves MAY be symlinks — useful for pointing memex at an existing Obsidian vault or a shared raw archive without editing `config.toml`. Memex resolves these once at reconciliation startup via `canonicalize()`. **Inside the resolved tree, symlinks are not followed.** A symlink in `wiki/` pointing at `/etc/passwd`, or a symlinked subdirectory, or a relative symlink attempting `../../` — all silently skipped, not indexed. The root-symlink allowance is a UX convenience; the internal-symlink ban is a security boundary.

**Network-drive failure modes.** If a `walkdir` step returns ENOENT/EIO (NFS/SMB drop mid-walk), log the error and fail (per user decision D6). Do not retry, do not fall back to "stale index, reconcile skipped" mode. Operator restarts the daemon when the mount is back.

**The `mtime + size` check** avoids re-hashing files that haven't changed. Cost: ~one stat() per file per reconciliation pass. **Caveat for sync clients:** some sync tools preserve mtime; others stamp at receive. Worse, same mtime + same size + different content is possible with truncate-then-write. The skip is a performance optimization that assumes well-behaved local filesystems; if a sync client misbehaves, the workaround today is to stop the daemon, delete `index.db`, and restart (auto-rebuild reads bodies from disk and recomputes every hash). A `--force-rehash` flag could short-circuit this — not implemented yet because nobody's hit the case.

If file mtime/size differ from stored values → re-hash. If the new hash matches the stored hash (mtime changed but content didn't), update mtime/size only; skip re-embed.

### 5.3 Reconciliation cost

For a 10K-doc memex with no changes: ~10K stat() calls. Order-of-magnitude estimate; measure on actual filesystems before final tuning.

For 10K docs with 100 changed files: stats + 100 hash computations + 100 re-index passes (embedding only if hash changed).

For an empty DB (cold start, full re-index): walk + hash + embed everything. Embedding dominates.

### 5.4 Recovery from missing or corrupt `index.db`

If `index.db` is missing, empty, or fails to open with the current schema, the daemon creates a fresh schema and runs a full reconciliation pass from scratch. The filesystem (`<wiki_dir>` + `<raw_dir>`) is canonical; everything in `index.db` derives from it (including raw provenance, which is parsed from each raw file's frontmatter).

To force a rebuild manually: stop the daemon, `rm <memex_root>/index.db`, restart the daemon. No special command needed.

There is no `memex rebuild` command. The daemon's startup path is the rebuild path.

---

## Section 6 — Query path

### 6.1 Vector search

```
1. Embed query: q_emb = embed(question)
2. SELECT hash_seq FROM chunks_vec WHERE embedding MATCH q_emb LIMIT k
3. Parse hash_seq → (hash, seq)
4. SELECT pos, len FROM chunks WHERE hash = ? AND seq = ?
5. Resolve hash → (path, mtime, size) from documents
6. Stat the file. If file mtime/size != documents.mtime/size, the file changed since we indexed it; chunks.pos/len may slice into stale coordinates. Two options:
     a) Skip the result silently (the next reconciliation will refresh it).
     b) Inline re-index: re-read body, recompute hash/chunks/embeddings, re-run search.
   Default: (a). Log at debug level. (b) is overkill for a single query; reconciliation catches up.
7. Read file bytes from disk
8. Strip frontmatter to get body bytes
9. extract_focused_snippet(body, pos, len, primary_query, intent, SNIPPET_MAX_LEN)
10. Build SearchResult { docid: hash[..7], path, title, score, snippet }
```

### 6.2 BM25 search

Identical until step 4 — BM25 returns documents (not chunks), so:

```
4. For each top doc: select all chunks for hash, score each chunk via score_chunk(chunk_body, query_terms, intent_terms), pick best
5. Use the best chunk's pos + len + body for snippet extraction (Section 6.1, step 7+)
```

### 6.3 Body fetch is a file read

The big change vs today: bodies come from disk, not from `content.doc`. Implication:

```rust
fn fetch_body(memex_root: &Path, path: &str) -> Result<String> {
    let abs_path = memex_root.join(path);
    let file = std::fs::read_to_string(&abs_path)?;
    // Both wiki and raw carry YAML frontmatter; the LLM only sees the body.
    Ok(strip_frontmatter(&file))
}
```

Cost: ~50-200 µs per body on local SSD (warm), ~1-2 ms cold. For top-k=10 with 5 unique docs: ~250 µs warm, ~5-10 ms cold. Sub-millisecond in steady state; negligible against LLM call dominator (100s of ms).

---

## Section 7 — Focused snippet + intent

### 7.1 New module: `core/src/snippet.rs`

```rust
pub const INTENT_WEIGHT_SNIPPET: f64 = 0.3;
pub const INTENT_WEIGHT_CHUNK: f64 = 0.5;
pub const SNIPPET_MAX_LEN: usize = 300;

pub fn extract_focused_snippet(
    body: &str,
    chunk_pos: usize,
    chunk_len: usize,
    primary_query: &str,
    intent: Option<&str>,
    max_len: usize,
) -> String { /* ... */ }

pub fn score_chunk(
    chunk_text: &str,
    query_terms: &[String],
    intent_terms: &[String],
) -> f64 { /* ... */ }

pub fn extract_intent_terms(intent: &str) -> Vec<String> { /* ... */ }
```

`extract_focused_snippet` returns the rendered snippet string directly. The diff-header at the front (`@@ -49,4 @@ (48 before, 48 after)`) encodes focus line, snippet line count, and document context — agents and the CLI parse those values straight from the header. Earlier drafts of this spec defined a structured `SnippetResult` with `focus_line` / `lines_before` / `lines_after` / `snippet_lines` fields; QMD's `extractSnippet` returns the same fields but no caller in QMD consumes them, and memex follows suit. If a future caller needs programmatic access without parsing the header, restoring a typed return is a one-line change — the values are already computed inside the function.

Window/context constants (`SNIPPET_CONTEXT_PAD = 100`, `SNIPPET_WINDOW_BEFORE = 1`, `SNIPPET_WINDOW_AFTER = 2`) are private to the snippet module — they're internal tuning, not part of the public API.

### 7.2 `extract_focused_snippet` algorithm

Mirrors QMD's `extractSnippet` (CLI mode, `cli/qmd.ts:1966`) which passes the full body + chunkPos + chunkLen and emits document-relative line numbers. **No chunker call at query time** — the function operates directly on body coordinates.

```
1. context_start = chunk_pos.saturating_sub(SNIPPET_CONTEXT_PAD)
2. context_end   = (chunk_pos + chunk_len + SNIPPET_CONTEXT_PAD).min(body.len())
3. search_body   = &body[context_start..context_end]
4. line_offset   = body[..context_start].matches('\n').count() as u32
5. Split search_body on '\n' into `lines`. Let search_total = lines.len().
6. Tokenize: query_terms = lowercase whitespace-split of primary_query.
              intent_terms = extract_intent_terms(intent).
7. For each i ∈ [0, search_total):
     score = (count of query_terms in lines[i] × 1.0) + (count of intent_terms in lines[i] × INTENT_WEIGHT_SNIPPET)
8. Pick best = i with highest score (first wins on ties).
9. window = [best - SNIPPET_WINDOW_BEFORE, best + SNIPPET_WINDOW_AFTER + 1) clamped to [0, search_total).
10. Join window lines with '\n'. If joined > max_len: truncate to max_len - 3 + "...".
11. Compute doc-relative coords:
     doc_total_lines = body.matches('\n').count() as u32 + 1
     doc_start = line_offset + window.start as u32 + 1
     count = window.len() as u32
     lines_before = doc_start - 1
     lines_after = doc_total_lines - (doc_start + count - 1)
12. Header: format!("@@ -{},{} @@ ({} before, {} after)", doc_start, count, lines_before, lines_after)
13. Numbered body: add_line_numbers(snippet_text, doc_start)
14. Return SnippetResult { focus_line: line_offset + best as u32 + 1, snippet: header + "\n" + numbered_body, ... }
```

Invariant: `lines_before + count + lines_after == doc_total_lines`.

Window default: 4 lines (1 before + best + 2 after). Inherited from QMD; the asymmetry presumes the highest-scoring line is often a heading or topic sentence with substantive content following.

Worked example: 100-line document; chunk at byte position 1234, length 3600. Best match at line index 3 of search_body (i.e., document line 50):

```
@@ -49,4 @@ (48 before, 48 after)
49: This document describes REST API design.
50: # REST Patterns
51: REST APIs map verbs to CRUD operations.
52: GET reads, POST creates, PUT replaces.
```

Agent reads `@@ -49,4 @@`, knows the snippet is at doc line 49, can run `memex read <docid> --from-line 49 --max-lines 30` to widen the context.

### 7.3 `extract_intent_terms`

Mirrors QMD's `extractIntentTerms` (`src/store.ts:3842`):

1. Lowercase the intent string.
2. Whitespace-tokenize.
3. Strip leading/trailing non-alphanumeric punctuation per token; preserve internal punctuation (so `node.js` survives, `C++` becomes `c` and is filtered).
4. Filter out tokens of length ≤ 1 OR present in `INTENT_STOP_WORDS` (75-word list per QMD `src/store.ts:3820-3835`).

### 7.4 Intent through the retrieval pipeline

Mirroring QMD, intent is wired into five sites:

**1. Worker prompt prefix.** `worker/mod.rs::run_one_attempt` prepends `Intent: <text>\n\n` to Expand and Synth user prompts when `intent` is `Some`. Single code path; uniform across claude-code, codex, gemini-cli, openai-api backends.

**2. Strong-signal disable.** `Bm25Search::is_strong_signal` short-circuits to `false` when `intent.is_some()`. Forces full expansion + rerank pipeline. Mirrors QMD `src/store.ts:4032`.

**3. Best-chunk selection.** `score_chunk(chunk_text, query_terms, intent_terms)` is **binary per term** (1.0 if the term appears anywhere in the chunk, 0.0 otherwise; intent terms scaled by `INTENT_WEIGHT_CHUNK = 0.5`). Mirrors QMD `src/store.ts:4141-4151` exactly — not a term-frequency variant. A chunk that simply *covers* both query terms beats one that repeats a single term many times.

In the BM25 snippet-backfill path, the candidate chunks are fetched from the `chunks` table by content hash (per §7.2's "no chunker call at query time" rule — coordinates only, no re-chunking). When the doc has zero chunks (indexed without an embedding model loaded), the path falls back to scoring the whole body as a single chunk; this is the only safe behavior since there is no chunk index to consult. Vector hits skip this step entirely — the matching chunk's `(pos, len)` is already selected by the vector search.

**4. Snippet line scoring.** `extract_focused_snippet` weights intent terms at `INTENT_WEIGHT_SNIPPET = 0.3` per matched line. Mirrors QMD `src/store.ts:3877`.

**5. Skill template.** `plugin/skills/memex-query/SKILL.md` carries the actual prompt text. It mirrors QMD's MCP guidance verbatim — agents are instructed to **always provide intent on every query**, to disambiguate the question and improve snippet selection. Intent is short context (a few words to a sentence) describing what the user actually means; it is *not* an additional search term.

The skill template (used uniformly across claude-code, codex, gemini-cli, openai-api) reads:

```
Always provide `--intent` on every `memex query` call to disambiguate
the question and improve snippet selection. Intent is short
disambiguating context — what the user actually means — not a second
search term.

Correct:
  memex query --intent "web page load times" "performance optimizations"
The user says "performance" but means front-end. Intent narrows the sense.

Incorrect:
  memex query --intent "performance optimizations to consider" "performance"
That just restates the query as intent. The system treats intent as a
weighted re-ranking signal, not as another search term — restating
washes out the boost.
```

Sourced from QMD's `src/mcp/server.ts:147` (`"Always provide \`intent\` on every search call to disambiguate and improve snippets."`).

### 7.5 CLI surface

```
memex query "<question>" [--raw] [--intent CTX] [--top-k N] [--collection NAME]...
```

`--top-k` defaults to **10** (matches QMD MCP default `src/mcp/server.ts:307`). Edit `cli/src/main.rs:89` (`default_value = "5"` → `"10"`).

The `memex search <title>` command (title-only) is unchanged — intent is meaningless for it.

Daemon protocol `Request::Query` gains `intent: Option<String>`. Default `None`.

### 7.6 Output

`memex query --raw` emits JSON via `cli/src/daemon/mod.rs::query_raw`. Per-result object:

```json
{
  "docid": "a3f2b1c",
  "doc_type": "wiki",
  "score": 1.0,
  "path": "wiki/rest-patterns.md",
  "title": "REST Patterns",
  "hash": "...",
  "snippet": "@@ -49,4 @@ (48 before, 48 after)\n49: This document describes REST API design.\n50: # REST Patterns\n..."
}
```

`Entry.body` (the field consumed by the synth pipeline and surfaced in `--raw`) carries the focused snippet, not the full body. Reduces synth context by ~12× vs today's "snippet = full chunk text" payload.

---

## Section 8 — Ranged read on `memex read`

`memex read` already accepts multiple refs (`refs: Vec<String>`) and reads bodies directly from files. Add two flags:

```
memex read <ref...> [--from-line N] [--max-lines M]
```

- `--from-line N`: 1-indexed start line. Default: 1.
- `--max-lines M`: maximum lines returned. Default: unbounded.

**Why `--from-line N --max-lines M` and not `--offset/--limit` or `--start-line/--end-line`?** The diff-header in search snippets has the form `@@ -47,4 @@` where the second number is *count*, not end-line. The agent reads `47,4` and types `--from-line 47 --max-lines 4` — direct paste, no arithmetic. `--start-line/--end-line` would force the agent to compute `end = 47+4-1 = 50` on every call. `--offset/--limit` borrows pagination semantics that don't match line-numbered text. Sticking with the count form preserves the diff-header → CLI symmetry.

**Multi-ref + slicing is an error.** When multiple refs are present AND either `--from-line` or `--max-lines` is set, fail with:

```
error: --from-line and --max-lines require a single ref; got N refs
```

Multi-ref without flags works as today: emit each doc's full body.

Output header gains a slice suffix when ranged:

```
=== a3f2b1 wiki rest-patterns [lines 47..96 of 312] ===
```

Doc-relative line numbers from focused snippets feed directly into `--from-line`: agent sees `@@ -47,4 @@` from search, runs `memex read <docid> --from-line 47 --max-lines 30` to widen.

---

## Section 9 — Chunking-quality refactor

Replace per-byte break-scoring walk with QMD's pre-scanned break points + backward-only search window + squared-distance decay. Three pieces:

**9.1 Pre-scan break points.** Walk the body once at chunk time, build a sorted `Vec<BreakPoint>` of all heading/code-fence/paragraph/list/newline positions with their scores. Replace per-byte `score_break` calls with iteration over this small list.

**9.2 Backward-only window.** Window changes from `[target − 400, target + 400]` (centered, today) to `[target − 800, target]` (backward only). Chunks no longer overshoot `target`; `chunk.len <= max_chars` becomes a hard upper bound.

**9.3 Squared-distance decay.** Find best cutoff: `final_score = bp.score × (1 - (distance/window)² × decay_factor)`. At distance 0: multiplier = 1.0. At window edge: 0.3. A high-quality break far back can win over a mediocre break near target — by computed margin, not iteration order.

Bumps `CHUNK_ALGO_VERSION = 2`. `chunks` table gains `algo_version` column. `lint --fix` extends to detect `algo_version != CURRENT_ALGO_VERSION` and re-embed.

Detailed algorithm and tests in the previous spec iteration; preserved verbatim.

---

## Section 10 — LLM result cache

Adopt QMD's `llm_cache(hash, result, created_at)` table. Cache key = `sha256(model + task + system_prompt + user_prompt)`. Wraps `worker/mod.rs::run_one_attempt`:

```rust
let key = cache_key(&cfg.model, system_prompt, &user_prompt, kind);
if let Some(cached) = lookup_cache(&db, &key)? {
    return parse_cached_result(&cached, kind);
}
let result = backend.complete(&system_prompt, &user_prompt).await?;
insert_cache(&db, &key, &result)?;
return parse_result(&result, kind);
```

Daemon-startup housekeeping prunes entries older than 90 days (longer than `ingest_jobs`'s 30 days because each entry costs an LLM call to rebuild). Since `cache_key` includes the model name, model upgrades produce new keys; old-model entries age out naturally without serving stale results. No manual `memex cache clear` verb — periodic prune is sufficient. For sensitive-data wipes, drop the whole table directly: `sqlite3 index.db 'DELETE FROM llm_cache'`.

Sensitive content note: cache stores LLM input + output verbatim; treat as sensitive as the source bodies. The existing redaction pass still applies before LLM submission.

---

## Section 11 — Testing

### 11.1 File layer + reconciliation

- **Wiki write round-trip:** `memex write` produces a file at `$MEMEX_ROOT/wiki/<slug>.md`; daemon reconciliation indexes it; `memex query` returns the page.
- **Raw source add:** `markitdown $url | memex source add $url` produces a file at `$MEMEX_ROOT/raw/<body_hash[..2]>/<body_hash[2..]>` whose first lines are YAML frontmatter (`source`, `source_kind`, `ingested_at`, `converter`, `title`) followed by the converted body. `sha256(body)` matches the filename hash; `sha256(file)` does not. Daemon indexes; `memex source show <docid>` returns the body.
- **Idempotent re-ingest:** running `memex source add $url` twice on the same source produces the same docid; the second run is a strict no-op (file is left untouched on disk; `ingested_at` is unchanged because frontmatter is write-once).
- **Frontmatter-only edit on raw:** modify a raw file's frontmatter `source:` field by hand, leaving the body unchanged. Reconciliation re-reads frontmatter, updates `documents.source`, does NOT re-embed (body hash unchanged).
- **External edit (wiki):** modify `wiki/auth-tokens.md` outside of memex; trigger reconciliation; assert `documents.hash` and `documents.mtime` change; assert chunks/chunks_vec are regenerated.
- **External body edit (raw):** modify the body of a `raw/<hash>` file outside of memex (so `sha256(body)` no longer matches the filename); reconciliation flags the file as a hash-vs-body integrity violation (Section 3.5), skips re-indexing, surfaces it via `memex lint`.
- **External delete:** delete `wiki/auth-tokens.md` from disk; reconciliation; assert documents row + chunks + chunks_vec are removed.
- **Reconciliation skip on unchanged file:** stat-then-skip path verified by mtime + size match.
- **DB-wipe safety threshold:** start with 100 indexed docs; remove `wiki/`; reconcile; assert it refuses (because `seen_paths < 0.5 × existing`) and the DB is unchanged. The bypass is `ReconcileOptions { force: true }` at the API level (no CLI surface today; covered by `reconcile_force_bypasses_threshold` unit test).
- **Auto-rebuild on missing index.db:** delete `index.db`; restart daemon; assert it auto-creates `index.db` and re-indexes everything from the filesystem.
- **Docid prefix resolution:** insert two docs whose hashes share a 4-char prefix and diverge at char 5. `memex source show <4-char-prefix>` errors with "ambiguous, matches N docs"; `memex source show <5-char-prefix>` resolves to one. Display side: `memex source list` prints `hash[..7]` for both.
- **Symlink policy:** symlinked `wiki_dir` resolves and indexes contents; symlinked file inside `wiki/` is skipped; relative symlink with `../../` is skipped.
- **Wiki delete with backlinks:** create `wiki/foo.md` and `wiki/bar.md` whose body contains `[[foo]]`; `memex delete foo` errors with the backlink list; `memex delete --force foo` proceeds.

### 11.2 Focused snippet + intent

Unit tests in `core/src/snippet.rs::tests`:
- `extract_focused_snippet` chooses the line with most query-term overlap.
- Intent terms boost score at 0.3× — line with one intent term ranks below a line with one query term but above a line with no terms.
- Window expansion: `[best - 1, best + 3]`, clamped at edges.
- Truncation appends `...` exactly when joined text > `max_len`.
- Doc-relative line numbers via `chunk_pos` + body newline counting.
- Empty inputs (empty body, empty query, no term matches) return deterministic outputs.
- **Non-ASCII regression:** Chinese characters, emoji, accented Latin in body — pos/len byte offsets slice cleanly without panics or garbled snippets.

`extract_intent_terms`:
- Stopwords filtered; single-character tokens filtered; punctuation stripped from edges only.
- `API` → `api`; `Node.js` → `node.js`; `C++` → strips `++` → `c` → filtered out by length.

Integration tests:
- Vector path returns focused snippet shape; BM25 path returns the same shape.
- Intent narrows results: `query="performance"` without intent vs. with `intent="web page load times"` produces different best-chunk selections on a doc that mentions both senses.
- `Entry.body` is the focused snippet, not the full body.
- **Stat-check skips stale results:** edit a wiki file's mtime/size after vector search starts; assert the stale result is dropped, not returned with mis-sliced coordinates.

### 11.3 Ranged read

- Line-slice boundaries: line 1, last line, beyond EOF (returns empty body).
- UTF-8 safety: lines containing multibyte codepoints split correctly on `\n`.
- Header includes line-range suffix when ranged; original header otherwise.
- Multi-ref + `--from-line` exits with non-zero status and the documented error.
- Multi-ref without flags emits all bodies with standard headers.

### 11.4 Chunking quality

- `scan_break_points` returns correct scores for each pattern; output sorted by position; no duplicates.
- `find_best_cutoff` decay math: an h2 at distance 600 of an 800-char window beats a paragraph break at distance 10.
- Upper-bound: `chunk.len <= max_chars` for every chunk produced.
- `lint --fix` algo_version mismatch path: insert chunks with `algo_version = 1`; run `lint --fix`; verify chunks deleted + re-embedded with `algo_version = 2`.

### 11.5 LLM cache

- Cache hit: insert row with known key; submit job whose prompt produces that key; assert backend.complete is NOT called and cached result returned.
- Cache miss: submit fresh job; assert backend.complete IS called once and result inserted.
- Different model → different key; identical text + different task → different key.
- Concurrent inserts: two simultaneous jobs with identical prompts; INSERT OR IGNORE handles the race.

### 11.6 Reconciliation perf

- 10K docs, no changes: full reconciliation pass < 200 ms on local SSD; < 5 sec on network drive.
- 10K docs, 100 modifications: detection + re-index < 2 sec on local SSD.
- Cold rebuild from filesystem (10K docs): < 2 minutes (dominated by embedding).

---

## References

### QMD source at HEAD `e8de7ca` (mirrored at `/data/MemVerge/qmd`)

- `src/store.ts:757-810` — content + content_vectors schema (memex consolidates these)
- `src/store.ts:3848` — `extractSnippet` (focused excerpt + diff header)
- `src/store.ts:3924` — `addLineNumbers`
- `src/store.ts:3842` — `extractIntentTerms`
- `src/store.ts:3820` — `INTENT_STOP_WORDS`
- `src/store.ts:3811` / `:3814` — `INTENT_WEIGHT_SNIPPET = 0.3` / `INTENT_WEIGHT_CHUNK = 0.5`
- `src/store.ts:4003` — `hybridQuery` (intent-aware pipeline)
- `src/store.ts:3297` / `:3299` — `rerank` with intent-prepended query
- `src/llm.ts:1131` — `expandQuery` LLM prompt with intent
- `src/store.ts:1702` — `getDocid(hash) = hash.slice(0, 6)` (QMD's docid is purely a hash function; memex collision-extends)
- `src/mcp/server.ts:307` — `limit: z.number().optional().default(10)` — QMD's MCP top-k default
- `src/mcp/server.ts:341-353` — primary-query selection + extractSnippet call
- `src/store.ts:787-792` — `llm_cache` schema (memex adopts identically)

### Memex code (current state)

- `core/src/search.rs:41` — `SearchResult`
- `core/src/retrieval.rs:43` — `hybrid_retrieve_expanded`
- `core/src/retrieval.rs:213` — `embed_document`
- `core/src/retrieval.rs:297` — `vector_search_as_results`
- `core/src/embed.rs:259` — `chunk_text`
- `core/src/docid.rs` — `resolve_prefix(conn, prefix)` and `short(hash) -> &str` (= `hash[..7]`); the prior `allocate_docid` collision-extension function is gone with the docid column
- `core/src/lint.rs` — `lint --fix` (model-name mismatch detection; extended with algo_version)
- `cli/src/main.rs:36` — `Search { title }` CLI surface
- `cli/src/main.rs:82` — `Query { question, raw, top_k, collections }` CLI surface (gains `intent`)
- `cli/src/main.rs:198` — `run_read` (gains `--from-line` / `--max-lines`)
- `cli/src/daemon/protocol.rs:41` — `Request::Query` (gains `intent` field)

### Other

- LoCoMo harness: `eval/locomo/run.py`
