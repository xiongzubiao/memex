# Memex — Design Specification

**Status:** DRAFT
**Date:** 2026-04-07 (revised 2026-04-15)

> **Partial supersession notice (2026-04-19):**
> - **Section 7 (Session Ingestion & Distillation)** is superseded by `2026-04-19-daemon-ingestion-design.md`. The hook-based distillation model, `/memex-distill` skill, `distilled_at` tracking, `memex source list --undistilled`, and `memex source mark-distilled` are replaced by daemon-based ingestion with an Extract + Merge LLM pipeline (1-2 calls per session).
> - **Architecture overview** references to `source mark-distilled`, `/memex-distill`, and the hook-spawned background agent are superseded.
> - **CLI commands** `memex source list --undistilled` and `memex source mark-distilled` are removed. `memex hook session-end` is replaced by `memex ingest`.
> - **SQLite schema** `distilled_at` column is removed.
> - **Daemon scope** is expanded from read-only (per `2026-04-17`) to read-write (per `2026-04-19`). All mutations now route through the daemon by default.
> - Sections 1-6, 8-9 of this document remain current.

---

## Table of Contents

- [Overview](#overview)
1. [Wiki Page Format](#1-wiki-page-format)
2. [CLI Commands](#2-cli-commands)
3. [Skills](#3-skills)
4. [Proactive Behavior](#4-proactive-behavior)
5. [SQLite Schema](#5-sqlite-schema)
6. [Search & Ranking](#6-search--ranking)
7. [Session Ingestion & Distillation](#7-session-ingestion--distillation)
8. [Plugin Distribution](#8-plugin-distribution)
9. [Deferred](#9-deferred)
- [References](#references)

---

## Overview

Memex is a personal knowledge system built on Karpathy's LLM-wiki pattern, named after Vannevar Bush's 1945 concept of a personal memory system with valued connections. It captures knowledge into a compounding wiki of interconnected pages, answers questions with full citations, and generates new knowledge through multi-LLM brainstorming.

The core insight: "the wiki is a persistent, compounding artifact." Instead of rediscovering relevant information from raw documents on every query (RAG), knowledge is extracted once, integrated into an evolving structure, and compounds over time. Cross-references are maintained automatically, contradictions are detectable via timestamps, and synthesis improves with each addition.

Memex is an independent experiment exploring the wiki synthesis pattern, separate from MemVerge's MemBox product.

### Architecture

The `memex` binary is an LLM-free wiki storage and search engine. All intelligence lives in agent skills that run inside AI agents — coding agents (Claude Code, Codex, Gemini CLI, Cursor), autonomous agents (OpenClaw, Hermes), or any agent that can invoke shell commands. Eight commands: `write`, `read`, `search`, `delete`, `lint`, `import` (session preprocessing), `source list` (query source documents), and `source mark-distilled` (distillation tracking). Skills orchestrate these commands to implement retrieval pipelines, interactive ingestion, multi-LLM brainstorming, and session distillation.

```mermaid
graph TD
    Agents["AI Agents<br/>Claude Code · Codex · Gemini CLI · Cursor · OpenClaw · Hermes · ..."]
    Agents --> Skills["memex plugin skills<br/>query · ingest · brainstorm · distill · backfill"]
    Skills -->|"shell calls"| CLI["memex CLI (LLM-free)<br/>write · read · search · delete · lint<br/>import · source list · source mark-distilled"]
    CLI --> SQLite["SQLite<br/>(FTS5 + sqlite-vec)"]
    CLI --> Wiki["wiki/*.md<br/>(on disk)"]
    CLI --> Model["ONNX model<br/>(computes embeddings)"]
    Hooks["Lifecycle Hooks"] -->|"SessionEnd: run"| CLI
    Hooks -->|"SessionEnd: spawn"| BG["Background Agent → /memex-distill"]
    Hooks -->|"SessionStart: inject context"| Agents
```

| Concept | Implementation |
|---|---|
| Wiki | `~/.memex/wiki/*.md` with YAML frontmatter (Section 1) |
| Source | Original content in `source` doc_type, SQLite only (Section 1) |
| Skills | query, ingest, brainstorm, distill, backfill — cross-platform SKILL.md (Section 3) |
| Storage | Content-addressable SQLite following QMD (Section 5) |
| Search | BM25 + vector, typed queries (lex/vec/hyde), RRF fusion (Section 6) |
| Session ingestion | Lifecycle hooks + CLI preprocessing + distillation (Section 7) |
| Plugin | npm package with bundled binary + model (Section 8) |

---

## 1. Wiki Page Format

Wiki pages and original sources serve different purposes:

| Layer | Purpose | Collection |
|---|---|---|
| Wiki page | Structured knowledge, cross-links, synthesis | `wiki` doc_type (also on disk as `~/.memex/wiki/*.md`) |
| Original source | Full detail, exact terms, verification | `source` doc_type (SQLite only) |

Wiki pages are markdown files with YAML frontmatter in `~/.memex/wiki/`.

```markdown
---
title: REST Patterns
tags:
  - api
  - design
created_at: 2026-04-06T00:00:00Z
updated_at: 2026-04-06T00:00:00Z
summary: Resource-oriented design with proper HTTP verbs and versioning.
sources: []
---

REST API patterns for resource-oriented design.

## URL Structure

Use nouns, not verbs: `/users/123` not `/getUser?id=123`.
Supports [[api-versioning]] via URL prefix.

## HTTP Methods

- GET: read
- POST: create
- PUT: replace
- PATCH: partial update
- DELETE: remove

Related: [[error-handling]], [[authentication]]
```

### Frontmatter fields

| Field | Required | Description |
|-------|----------|-------------|
| `title` | yes | Page title |
| `tags` | yes | Free-form tags (list) |
| `created_at` | yes | ISO 8601 timestamp |
| `updated_at` | yes | ISO 8601 timestamp |
| `summary` | no | One-line summary (stored in `documents.summary` for agent use) |
| `sources` | no | List of source file paths (for human readers; populated by the agent when drafting the page content, not by the binary. Not used by search or schema — purely informational.) |

### Cross-references

Wiki links use `[[page-stem]]` syntax where `page-stem` matches the filename without `.md`. Example: `[[rest-patterns]]` links to `wiki/rest-patterns.md`.

---

## 2. CLI Commands

The `memex` binary is LLM-free. No `init` (lazy init on first `write`), no `auth` (no LLM provider). Eight commands: `write`, `read`, `search`, `delete`, `lint`, `import`, `source list`, and `source mark-distilled`.

### `memex write <filename|title> [--source <path> ...] [--force] [--quiet]`

Writes a wiki page. Content from piped stdin (auto-detected) or interactive `$EDITOR` (when terminal). Like `git commit` — no flag needed. Optionally attaches original source content via `--source` (repeatable).

**Filename normalization**: if the argument contains spaces or uppercase, it is normalized to a kebab-case filename stem (e.g., `"REST API Design"` → `rest-api-design.md`). If the argument already looks like a filename stem (lowercase, hyphens, no spaces), it is used as-is.

```mermaid
graph TD
    Stdin["Piped stdin or $EDITOR"] --> Validate["Validate frontmatter"]
    Source["--source files"] --> ReadSrc["Read source files"]
    Validate --> AutoLink["Auto-link + backlink"]
    AutoLink --> Hash["SHA-256 hash"]
    ReadSrc --> Hash
    Hash --> Mut["Mutation ordering invariant:<br/>content → embed → documents → orphan cleanup"]
```

Flags:

- `--force` — overwrite if page already exists (default: fail on conflict)
- `--quiet` — reduced output (suppress linked, backlinked, suggest-create lines)

On create, checks for exact filename conflict → hard block if exists. Use `--force` to overwrite.

If no conflict (or `--force`):

1. **Lazy init** — if `~/.memex/` doesn't exist, create directory + SQLite DB
2. **Validate** — parse YAML frontmatter, reject if invalid
3. **Auto-link forward** — scan body for mentions of *eligible* existing page titles/stems (case-insensitive), replace first un-linked occurrence of each target with `[[page-stem]]`. Eligibility: stem must be multi-token (`stem.contains('-')`). See "Auto cross-linking" below.
4. **Atomic write + index** — write the linked body verbatim under `<wiki_dir>/<slug>.md` (atomic temp+rename) and index it (`documents` row, FTS5, chunk/embed).
5. **Auto-link backward** — if the new slug is itself eligible (multi-token), sweep other wiki bodies for mentions of the new title/stem, rewrite the first un-linked occurrence with `[[new-stem]]`. Body-only rewrite: frontmatter (including `updated_at`) preserved verbatim. Each rewritten page is reindexed *and re-embedded* under the same writer pass — `commit_doc` drops chunks on hash change, so embedding must follow the rewrite or vector search loses the page.
6. **Store sources** — if `--source` provided, the source's docid is recorded in the new page's frontmatter `sources:` list as `"#<docid>"`. The source itself is added separately via `memex source add`.

Output (success):
```
written: api-design (a3f2b1)
linked: rest-patterns, distributed-systems
backlinked: caching-strategies
suggest-create: error-handling
```

Output (success, `--quiet` flag — for bulk operations):
```
written: api-design (a3f2b1)
```

Output (conflict — emitted via stderr with non-zero exit):
```
write error (bad_request): wiki/api-design.md already exists; use --force
```

- `written` — slug + short docid of the page. Always shown.
- `linked` — existing pages this body now references (forward-linked). Only printed when non-empty. Suppressed by `--quiet`.
- `backlinked` — existing pages that now link to this page (backward-linked). Only printed when non-empty. Suppressed by `--quiet`.
- `suggest-create` — `[[stem]]` references in the body whose target doesn't exist. Only printed when non-empty. Suppressed by `--quiet`.

#### Auto cross-linking

`memex write` does deterministic cross-link maintenance for **multi-token titles only**. No LLM needed for this layer; the LLM (running through `/memex-ingest`) handles the judgment cases the daemon can't.

**Eligibility filter (`auto_link_eligible`)**: a stem qualifies iff it contains `-`. Single-token stems — short ones (`api`, `auth`, `bob`, `alice`) and longer ones (`caching`, `kubernetes`, `performance`, `database`, `testing`) — never auto-link. Reason: case-insensitive title matching can't tell a navigation cue from generic English prose for single-token names. Matching `caching` would bracket every body mention of the common noun "caching"; matching `kubernetes` would bracket every casual reference. Multi-token names (`oauth-migration`, `auth-tokens`, `kubernetes-deployment`) are distinctive enough that a body mention is almost always a deliberate reference.

**Forward link**: on write, scan the body for mentions of any *eligible* existing title or stem-as-words (case-insensitive). Replace the first un-linked occurrence with `[[stem]]`.

- Body: *"Migrating to OAuth 2.1 affects auth-tokens validation."*
- `oauth-migration.md` (multi-token, eligible) and `auth-tokens.md` (multi-token, eligible) exist
- After write: *"Migrating to [[oauth-migration]] affects [[auth-tokens]] validation."*

**Backward link**: after writing a new page, if its slug is multi-token, sweep all existing wiki bodies for mentions of the new title or stem-as-words. Rewrite the first un-linked occurrence in each match with `[[new-stem]]`.

- Pre-existing `auth-overview.md` body: *"OAuth Migration was completed in Q1."*
- New page written: `oauth-migration.md` titled "OAuth Migration"
- After write, `auth-overview.md` body becomes: *"[[oauth-migration]] was completed in Q1."* — frontmatter (incl. `updated_at`) untouched; chunks re-embedded for the new body hash.

**Suggest-create**: extract `[[stem]]` references from the body, filter against existing slugs and the new slug. Anything left is a typo or a pending-creation gap. Surfaced in `suggest-create:` output; not auto-fixed.

**LLM responsibilities** (the 20% the daemon can't do):

- Single-token references — type `[[bob]]`, `[[alice]]`, `[[caching]]`, `[[kubernetes]]` explicitly when meaning a navigation cue. Auto won't fill these in.
- Reacting to `suggest-create:` — typo, pending creation, or intentional placeholder.
- Reacting to lint findings (`missing-link:`, `dangling:`) — short-title links the daemon can't bracket, deletions/renames the LLM may want to fix.
- Semantic linking the daemon's keyword match doesn't cover (synonyms, pronoun coreferences).

**Rules** (apply when matching):

- Match longer titles before shorter ones (prevents "Rust" from consuming "Rust Borrow Checker")
- Only replace the first un-linked occurrence of each target
- Skip text already inside `[[...]]`
- Case-insensitive matching against title and stem-as-words (Unicode-safe via regex)
- Don't link a page to itself

### `memex read <identifier> ...`

Reads one or more documents (wiki or source). Accepts docid, filename stem, path, or title (see identifier resolution). Includes `updated_at` timestamp for contradiction resolution.

```
=== a3f2b1c9 wiki api-design-notes ===
---
title: API Design Notes
tags:
  - api
  - rest
created_at: 2026-04-06T00:00:00Z
updated_at: 2026-04-10T14:30:00Z
---

Summary of API design notes covering [[rest-patterns]] and error-handling.
```

Header line: `=== docid doc_type stem ===` (space-separated). Followed by the full .md content (frontmatter + body). The `updated_at` timestamp is in the frontmatter, not duplicated in the header.

Source documents are also readable by docid. The agent can read any document (wiki or source) returned by search.

**Identifier resolution**:
1. **Docid**: prefix match against `documents.docid`
2. **Filename stem**: match input against `documents.path`. For wiki docs: input `rest-patterns` matches path `wiki/rest-patterns.md` (strip directory prefix and `.md` extension). For source docs: match against the basename without extension (e.g., input `article` matches path `/path/to/article.md`). If no stem match, try exact path match.
3. **Title**: case-insensitive match against `documents.title`

First match wins across tiers. If multiple documents match within a tier (e.g., duplicate titles), return all matches for `read`, error for `delete` (must resolve to exactly one wiki document). `delete` only accepts wiki doc_type documents; attempting to delete a source document is an error.

### `memex search <query> [--lex <term> ...] [--vec <term> ...] [--hyde <term> ...]`

Full-text and vector search across wiki pages and source documents. Typed expansion flags follow QMD's query types. See Search & Ranking for the math.

**Simple form** (probe — both BM25 and vector):
```bash
memex search "REST API design"
```

**With typed expansions** (agent generates these):
```bash
memex search "REST API design" \
  --lex "RESTful endpoints" --lex "HTTP methods CRUD" \
  --vec "web service interface patterns" \
  --hyde "A guide to designing REST APIs with proper resource naming"
```

**Query types**:

| Flag | Routed to | Purpose |
|---|---|---|
| positional `<query>` | BM25 + vector | Primary query — probes all backends |
| `--lex <term>` | BM25 only | Keyword variants (repeatable) |
| `--vec <term>` | Vector only | Semantic reformulations (repeatable) |
| `--hyde <term>` | Vector only | Hypothetical document embedding (repeatable) |

Each flag value becomes a separate ranked list. All lists merged via RRF.

**Probe call** (`memex search "query"` — no typed flags): runs full pipeline (see Search & Ranking diagram). Returns signal + results.

**Expansion call** (`memex search "query" --lex ... --vec ... --hyde ...`): same pipeline but with additional BM25 lists (from `--lex`) and vector lists (from `--vec`/`--hyde`). Signal detection is **skipped** — no `signal:` line in output.

**Query sanitization**: quoted phrases preserved (`"exact match"` → FTS5 phrase query), negation supported (`-term` → NOT clause), hyphens converted to phrases (`multi-agent` → `"multi agent"`), bare words prefix-matched (`term` → `"term"*`), positives AND-joined.

**Probe output** (no typed flags):
```
signal: strong
a3f2b1c9  wiki  1.000  api-design-notes  REST API patterns for resource-oriented design...
7c9e4d02  wiki  0.500  rest-patterns     HTTP methods map to CRUD operations on resources...
```

**Expansion output** (typed flags present — no signal line):
```
a3f2b1c9  wiki    1.000  api-design-notes  REST API patterns for resource-oriented design...
7c9e4d02  wiki    0.500  rest-patterns     HTTP methods map to CRUD operations on resources...
e4f1a2b3  source  0.333  /path/to/article  Original REST API specification from...
```

- Probe includes `signal: strong` / `signal: weak` first line; expansion does not
- Each result line: docid + doc_type + post-fusion score + filename stem (wiki) or path (source) + best chunk snippet (tab-separated)
- Scores are `1/rank` (rank 1 = 1.0, rank 2 = 0.5, rank 3 = 0.33) — ordinal position, not calibrated similarity

**Two-call pattern**:

1. **Probe call**: `memex search "query"` — runs BM25 + vector, returns signal + results. Agent checks signal.
2. **Expansion call** (only if weak): `memex search "query" --lex ... --vec ... --hyde ...` — runs type-routed search, no signal detection (agent already decided to expand).

When `--lex`/`--vec`/`--hyde` flags are present, signal detection is skipped and no `signal:` line is emitted. This matches QMD's `structuredSearch()` path which takes user-provided typed queries as-is.

### `memex delete <identifier> [--force]`

Removes a wiki page. Identifier resolved via the shared identifier resolution rules (must resolve to exactly one wiki document; error on multiple matches or source documents). Prompts for confirmation when stdin is a TTY; `--force` skips the prompt. Agents (not a TTY) skip confirmation automatically.

**Pre-delete backlink guard**: if any other wiki page contains `[[<slug>]]` and `--force` is not set, deletion is blocked with the list of referencing pages. `--force` overrides the guard.

After a successful delete, the `documents` row is removed (chunks/FTS5 entries cascade). Now-dangling `[[<slug>]]` references in other pages are reported separately by `memex lint`, not by the delete output itself.

```
deleted: a3f2b1
```

Output (conflict — backlinks present, no `--force`):
```
delete error (conflict): 2 wiki page(s) link to [[<slug>]]: wiki/auth-overview.md, wiki/api-design-notes.md
Re-run with --force to delete anyway.
```

### `memex lint [--fix]`

Reports structural issues in the wiki. LLM-free.

`memex lint` (read-only) opens the index in-process and scans. `memex lint --fix` is **daemon-routed** as `Request::LintFix` so the daemon remains the single writer; concurrent ingests/writes serialize against the daemon's writer lock.

Both passes also prune `ingest_jobs` rows in terminal status (`completed`, `failed`) older than 30 days. Without this the table would grow unboundedly on long-running daemons.

- **Stale index** — wiki file on disk has been manually edited; content hash doesn't match SQLite
- **Dangling wiki links** — `[[page-stem]]` references to pages that don't exist
- **Missing cross-references** — page body mentions an existing page's title/stem but has no `[[...]]` link
- **Untracked file** — .md file exists in `wiki/` but has no `documents` row (crash recovery or manual file creation)
- **Missing file** — wiki `documents` row exists but .md file is missing from disk (crash recovery or manual file deletion)
- **Outdated embeddings** — `chunks.model` differs from current model name (model upgrade)

Note: lint only checks wiki documents against disk. Source documents are SQLite-only (no files on disk to go stale).

Output:
```
stale-index: rest-patterns (file modified, index outdated)
dangling: api-design-notes -> [[error-handling]]
missing-link: auth-overview -> [[oauth-migration]]
untracked: wiki/new-page.md (no DB row)
missing-file: old-page (DB row, no file)
```

With `--fix`:
- **Stale index**: reindexes from disk following the mutation ordering invariant (see SQLite Schema). Preserves frontmatter `created_at` and `updated_at` timestamps as-is.
- **Outdated embeddings**: compares `chunks.model` against current model name, re-embeds all documents (wiki and source) with outdated models.
- **Untracked files**: reported only (user decides: `memex write --force < file` to index, or delete the file).
- **Missing files**: reported only (user decides: restore the file, or `memex delete` to clean the DB row).
- **Dangling links / missing cross-references**: reported but not auto-fixed.

```
fixed: rest-patterns (reindexed from disk)
re-embedded: 12 documents (model upgrade)
dangling: api-design-notes -> [[error-handling]]
missing-link: auth-overview -> [[oauth-migration]]
untracked: wiki/new-page.md (no DB row)
missing-file: old-page (DB row, no file)
```

`memex write` is the canonical write path. Manual edits are detected by lint, not first-class. `memex write` already reports per-page lint info (linked, backlinked, suggest-create). `memex lint` checks the entire wiki. Disk-level checks (stale index, untracked, missing file) apply to wiki documents only. Embedding checks (model upgrades) apply to all documents (wiki and source).

### `memex import <agent> [--path <file>] [--quiet]`

Discovers and preprocesses agent session transcripts for one agent. LLM-free — preprocessing only. Agent spawning for distillation is handled by the SessionEnd hook (Section 7), not by this command. See Section 7 for full ingestion design.

```bash
memex import claude-code            # discover and preprocess Claude Code sessions
memex import codex                  # discover and preprocess Codex sessions
memex import gemini                 # discover and preprocess Gemini CLI sessions
memex import claude-code --path /some/file.jsonl  # single file (used by SessionEnd hook)
```

```mermaid
graph TD
    Start["memex import &lt;agent&gt;"]
    Start --> Discover["Discover session files<br/>for specified agent<br/>--path for single file"]
    Discover --> Loop

    subgraph Loop["For each transcript"]
        Check{"Already ingested?<br/>(file path dedup)"}
        Check -->|yes| Skip["Skip"]
        Check -->|no| Filter{"Distillation session?<br/>Non-substantive?"}
        Filter -->|yes| Skip
        Filter -->|no| Parse["Parse transcript format"]
        Parse --> Strip["Strip tool call outputs<br/>Keep: user messages,<br/>assistant reasoning,<br/>tool call metadata"]
        Strip --> Store["Store cleaned transcript<br/>as source document"]
    end

    Store --> Report["Report: discovered,<br/>skipped, prepared"]
```

- `<agent>` specifies which agent's session location to scan (e.g., `claude-code`, `codex`, `gemini`)
- `--path` processes a single file directly (used by the SessionEnd hook)
- `--quiet` suppresses progress messages (default: human-readable with progress). Per-file `imported: <docid>\t<path>` lines always print for hook consumption.
- **Deduplication**: skips already-ingested sessions by tracking original file path (`UNIQUE(doc_type, path)`)
- **Distillation session filtering**: skips sessions whose first user message starts with `/memex-distill`
- **Format version detection**: checks format markers (Claude Code `version` field, Codex `cli_version`), warns on unknown versions
- **Secret redaction**: scans tool input summaries for common secret patterns (API keys, tokens, passwords), replaces with `[REDACTED]`
- Skips non-substantive sessions: no assistant messages (aborted), no user messages (never interacted), empty/malformed files
- Stripping tool outputs reduces a potentially million-token transcript to 10K-50K tokens that fit in a single LLM context window
- **Error messages**: parse failures include problem + cause + fix suggestion (not just the raw error)
- **Streaming parse**: JSONL files (Claude Code, Codex) read line-by-line via BufReader (not loaded entirely into memory). Gemini JSON files loaded whole (single object).

### `memex source list --undistilled`

Lists source documents that have been ingested but not yet distilled. Used by `/memex-backfill` to get the list of sources awaiting distillation. Supports retry — if distillation failed on some sources, they remain undistilled and appear on the next run.

### `memex source mark-distilled <identifier>`

Marks a source document as distilled by setting `distilled_at` to the current timestamp. Accepts a docid or an absolute file path. Used by `/memex-distill` after completing distillation of a source.

---

## 3. Skills

Five skills. All cross-platform — same SKILL.md works in any agent that can run shell commands (Claude Code, Codex, Gemini CLI, Cursor, OpenClaw, Hermes, etc.).

### `memex-query` — Full retrieval pipeline

Follows QMD's pipeline structure. The agent LLM replaces QMD's local models (Qwen3-1.7B for expansion, Qwen3-Reranker for reranking) — same pipeline, same typed queries, agent handles the intelligence.

```mermaid
graph TD
    UQ["User question"] --> S1

    S1["1. memex search 'query'<br/>BM25 + vector probe, signal check"]
    S1 -->|"signal: strong"| S5
    S1 -->|"signal: weak"| S2

    S2["2. Agent generates typed expansions<br/>--lex 'keyword1' --lex 'keyword2'<br/>--vec 'semantic reformulation'<br/>--hyde 'hypothetical document...'"]
    S2 --> S3

    S3["3. memex search 'query' --lex ... --vec ... --hyde ...<br/>type-routed search, all lists RRF merged"]
    S3 --> S4

    S4["4. Agent reranks results using chunk snippets"]
    S4 --> S5

    S5["5. memex read &lt;top docids&gt;"]
    S5 --> S6

    S6["6. Agent synthesizes answer with citations<br/>(empty results → tell user)"]
```

**Typed query expansion**: when signal is weak, the agent generates three types of expansion:
- **`--lex`** — keyword variants targeting exact terms the wiki might contain (routed to BM25). Example: user asks "how do we handle auth?" → agent generates `--lex "authentication" --lex "OAuth token session"`
- **`--vec`** — semantic reformulations of the question (routed to vector search). Example: → `--vec "user identity verification and access control"`
- **`--hyde`** — a hypothetical document that would answer the question (routed to vector search). Example: → `--hyde "Authentication in this system uses OAuth 2.0 with JWT tokens stored in httpOnly cookies"`

This matches QMD's Qwen3-1.7B expansion output format, but generated by the agent LLM instead.

**Agent reranking**: search results include best-chunk snippets (see Search & Ranking). The agent reranks by relevance before reading full documents, replacing QMD's local Qwen3-Reranker.

### `memex-ingest` — Interactive source ingestion

The agent reads source material, generates wiki pages, and writes them interactively.

```mermaid
graph TD
    S1["1. Agent reads source file (Read tool)<br/>(local paths only; URLs deferred)"]
    S1 --> S2["2. Agent summarizes content"]
    S2 --> S3["3. Ask user: what wiki pages to create?"]
    S3 --> S4["4. Agent drafts pages, shows for approval"]
    S4 --> Loop

    subgraph Loop["For each approved page"]
        Search["5. memex search 'title' (probe)"]
        Search -->|"strong signal +<br/>wiki match"| Read["Agent reads existing page"]
        Search -->|"weak / no match"| Create

        Read --> Decide{"Overwrite or<br/>create new?"}
        Decide -->|overwrite| Force["memex write --force<br/>--quiet --source /path"]
        Decide -->|create new| Create["memex write<br/>--quiet --source /path"]
    end

    Force --> Next["Next page"]
    Create --> Next
    Next -->|"more pages"| Search
    Next -->|"all done"| Lint["6. memex lint (wiki-wide health check)"]
    Lint --> Report["7. Agent reports: pages created, lint findings"]
```

Pre-write check (step 5): agent uses signal, doc_type, and stem from search output to judge similarity. Source hits are ignored for duplicate detection. Numeric scores are ordinal (`1/rank`), not calibrated similarity — don't threshold on them.

### `memex-brainstorm` — Multi-LLM brainstorming via cross-CLI calls

Uses external agent CLIs for multi-model diversity. Discovery: `which claude codex gemini 2>/dev/null`.

```mermaid
graph TD
    Topic["Topic"] --> Propose

    subgraph Propose["1. Propose (parallel)"]
        Host["Host agent proposes"]
        Codex["codex --quiet 'Propose 3 approaches for: topic'"]
        Gemini["gemini 'Propose 3 approaches for: topic'"]
    end

    Propose --> Merge["2. Host agent merges all proposals"]

    subgraph Review["3. Review (parallel)"]
        RC["codex --quiet 'Review for weaknesses: merged'"]
        RG["gemini 'Review for weaknesses: merged'"]
    end

    Merge --> Review
    Review --> Converged{"Converged?"}
    Converged -->|no| Merge
    Converged -->|yes| Write["4. echo result | memex write 'Design Topic'"]
```

**Fallback:** if only one CLI is available, single-model brainstorm with persona/temperature variation.

### `memex-distill` — Automatic knowledge extraction

LLM-powered extraction from a single source document. Distills one source into wiki pages — works on session transcripts, articles, notes, or any content stored as a memex source. Used by the SessionEnd hook's spawned agent and by `/memex-backfill` subagents.

```mermaid
graph TD
    S1["1. Read source document"]
    S1 --> S2["2. Extract distinct topics,<br/>decisions, findings, insights"]
    S2 --> Loop

    subgraph Loop["For each topic"]
        Search["3. memex search 'topic'"]
        Search -->|"match found"| Read["Read existing page"]
        Search -->|"no match"| Create["Create new page<br/>memex write --source"]

        Read --> Merge["Merge new findings<br/>memex write --force --source"]
    end

    Merge --> Next["Next topic"]
    Create --> Next
    Next -->|"more topics"| Search
    Next -->|"all done"| Summary["4. Generate summary page<br/>(optional, for session audit trail)"]
```

**Merge strategy:** Preserve existing content structure. Add new findings in context. Update the `updated_at` timestamp. Add the new source to the `sources` list. Don't rewrite content that hasn't changed. Mark source as distilled on completion.

### `memex-backfill` — Batch ingestion and distillation

Discovers, preprocesses, and distills sources in bulk. Handles session backfill (initial onboarding with hundreds of past sessions) and batch distillation of any undistilled sources.

```mermaid
graph TD
    S1["1. memex import &lt;agent&gt;<br/>(Rust: discover sessions,<br/>filter, preprocess)"]
    S1 --> S2["2. memex source list --undistilled<br/>(all undistilled sources,<br/>sessions and non-sessions)"]
    S2 --> S3{"Sources found?"}
    S3 -->|none| Done["Nothing to distill"]
    S3 -->|yes| S4["3. Dispatch parallel subagents"]
    S4 --> Sub["/memex-distill per source"]
    Sub --> Done2["Report: N sources distilled"]
```

Agent-native — works in any agent that supports subagent dispatch.

---

## 4. Proactive Behavior

Defined in `plugin/AGENTS.md` (cross-platform behavioral instructions). Platform-specific files (`plugin/CLAUDE.md`, `plugin/GEMINI.md`) import these triggers. Not a skill — behavioral instructions that any host agent can follow.

### Trigger 1: New knowledge produced

When the conversation produces wiki-worthy knowledge (design decisions, debugging insights, discovered patterns), the agent prompts:

> "This looks like useful knowledge for your memex. Want me to add a wiki page about [topic]?"

If yes, the agent drafts content and pipes it to `memex write`.

### Trigger 2: Stale information detected

When the agent retrieves wiki pages via `memex-query` and content contradicts the current conversation, the agent compares `updated_at` timestamps to determine recency, then prompts:

> "Your wiki page [title] (last updated [date]) seems outdated — it says X but we're seeing Y. Want me to update it?"

If yes, the agent updates via `memex write --force`.

### Trigger 3: Wiki health check

When the user asks about wiki health, issues, or maintenance, the agent runs `memex lint` and reports findings.

All triggers are opt-in — the agent prompts, the user decides.

Trigger 1 is also the primary mechanism for session ingestion (Section 7). The agent captures significant findings to memex during the session via AGENTS.md instructions: search memex, create or update wiki pages. Zero additional LLM cost — the agent is already running. This also serves as compaction recovery — when context is compacted, the agent queries memex to recover earlier findings it captured. Session lifecycle hooks (Section 7) handle automated post-session distillation as a secondary safety net.

---

## 5. SQLite Schema

Follows QMD's content-addressable document model. All text lives in a single `content` table keyed by SHA-256 hash. The `documents` table stores all metadata (path, title, docid, tags, summary) and points to content via hash.

```mermaid
erDiagram
    documents ||--o| content : "hash FK"
    content ||--o{ chunks : "hash FK"
    chunks ||--|| chunks_vec : "hash_seq PK"
    documents ||--o{ documents_fts : "triggers"

    documents {
        int id PK
        text doc_type
        text path
        text title
        text hash FK
        text docid UK
        text tags
        text summary
        text created_at
        text updated_at
        text distilled_at
    }
    content {
        text hash PK
        text doc
        text created_at
    }
    chunks {
        text hash FK
        int seq
        text chunk_text
        text model
    }
    chunks_vec {
        text hash_seq PK
        float768 embedding
    }
```

### Content table

Single source of truth for all document text. Content-addressable: identical text inserted twice is deduplicated.

**Hashed payload**: for wiki pages, the hash is computed over the **complete .md file** (frontmatter + body, exactly as written to disk). For source documents, the hash is computed over the full source file content at ingestion time. `lint --fix` detects stale wiki indexes by comparing the on-disk file hash against the stored hash (source documents are SQLite-only and not checked against disk).

```sql
CREATE TABLE IF NOT EXISTS content (
    hash       TEXT PRIMARY KEY,           -- SHA-256 of complete file content
    doc        TEXT NOT NULL,              -- full document text (frontmatter + body)
    created_at TEXT NOT NULL               -- ISO 8601 timestamp
);
```

### Documents table

Maps filesystem paths and doc_types to content hashes. A document is a wiki page or a source.

```sql
CREATE TABLE IF NOT EXISTS documents (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    doc_type    TEXT NOT NULL,             -- 'wiki' or 'source'
    path        TEXT NOT NULL,             -- filesystem path (wiki: relative, source: absolute local path)
    title       TEXT NOT NULL,             -- extracted from frontmatter (wiki) or filename (source)
    hash        TEXT NOT NULL REFERENCES content(hash),
    docid       TEXT NOT NULL,             -- short content hash (6+ chars)
    tags        TEXT NOT NULL DEFAULT '',  -- wiki: from frontmatter; source: empty
    summary     TEXT NOT NULL DEFAULT '',  -- wiki: frontmatter summary if present, else first 120 chars of body; source: empty
    created_at  TEXT NOT NULL,              -- ISO 8601 timestamp
    updated_at  TEXT NOT NULL,             -- ISO 8601 timestamp
    distilled_at TEXT,                     -- NULL = not yet distilled, ISO 8601 = when distilled
    UNIQUE(doc_type, path)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_documents_docid ON documents(docid) WHERE docid != '';
```

**`path` for sources:** The `UNIQUE(doc_type, path)` constraint means the absolute local path serves as the dedup key. `memex import` stores the original transcript path (e.g., `~/.claude/projects/myproject/sessions/abc123.jsonl`), so re-running skips already-ingested files.

**`distilled_at`:** NULL on ingest, set to ISO 8601 timestamp when `/memex-distill` completes. `memex source list --undistilled` queries `WHERE doc_type = 'source' AND distilled_at IS NULL`.

### FTS5 virtual table

Single FTS5 table indexing all documents. Searched with doc_type-filtered queries.

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    path,
    title,
    tags,
    body,
    content='',                              -- contentless: triggers manage content
    tokenize='porter unicode61'
);
```

Contentless FTS (`content=''`) because the `body` column comes from a join with `content` table, not from `documents` directly. Triggers manage all inserts/deletes. This avoids the column mismatch that `content='documents'` would cause on FTS rebuild.

FTS5 triggers join `documents` with `content` to populate the virtual table. The `body` value is the **body only** (after stripping YAML frontmatter), not the full .md file. This prevents frontmatter fields from being double-indexed (title and tags are already separate FTS columns). The same stripping applies to chunking and embedding.

```sql
CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN
    INSERT INTO documents_fts(rowid, path, title, tags, body)
    VALUES (
        new.id, new.path, new.title, new.tags,
        (SELECT strip_frontmatter(doc) FROM content WHERE hash = new.hash)
    );
END;

CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, path, title, tags, body)
    VALUES ('delete', old.id, old.path, old.title, old.tags,
        (SELECT strip_frontmatter(doc) FROM content WHERE hash = old.hash)
    );
END;

CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, path, title, tags, body)
    VALUES ('delete', old.id, old.path, old.title, old.tags,
        (SELECT strip_frontmatter(doc) FROM content WHERE hash = old.hash)
    );
    INSERT INTO documents_fts(rowid, path, title, tags, body)
    VALUES (
        new.id, new.path, new.title, new.tags,
        (SELECT strip_frontmatter(doc) FROM content WHERE hash = new.hash)
    );
END;
```

`strip_frontmatter()` is a Rust function registered as a SQLite application-defined function. It strips YAML frontmatter (text between `---` delimiters at the start of the document) and returns the body only. Source documents without frontmatter are returned as-is.

### Vector search tables

Content-hash-based storage. Embeddings are keyed by content hash + chunk sequence. Survive renames and deduplicate naturally.

```sql
CREATE TABLE IF NOT EXISTS chunks (
    hash        TEXT NOT NULL REFERENCES content(hash),
    seq         INTEGER NOT NULL,          -- chunk index within document
    chunk_text  TEXT NOT NULL,             -- chunk text (body only, frontmatter stripped)
    pos         INTEGER NOT NULL,          -- character offset in body
    len         INTEGER NOT NULL,          -- character length of chunk
    model       TEXT NOT NULL,             -- embedding model name
    embedded_at TEXT NOT NULL,             -- ISO 8601 timestamp
    PRIMARY KEY(hash, seq)
);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
    hash_seq TEXT PRIMARY KEY,             -- "{hash}_{seq}"
    embedding float[768] distance_metric=cosine
);
```

### Mutation ordering invariant

All operations that change `documents.hash` (wiki overwrite via `--force`, backlink rewrite, `lint --fix` reindex, source upsert) or delete a document follow this order:

```mermaid
graph TD
    File["File: write .md to disk"]
    File --> Begin["BEGIN TRANSACTION"]

    Begin --> S1["1. Insert new content hash"]
    S1 --> S2["2. Chunk + embed into chunks / chunks_vec"]
    S2 --> S3["3. Update/delete documents row<br/>(FTS5 triggers fire here)"]
    S3 --> S4{"Old hash orphaned?"}
    S4 -->|yes| S5["4. Orphan cleanup:<br/>DELETE FROM chunks WHERE hash = old<br/>DELETE FROM chunks_vec WHERE hash_seq LIKE old_%<br/>DELETE FROM content WHERE hash = old"]
    S4 -->|no| Commit
    S5 --> Commit["COMMIT"]

    Commit --> Del["File: delete .md from disk (if delete op)"]
```

Filesystem writes happen **outside** the transaction: file written before BEGIN, deleted after COMMIT. If crash occurs between file and DB, `lint --fix` detects the mismatch (untracked file or missing file).

This ordering ensures: FTS triggers have access to content during step 3 (old + new content rows both exist), vector search never sees a document without embeddings (step 2 before step 3), and stale data is cleaned last (step 4). The SQLite transaction guarantees crash safety — partial mutations are rolled back.

### Docid

Short identifier for each document. Every document (wiki and source) gets a docid.

- **Primary**: first 6 characters of the content hash. Extend to 7, 8, ... if another document already has that prefix.
- **Fallback**: if two documents have identical content (same full hash, different paths), the content-hash prefix can never disambiguate. In this case, hash `doc_type + path` and use a 6+ char prefix of that instead. This is rare (duplicate content across different paths).
- Most documents get 6-char content-hash docids (16.7M possibilities, collisions unlikely below ~5,000 documents)
- **Stability**: content-hash docids change when content is edited. Path-hash fallback docids are stable only while the duplicate-content condition persists; if a document is later edited so its content hash becomes unique, the docid reverts to content-hash-based on next recompute. In all cases, docids should be treated as short-lived lookup handles, not permanent identifiers. Agents should use filename stems in citations and cross-references. Wiki links (`[[page-stem]]`) are filename-based for this reason.
- Stored in `documents` table with unique index for O(1) lookup
- Computed on write, collision check is a single `WHERE docid = ?` query per attempt
- `memex read` and `memex delete` accept any identifier form (docid, stem, path, or title) — see identifier resolution in CLI Commands

### Summary

Frontmatter `summary` field if present; otherwise, first 120 characters of the body (after stripping frontmatter). Stored in `documents.summary`. Available for agent use via `memex read` output; not included in search output (search returns docid + score + stem + chunk snippet).

---

## 6. Search & Ranking

```mermaid
graph TD
    Q["Query + optional --lex / --vec / --hyde expansions"]

    subgraph Retrieval["1. Retrieval"]
        BM25["BM25 FTS5<br/>wiki (2x weight) + source (1x weight)<br/>separate list per query term"]
        Vec["Vector search (sqlite-vec cosine)<br/>embed each query, search chunks,<br/>collapse to doc-level by max score"]
    end

    subgraph Fusion["2. Fusion"]
        RRF["RRF merge all ranked lists (k=60)<br/>reassign scores as 1/rank"]
    end

    subgraph Selection["3. Selection"]
        Chunk["Best chunk per doc<br/>(keyword overlap scoring)"]
        Signal{"Signal detection<br/>(wiki BM25 probe only,<br/>skipped if expansions present)"}
    end

    Out["Output: signal · docid · doc_type · score · stem · chunk snippet"]

    Q --> BM25
    Q --> Vec
    BM25 --> RRF
    Vec --> RRF
    RRF --> Chunk
    Chunk --> Signal
    Signal --> Out
```

Subsections below define each step in this pipeline.

### Embedding model

- **Default**: embedding-gemma-300m (ONNX INT8, ~329MB)
  - 768 dimensions, 2048 token context, strong code support (90.1 on CodeSearchNet Retrieval)
- **Bundled**: model pinned by npm package version, downloaded during `postinstall` to `~/.memex/models/`. Model upgrades via `npm update`.
- **Runtime**: ONNX via `ort` crate (prebuilt binaries, no cmake needed, ~135ms/embed on CPU). GGUF/llama.cpp rejected: ~3s/embed on CPU (20x slower), requires cmake + C++ toolchain.

### Chunking

Documents are split into chunks before embedding:
- **Max chunk size**: 900 tokens (within embedding-gemma's 2048 limit)
- **Overlap**: 15% (~135 tokens)
- **Markdown-aware break points** with scored boundaries: heading h1-h6 (score 100-50), code block (80), paragraph (20), list item (5), newline (1). Higher-scored break points are preferred. Code fence protection prevents splitting inside fenced blocks.
- **Search window**: 200 tokens around target split point to find the best break point
- **Both wiki pages and source content are chunked and embedded**

### RRF fusion (following QMD)

Each document $d$ receives an RRF score summed across $n$ ranked lists:

$$\text{RRF}(d) = \sum_{i=1}^{n} \frac{w_i}{k + r_i(d)} + \text{bonus}(r_i(d))$$

where $r_i(d)$ is the **1-based rank** of document $d$ in list $i$ (rank 1 = top result). Documents not in a list are excluded from that list's contribution.

| Parameter | Value | Notes |
|---|---|---|
| $k$ | 60 | Smoothing constant |
| $w_i$ | 2.0 for wiki BM25 lists, 1.0 for all others | By doc_type, not by position (see below) |
| $\text{bonus}$ | +0.05 if $r = 1$, +0.02 if $r \in \{2, 3\}$ | Top-rank bonus |

**List weighting**: wiki BM25 lists (from primary query and each `--lex` term) get weight 2.0. Source BM25 lists and all vector lists get weight 1.0. This is **doc_type-based**, not positional — unlike QMD which weights the first 2 lists by position. Memex uses doc_type-based weighting because `--lex` expansion creates variable numbers of BM25 lists; positional weighting would incorrectly over-weight whichever list happens to be second.

Post-fusion scores reassigned as $s(d) = 1 / r$ where $r$ is the final rank (rank 1 = 1.0, rank 2 = 0.5, rank 3 = 0.33). No minScore filtering (QMD's `hybridQuery` default is 0).

Note: QMD blends RRF rank with local reranker scores via $s = w_{\text{rrf}} \cdot \frac{1}{r} + (1 - w_{\text{rrf}}) \cdot s_{\text{rerank}}$. Memex returns RRF scores with best-chunk snippets; the agent LLM reranks results using the snippets before reading full documents.

### BM25 column weights

| Collection | SQL | Weights |
|---|---|---|
| wiki | `bm25(documents_fts, 1.5, 4.0, 1.5, 1.0)` | path=1.5, title=4.0, tags=1.5, body=1.0 |
| source | `bm25(documents_fts, 1.5, 4.0, 0.0, 1.0)` | path=1.5, title=4.0, tags=0.0, body=1.0 |

Note: QMD has 3 FTS columns (filepath, title, body) with weights (1.5, 4.0, 1.0); memex adds a tags column. Wiki 2x weighting is applied in the RRF list weighting above, not in BM25 scores.

### Score normalization

Raw FTS5 BM25 scores (negative, lower = better) normalized via sigmoid:

$$s = \frac{|x|}{1 + |x|}$$

Query-independent mapping:

| Raw BM25 | Normalized | Interpretation |
|---|---|---|
| $-10$ | $0.91$ | Strong match |
| $-2$ | $0.67$ | Medium match |
| $-0.5$ | $0.33$ | Weak match |
| $0$ | $0$ | No match |

### Signal detection

Runs on the **initial BM25 probe only** (normalized BM25 scores before RRF fusion), not on post-fusion `1/rank` scores. This matches QMD's `hybridQuery()` which probes FTS first, then decides whether to expand.

$$\text{strong} \iff s_1 \geq 0.85 \;\wedge\; (s_1 - s_2) \geq 0.15$$

where $s_1$ and $s_2$ are the top two **normalized BM25 scores** from the wiki doc_type FTS probe. If fewer than 2 results, $s_2 = 0$. If no results, signal is always weak. When strong, the agent skips query expansion. Source and vector evidence do not participate in signal detection (intentional: the BM25 probe is a fast pre-check, not a full search).

### Best chunk selection

For each result document, the best-matching chunk is selected via keyword overlap scoring (primary query terms weighted 1.0, `--lex` terms weighted 1.0, `--vec`/`--hyde` terms weighted 0.5). The chunk snippet is included in the output for agent reranking.

### Vector search: chunk-to-document collapse

Each `--vec`/`--hyde` query (and the primary query) is embedded and searched via sqlite-vec cosine similarity. Chunk hits are collapsed to document-level: for each document, keep the chunk with the highest cosine score, deduplicate by document, rank by max chunk score. Each query produces one document-level ranked list fed into RRF.

### Configuration

Embedding model is a constant in the binary, pinned by the npm package version. No user-facing configuration. Model upgrades ship with npm package updates; run `memex lint --fix` after upgrade to re-embed (see lint command).

---

## 7. Session Ingestion & Distillation

> **SUPERSEDED** by `2026-04-19-daemon-ingestion-design.md`. The section below describes the original hook-based architecture. The current design uses daemon-based ingestion. Retained for historical context.

AI agent sessions generate knowledge — design decisions, debugging findings, research conclusions — that vanishes when the session ends. Session ingestion captures this knowledge into the wiki automatically.

### Two-Layer Architecture

**Primary — In-session agent capture (Trigger 1 in Section 4).**

**Secondary — Post-session distillation (automated):**
When a session ends, a background agent processes the session transcript to catch what the primary mechanism missed. Produces a session summary (audit trail) and extracts any findings the in-session agent didn't capture.

**Post-session flow** — SessionEnd hook triggers distillation:

```mermaid
graph TD
    End["Session ends → SessionEnd hook"]
    End --> Guard{"MEMEX_DISTILLING=1?"}
    Guard -->|yes| Skip["Skip (distillation session)"]
    Guard -->|no| Ingest["memex import &lt;agent&gt; --path<br/>(Rust: parse, strip, store source)"]
    Ingest -->|success| Spawn["Spawn background agent"]
    Ingest -->|fail| Skip
    Spawn --> Distill["/memex-distill:<br/>extract topics → search memex →<br/>create/merge wiki pages →<br/>generate session summary"]
```

**Batch backfill** — user runs `/memex-backfill` in any agent session. See Section 3 for the skill's flow diagram.

### Session lifecycle hooks

**SessionStart hook** — injects relevant memex context. Queries memex for wiki pages relevant to the current project/directory, injects a compact index (title, summary, tags). Agent queries full pages on demand — progressive disclosure avoids context pollution (lesson from claude-mem: v3 injected all history at 35K tokens/1.4% relevance, v4 switched to index-first at 100% relevance). **First-run detection:** if the wiki is empty (no search results), shows a one-time onboarding message: "memex installed. Run `/memex-backfill` to import existing sessions, or just keep working — sessions will auto-capture."

### Recursion guard

The distillation agent creates its own session, which would trigger the SessionEnd hook again. Two mechanisms prevent infinite recursion:

1. **Ephemeral sessions:** The distillation agent runs with no session persistence, leaving no transcript on disk.

   | Agent | Ephemeral flag |
   |---|---|
   | Claude Code | `--no-session-persistence` |
   | Codex CLI | `codex exec --ephemeral` |
   | Gemini CLI | Not available — use env var guard only |

2. **Hook guard (env var):** The hook sets `MEMEX_DISTILLING=1` before spawning the background agent. The hook checks this variable and skips if set.

```bash
# SessionEnd hook — Claude Code example
# Hooks receive JSON via stdin: {"session_id":"...","transcript_path":"...","cwd":"..."}
if [ "$MEMEX_DISTILLING" = "1" ]; then
  exit 0  # Skip — this is a distillation session
fi

# Read stdin JSON and extract transcript_path
INPUT=$(cat)
TRANSCRIPT=$(echo "$INPUT" | sed -n 's/.*"transcript_path"\s*:\s*"\([^"]*\)".*/\1/p')
[ -z "$TRANSCRIPT" ] && exit 0

# Import and capture the docid from the imported: line
DOCID=$(memex import claude-code --path "$TRANSCRIPT" --quiet 2>/dev/null \
  | grep '^imported:' | head -1 | cut -f1 | sed 's/imported: //')
[ -z "$DOCID" ] && exit 0

MEMEX_DISTILLING=1 claude --no-session-persistence --permission-mode auto \
  -p "/memex-distill $DOCID" &
# For Codex: MEMEX_DISTILLING=1 codex exec --ephemeral --full-auto "/memex-distill $DOCID" &
# For Gemini: MEMEX_DISTILLING=1 gemini -p --approval-mode yolo "/memex-distill $DOCID" &
```

Hooks receive JSON via stdin with `session_id`, `transcript_path`, `cwd`, and `hook_event_name`. The hook reads stdin once and extracts `transcript_path` for import. Agent-specific hooks are generated by `postinstall.js` based on the installing agent (Claude Code, Codex, or Gemini).

Agent spawning is in the hook script (shell), not in the Rust binary. This keeps the binary LLM-free and lets users customize the model, flags, and behavior per agent. The distillation agent uses the user's default model — distillation is high-judgment work (topic extraction, merge decisions) where model quality directly determines wiki quality. Each agent uses its auto-approve mode (`--permission-mode auto`, `--full-auto`, `--approval-mode yolo`) so the distillation agent can run memex commands without interactive approval. Recursion is prevented by the `MEMEX_DISTILLING=1` env var guard.

### Agent compatibility

| Agent | Session End Trigger | Session Start | Hook Mechanism |
|---|---|---|---|
| Claude Code | SessionEnd | SessionStart | Lifecycle hook |
| Gemini CLI | SessionEnd | SessionStart | Lifecycle hook |
| Cursor | sessionEnd | sessionStart | Lifecycle hook |
| Codex CLI | Stop | SessionStart | Fires when agent finishes task |
| Amazon Q CLI | Stop | AgentSpawn | Fires when agent finishes task |
| Cline | N/A | N/A | MCP only — use manual ingestion |
| Aider | N/A | N/A | No hooks — use manual ingestion |

### Known agent session locations

| Agent | Session location | Format |
|---|---|---|
| Claude Code | `~/.claude/projects/*/*.jsonl` | JSONL (one event per line) |
| Codex CLI | `~/.codex/sessions/YYYY/MM/DD/*.jsonl` | JSONL (one event per line) |
| Gemini CLI | `~/.gemini/tmp/*/chats/session-*.json` | JSON (single object with messages array) |
| Web exports | User-provided via `--path` | TBD (future) |

### Data model

Sessions are stored as **source documents** (doc_type `source`). Cleaned transcripts get the same treatment as any other source: indexed in FTS5, chunked and embedded for vector search, searchable alongside wiki pages (with wiki results boosted 2x in RRF fusion). Wiki pages produced from sessions link back via the `sources` frontmatter field for provenance.

**Distillation output per session:**
- One wiki page per distinct topic (not one page per session)
- If a topic already has a wiki page, merge new findings into the existing page
- Each wiki page links back to its source session(s) for provenance
- Session summary page for audit trail (goal, findings, outcome, timestamp)

**Evolution tracking:**
When a session revisits a topic that already has a wiki page, the distillation process updates the existing page, timestamps the update, and links to the new source session. Over time, wiki pages accumulate knowledge from multiple sessions with full provenance.

### Cost model

- **In-session capture:** Zero additional LLM cost
- **Post-session distillation:** One LLM session per completed session. Input is a cleaned transcript (10K-50K tokens). Uses the same CLI subscription.
- **Pre-filter:** Non-substantive sessions (aborted, no user interaction, malformed) are skipped before the LLM pass

---

## 8. Plugin Distribution

```
memex/
├── .claude-plugin/
│   └── marketplace.json        # npm source: @memverge/memex
├── Cargo.toml                  # Workspace: core, cli
├── CLAUDE.md                   # Developer instructions
├── AGENTS.md                   # Developer instructions
├── core/                       # memex-core library (storage, search, validation)
├── cli/                        # memex-cli binary (write, read, search, delete, lint, import, source)
├── plugin/                     # Cross-platform agent plugin (published as @memverge/memex)
│   ├── package.json            # npm package with postinstall script
│   ├── postinstall.js          # Downloads binary to PATH + model to ~/.memex/models/
│   ├── .claude-plugin/plugin.json
│   ├── .codex-plugin/plugin.json
│   ├── .codex/INSTALL.md
│   ├── .cursor-plugin/plugin.json
│   ├── gemini-extension.json
│   ├── CLAUDE.md               # Plugin user instructions + proactive behavior
│   ├── AGENTS.md
│   ├── GEMINI.md
│   ├── skills/
│   │   ├── memex-query/SKILL.md
│   │   ├── memex-ingest/SKILL.md
│   │   ├── memex-brainstorm/SKILL.md
│   │   ├── memex-distill/SKILL.md
│   │   └── memex-backfill/SKILL.md
│   └── hooks/
│       └── hooks.json              # SessionEnd + SessionStart hooks
└── docs/
```

### Audience separation

- Root `CLAUDE.md` / `AGENTS.md` — for developers working ON memex
- `plugin/CLAUDE.md` / `plugin/AGENTS.md` — for users of the plugin (loaded when installed)

### Install

Distributed as a single npm package (`@memverge/memex`). The `postinstall` script downloads the platform-specific prebuilt `memex` binary from GitHub Releases and the embedding model (~329MB) from HuggingFace. Everything is ready after install — no separate steps.

| Platform | Install |
|----------|---------|
| Claude Code | `/plugin install memex@memverge` (marketplace runs `npm install` automatically) |
| Codex | `npm install @memverge/memex`, configure skill directory |
| Gemini CLI | `npm install @memverge/memex`, configure extension directory |
| Cursor | `npm install @memverge/memex`, configure plugin directory |
| Any agent | `npm install @memverge/memex` |

The `postinstall` script detects the platform (macOS arm64/x86, Linux x86, Windows x86_64), downloads the correct prebuilt binary to the package's `bin/` directory, and downloads the embedding model to `~/.memex/models/`. The npm `"bin"` field in `package.json` points to a thin JS wrapper that spawns the platform binary — npm automatically makes this available on PATH (symlinks on Linux/macOS, `.cmd` wrappers on Windows). Skills call `memex` directly.

For developers building from source: `cargo install --path cli` (puts `memex` on PATH; plugin skills fall back to PATH lookup if `$PLUGIN_DIR/bin/memex` doesn't exist).

**Marketplace entry** (in root `.claude-plugin/marketplace.json`):
```json
{
  "name": "memex",
  "source": {
    "source": "npm",
    "package": "@memverge/memex"
  }
}
```

---

## 9. Deferred

**Source ingestion:**
- **PDF/image source extraction**: current design stores text sources only. PDF text extraction and image description generation are future additions.
- **Source content for URLs**: mechanism for storing web content fetched by the agent. May need a stdin protocol for passing pre-fetched content.

**Knowledge management:**
- **Confidence scoring and decay**: facts from older sessions could carry lower confidence. Deferred until the wiki is large enough for staleness to be a real problem.
- **Consolidation tiers**: working memory → episodic → semantic → procedural. Interesting for scale, but premature for v1.
- **Knowledge graph**: entity extraction, typed relationships, graph traversal. Adds value at scale, not needed initially.

**Infrastructure:**
- **MCP server**: expose memex operations as MCP tools for richer structured integration.
- **Transcript watching**: file-system watching as an alternative to hooks (used by claude-mem for Codex CLI). Could be added for agents where hooks are unreliable.
- **Multi-agent coordination**: multiple agents writing to the same wiki simultaneously. Handle when it becomes a real use case.

---

## References

- Karpathy's LLM-wiki idea: https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f
- QMD (Query Markup Documents): https://github.com/tobi/qmd
- LLM Wiki v2 (agentmemory patterns): https://gist.github.com/rohitg00/2067ab416f7bbe447c1977edaaa681e2
- claude-mem (reference implementation): https://github.com/thedotmack/claude-mem
