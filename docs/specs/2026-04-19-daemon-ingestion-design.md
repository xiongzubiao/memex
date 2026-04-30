# Daemon-Based Session Ingestion & Write Routing

**Status:** Draft
**Date:** 2026-04-19
**Supersedes:** Session ingestion sections of `2026-04-07-memex-design.md` (Section 7)
**Modifies:** `2026-04-17-memex-query-daemon-design.md` (expands daemon scope from read-only to read-write)

## Context and motivation

Session ingestion currently runs in a SessionEnd hook. The hook parses the transcript, stores it as a source document, embeds chunks with ONNX, writes a temp file, and spawns a fresh agent subprocess to run `/memex-ingest`. This architecture has five problems:

1. **Timeout pressure.** The hook runs synchronously during session exit. Claude Code's default SessionEnd timeout is ~1.5s (configurable to 300s via `CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS`). Spawning an agent subprocess that loads an ONNX model and runs LLM calls within this window is fragile.

2. **Cold resources.** Each hook invocation cold-starts the ONNX embedding model and spawns a fresh agent subprocess. The daemon already has both warm.

3. **Agent subprocess fragility.** The hook must handle agent-specific permission flags (`--permission-mode auto`, `--full-auto`, `--approval-mode yolo`), Codex's `codex_hooks` feature flag, background process limitations (`kill_on_drop`), and stdout parsing differences (Codex Stop hooks parse stdout as JSON).

4. **SQLite contention.** The hook writes directly to SQLite while the daemon may also be reading. WAL mode handles this, but single-writer is cleaner.

5. **Duplicated infrastructure.** The hook reimplements parsing, storage, embedding, and agent dispatch. The daemon already has all of this for query expansion and synthesis.

This spec moves all ingestion into the daemon and expands the daemon from a read-only query engine to the single mutating engine for memex.

### Relationship to prior specs

- **`2026-04-07-memex-design.md` Section 7** described hook-based ingestion with `memex hook session-end`, `/memex-distill` skill, `distilled_at` tracking, and background agent spawning. This spec replaces that entire section.

- **`2026-04-17-memex-query-daemon-design.md`** scoped the daemon as read-only ("Routing other commands through the daemon is a possible future optimization... deferred"). This spec un-defers that: all mutations now route through the daemon.

- **Parallel access** (`2026-04-16-parallel-access-design.md`): The daemon becomes the single writer to SQLite. The wiki writer flock (Lock 1) is still acquired per write operation, but only by the daemon process. CLI write commands become daemon clients, not direct writers.

### Non-goals

- Replacing `memex search` or `memex read` as direct CLI commands. Read-only operations stay in-process.
- MCP server. Separate architectural choice, can be layered later.
- Cross-agent session format normalization. Each agent's transcript format is parsed as-is.

## Architecture overview

```
Session ends
    |
    v
Hook: memex ingest --agent claude-code
    |  reads stdin JSON, sends Request::Ingest to daemon, exits (<200ms)
    v
Daemon (persistent process)
    |
    |-- Dedup check (content hash in source doc_type)
    |-- Parse transcript (agent-specific parser)
    |-- Filter (NonSubstantive / InternalSession -> skip)
    |-- Dispatch IngestJob to worker pool
    |       |
    |       |-- LLM Call 1 (extract): transcript -> full wiki pages
    |       |
    |       |-- Daemon searches for each proposed title (dedup)
    |       |-- New pages: store directly from Call 1 output
    |       |-- Matching pages: read existing content from disk
    |       |
    |       |-- LLM Call 2 (merge, only if matches found):
    |       |       proposed + existing pages -> merged content
    |       |
    |       v
    |-- Store source document + wiki pages
    |-- Embed all content with warm ONNX
    v
Done
```

### CLI as thin clients

| Command | Before | After |
|---------|--------|-------|
| `memex write` | Direct SQLite + ONNX | `Request::Write` to daemon |
| `memex delete` | Direct SQLite | `Request::Delete` to daemon |
| `memex lint --fix` | Direct SQLite | `Request::LintFix` to daemon |
| `memex ingest` | N/A (was `memex hook session-end`) | `Request::Ingest` to daemon (top-level command) |
| `memex backfill` | N/A (was `/memex-backfill` skill) | Batch `Request::Ingest` to daemon |
| `memex query` | Already through daemon | No change |
| `memex search` | Direct SQLite | No change (read-only) |
| `memex read` | Direct filesystem | No change (read-only) |

All mutations route through daemon. All reads stay direct.

## 1. Thin hook

The SessionEnd hook becomes a simple notification.

**hooks.json entry:**
```json
{
  "hooks": [{
    "type": "command",
    "command": "memex ingest --agent claude-code",
    "timeout": 10
  }]
}
```

**`memex ingest --agent <agent>` implementation:**

1. Read stdin JSON, extract `transcript_path`
2. Connect to daemon socket via `connect_or_spawn()` (auto-starts daemon if needed)
3. Send `Request::Ingest { transcript_path, agent, memex_root }`
4. Read response: `Queued { job_id }` or `Error`
5. Exit

Under 200ms with warm daemon. Cold start (daemon not running) takes 1-5 seconds for `connect_or_spawn()` to start the daemon and connect. This is still far better than the current hook architecture (minutes for full pipeline). No parsing, no SQLite, no ONNX, no agent spawning in the hook.

**`MEMEX_INTERNAL=1` guard:** The daemon sets this env var on all worker subprocesses (expansion, synthesis, distillation). The hook checks it first. If set, exit immediately. Prevents ingesting the daemon's own sessions.

**Agent detection in postinstall.js** remains the same: check `CLAUDECODE=1`, `CODEX_CI=1`, `GEMINI_CLI=1`, fall back to binary detection. Hook event name: `SessionEnd` for Claude Code and Gemini, `Stop` for Codex.

## 2. Daemon protocol additions

New request types added to `protocol.rs`:

```rust
enum Request {
    // Existing
    Ping { v: u32 },
    Query { v: u32, question: String, raw: bool, top_k: usize, memex_root: String },

    // New: mutations
    Write {
        v: u32,
        title: String,
        content: String,
        tags: Vec<String>,
        sources: Vec<String>,
        force: bool,
        memex_root: String,
    },
    Ingest {
        v: u32,
        transcript_path: String,
        agent: String,  // "claude-code", "codex", "gemini"
        memex_root: String,
    },
    Delete { v: u32, slug: String, memex_root: String },
    LintFix { v: u32, memex_root: String },
}
```

New response events:

```rust
enum Event {
    // Existing
    Pong { pid, started_at },
    Queued { job_id: String, ahead: u32 },
    Answer { text, citations },
    Expansion { lex, vec, hyde },
    Context { pages },
    Error { code, message, status },
    Done { status },

    // New
    Written {
        slug: String,
        docid: String,
        // Per the auto-cross-link rule (canonical-arch §3.7); empty
        // vectors are skip_serializing_if = "Vec::is_empty" so the
        // wire shape stays unchanged when nothing fires.
        linked: Vec<String>,
        backlinked: Vec<String>,
        suggest_create: Vec<String>,
    },
    Deleted { slug: String },
    // Per-issue progress for Request::LintFix (streamed before LintResult).
    LintFixed { page: String, kind: String },
    LintAlreadyFixed { page: String },
    LintRemaining { page: String, kind: String, target: String },
    LintResult { fixed: u32, remaining: u32 },
    Parsing { job_id: String, transcript_path: String },
    Distilling { job_id: String, transcript_path: String },
    Stored { job_id: String, source_docid: String, wiki_pages: Vec<String> },
}
```

## 3. Daemon ingest pipeline

When the daemon receives `Request::Ingest`:

### 3.1 Input validation and dedup

**Path validation:** Verify `transcript_path` is an absolute path within known agent session directories (`~/.claude/projects/`, `~/.codex/sessions/`, `~/.gemini/sessions/`, or platform equivalents). Reject paths outside these directories to prevent path traversal attacks via malicious hook payloads. Respond `Error` with a clear message.

**Dedup:** Compute SHA-256 hash of the transcript file content. Check if this hash already exists in the source doc_type's content table. If yes, respond `Done` and skip. This is content-based dedup, not path-based, so file moves or renames don't cause re-ingestion, and file overwrites with new content are correctly re-processed.

### 3.2 Parse and clean

Read file from disk. Select parser by agent:

| Agent | Parser | Format |
|-------|--------|--------|
| `claude-code` | `parse_claude_code_session()` | JSONL, one JSON object per line |
| `codex` | `parse_codex_session()` | JSONL, event_msg for user messages |
| `gemini` | `parse_gemini_cli_session()` | Single JSON object with messages array |

Produces `CleanedTranscript` with `cleaned_text`, `session_id`, `agent`, `first_user_message`, `filter`.

#### Transcript cleaner specification

The raw transcript is mostly machinery. Empirical measurement of a 15MB session (1,897 tool calls) shows:

| Category | % of raw transcript | Action |
|----------|-------------------|--------|
| Tool results (file contents, command output) | 37.5% | **Strip** — raw data, in git |
| Other metadata (permission-mode, file-history, etc.) | 27.1% | **Strip** |
| Tool use inputs (full JSON args) | 18.7% | **Summarize** — keep one-line summary via `summarize_tool_input()` |
| Assistant text (reasoning, discussion, code) | 8.9% | **Keep** |
| User messages (questions, requests) | 7.8% | **Keep** |
| System-reminder, thinking blocks | <1% | **Strip** |

**What to keep:**

- **User messages** (string content) — the questions, requests, and decisions. The knowledge intent.
- **Assistant text blocks** — reasoning, analysis, design discussions, error explanations. The knowledge content.
- **Tool call summaries** — one-line per tool call via `summarize_tool_input()`, e.g. `[Tool: Read — file: src/auth.rs]`, `[Tool: Bash — command: cargo test]`. Provides context about what was investigated without the raw output (~50-100 chars each).

**What to strip:**

- **Tool result blocks** — full file contents, command outputs, search results. These are point-in-time snapshots of system state. The files are on disk (and may have changed). The assistant text interprets these results. Stripping saves ~37% of transcript.
- **Tool use input JSON** — the full `{"file_path": "...", "offset": 0, "limit": 200}`. Replaced by the one-line summary. Stripping saves ~19%.
- **Metadata lines** — `permission-mode`, `file-history-snapshot`, `attachment`, `queue-operation`, `last-prompt`. Not knowledge.
- **Thinking blocks** — Claude Code stores these with empty content and a cryptographic signature. The actual thinking text is not persisted to disk. Nothing to extract.

**Tags to strip from content:**

| Tag | Source | Why |
|-----|--------|-----|
| `<system-reminder>` | Claude Code | CLAUDE.md injection, deferred tool lists, skill definitions, task reminders |
| `<private>` | User explicit | User opted out of knowledge capture |
| `<memex-context>` | Memex SessionStart hook | Prevents circular ingestion of memex's own injected context |
| `<persisted-output>` | Claude Code | Already-stored tool output markers |

**Secret redaction** runs on all kept content via regex patterns for API keys (`sk-...`), AWS credentials (`AKIA...`), and password/token/secret assignments.

**Result:** A 15MB raw transcript (1.37M tokens) cleans to ~228K tokens (user + assistant text + tool summaries). Most current models offer 1M context variants at the same per-token price, and daemon ingestion runs in the background with no latency requirement, so prefer the largest context available.

**Overflow handling:** The context limit is determined dynamically from the configured model (via daemon worker config). If the cleaned transcript exceeds the model's context limit minus output reserve, truncate from the beginning (oldest turns removed first). Log a warning with the original and truncated sizes.

### 3.3 Filter

Check `CleanedTranscript.filter`:

| Filter | Meaning | Action |
|--------|---------|--------|
| `Pass` | Real user session with both user and assistant messages | Continue |
| `NonSubstantive` | Missing user or assistant messages (aborted, empty) | Skip, respond `Done` |
| `InternalSession` | Daemon-spawned work (distillation, expansion, synthesis) | Skip, respond `Done` |

Filtering costs zero (no LLM, no storage, no embedding).

### 3.4 Durable job queue and dispatch

**Persist before acking.** Before responding `Queued`, write the job to a `ingest_jobs` SQLite table:

```sql
CREATE TABLE ingest_jobs (
    job_id TEXT PRIMARY KEY,
    transcript_path TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    agent TEXT NOT NULL,
    memex_root TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending, processing, completed, failed
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    error TEXT
);
```

This ensures that if the daemon crashes after acking `Queued` but before completing the job, the row survives. On daemon startup, scan for `status = 'pending'` or `status = 'processing'` (crashed mid-job) and mark them `failed` with `error = 'interrupted by daemon restart'`. The recovery does NOT re-dispatch — rebuilding the worker payload requires re-reading the source path (which may have moved or been deleted) and can fail in different ways than the original. The user re-runs `memex ingest` if they want the work done. The `updated_at` bump on recovery resets the 30-day prune window so the user has time to investigate the crash; see canonical-arch §2.1 for details.

**Dispatch.** Send `IngestJob` to the worker pool. No worker pinning required — Call 1 and Call 2 are independent, stateless calls that can be handled by any worker. The worker pool's existing `timeout_sec` config (default 60s) applies per LLM call.

**Serialization.** Steps 3.1-3.5 (validation, parse, filter, dispatch, LLM Call 1) run fully in parallel across concurrent ingest jobs. Serialization is only needed from step 3.6 onward (dedup search, merge, validate, store). This prevents merge races where two concurrent jobs read the same page, generate independent merges, and the last writer silently overwrites the first. Query and synthesis jobs are unaffected and run concurrently as before.

Respond `Queued { job_id }` to the client. Client can disconnect or stay connected for progress events (correlated by `job_id`).

### 3.5 Worker: LLM Call 1 (Extract)

Worker sends to its agent subprocess:

**Input:**
- Cleaned transcript text
- Instructions: "Extract knowledge from this transcript into complete wiki pages. For each topic worth capturing, output a wiki page with slug, title, tags, and full markdown body. Include not just WHAT but WHY — reasoning, qualifications, specific numbers, tradeoffs. Each page should be self-contained: a reader should understand the full picture without the original transcript."

**Output:** Full wiki pages, e.g.:
```yaml
- slug: oauth-token-debugging
  title: OAuth Token Debugging
  tags: [auth, debugging, tokens]
  body: |
    ## Problem
    Token cache TTL was set to 3600s but tokens expire at ~3500s
    due to clock skew between auth server and client...
    ## Root Cause
    ...
    ## Fix
    ...
```

Token cost: cleaned transcript (~50K typical, see Section 3.2 cleaner for overflow handling) + instructions (~2K) = ~52K input, ~15K output.

### 3.6 Dedup search

For each proposed page from Call 1, the daemon searches for existing wiki pages with overlapping topics using `hybrid_retrieve()` with the proposed page title as query. This is the same dedup pattern as the `/memex-ingest` skill: propose first, search after. 3-5 searches total, cheap.

If a match is found, the daemon reads the existing page content from disk. If no match, the proposed page is stored directly from Call 1 output — no Call 2 needed for new pages.

### 3.7 Worker: LLM Call 2 (Merge) — only if needed

Skipped entirely when all proposed pages are new (no matches found in 3.6). Empirically, ~91% of sessions produce only new pages.

When merges are needed:

**Input:**
- Proposed page content (from Call 1, only pages that need merging)
- Existing page content (from disk)
- Instructions: "Merge the new content into the existing page. Preserve existing structure. Add new information in context. Resolve any contradictions in favor of the newer information, noting what changed and why."

**Output:** Merged page content.

Token cost: proposed pages (~6K for 2 pages) + existing pages (~6K) + instructions (~2K) = ~14K input, ~6K output. Only for pages that need merging.

**Total per ingestion:**
- Common case (all new, ~91%): ~52K input + ~15K output. One call.
- Merge case (~9%): ~52K + ~14K = ~66K input, ~15K + ~6K = ~21K output. Two calls.
- No prompt caching required. No worker pinning required. Calls are independent and stateless.

### 3.8 Validate LLM output

Before storing, validate the LLM's structured output:

1. **Schema check:** Each page must have `slug`, `title`, `tags` (array), and `body` (non-empty string). Reject pages missing required fields.
2. **Slug normalization:** Force kebab-case, strip leading/trailing hyphens, collapse consecutive hyphens. Reject slugs containing path separators or special characters.
3. **Page count limit:** Cap at 10 wiki pages per ingestion. If the LLM outputs more, take the first 10 and log a warning. Prevents runaway generation.
4. **Body size limit:** Cap each page body at 20K characters. Truncate with a warning if exceeded.
5. **Collision policy:** If a `create` action targets a slug that already exists (missed by dedup search in 3.6), downgrade to `update`. If an `update` targets a slug that doesn't exist, downgrade to `create`.

### 3.9 Store with explicit transaction boundaries

After validation, the daemon stores everything in a defined order with rollback on failure:

**Transaction scope:**

```
BEGIN TRANSACTION
  1. insert_content() for source document (SHA-256 hash)
  2. upsert_document() for source in source doc_type
  3. For each wiki page:
     a. insert_content() for page body
     b. upsert_document() in wiki doc_type
     c. Insert chunks into chunks table
     d. Embed chunks with warm ONNX, store embeddings
  4. Update backlinks for all affected pages
COMMIT
```

**Filesystem writes** (markdown files to `wiki/`) happen BEFORE the SQLite transaction commits, per the filesystem-canonical principle (`2026-04-25` §3.4): bodies on disk are the source of truth, so they land first. If any wiki-file write fails, the daemon aborts before touching the index — the alternative (commit row, then write file, then notice failure) leaves a `documents` row pointing at nothing readable. The reverse order leaves orphan files on DB-commit failure, which `memex lint`'s reindex-from-disk path *does* recover.

**ONNX embedding failure:** If embedding fails for a chunk, store the chunk with a hash-based embedding as fallback. Log a warning. The chunk is searchable via BM25 but not via vector search until re-embedded.

**On success:** Update `ingest_jobs` row to `status = 'completed'`. Send `Stored` event.

**On failure:** Update `ingest_jobs` row to `status = 'failed'` with error message. Send `Error` event. Worker retries per existing retry policy (1 retry after soft reset). If retries exhausted, job stays `failed` for manual inspection.

If LLM calls fail before storage, nothing is stored, job is marked `failed`.

## 4. Write routing

`memex write` becomes a daemon client.

**Before:**
```
memex write "Title" --source /path < content
    -> opens SQLite directly
    -> cold-starts ONNX
    -> inserts content, embeds, writes file
```

**After:**
```
memex write "Title" --source /path < content
    -> connect_or_spawn() to daemon
    -> send Request::Write { title, content, tags, sources, force }
    -> receive Written { slug, docid }
```

The daemon handles all storage: content insertion, document upsert, embedding, auto cross-linking (`forward_link`, `maintain_backlinks`, `suggest_create` per `2026-04-25` §3.7), and source document storage (for `--source`).

**Test-only seeder.** Tests need deterministic wiki pages with exact frontmatter for assertions. The production daemon write path synthesizes frontmatter and runs through the full LLM path, which is non-deterministic. `memex_cli::test_utils::seed_wiki_page` is a `feature = "test-harness"`-gated helper that mirrors `handle_write` in-process without the daemon round-trip; production binaries don't include it. (Earlier drafts of this spec described a `--direct` CLI flag for the same purpose; the flag has been removed and the logic moved into the test helper.)

**Benefits:**
- Warm ONNX for every write (no cold-start)
- Single writer to SQLite (no contention)
- Consistent architecture (CLI = client, daemon = engine)
- Auto cross-linking happens uniformly via the daemon, never via a side path

## 5. Backfill

`memex backfill <agent>` replaces the `/memex-backfill` skill.

**Implementation:**

1. Discover session files using glob patterns per agent
2. For each file, send `Request::Ingest { transcript_path, agent, memex_root }` to daemon
3. Stream progress: client tracks "N of M done" from `Done` events
4. Report summary

The daemon processes each ingest through the same pipeline as session-end hooks. Dedup ensures already-ingested sessions are skipped.

**User experience:**
```
$ memex backfill claude-code
Discovered 347 sessions
Queued 312 (35 already ingested)
Processing... 47/312
```

User can Ctrl+C. Daemon continues processing queued jobs. Re-running `memex backfill` skips completed sessions.

## 6. Skill changes

### `/memex-ingest` (modified)

Interactive ingestion skill, used when a user provides material manually. Single LLM session (no subagents, cheaper for typical ~10 tool calls).

Changes:
- Read all wiki page titles upfront for better dedup decisions
- Remove per-topic search for dedup (titles in context handle it)
- Writes via `memex write` which now routes through daemon

Flow:
1. Read source file
2. List all wiki page titles (`memex list --titles` or equivalent)
3. With source + titles in context, propose pages to create/update
4. Ask user for approval
5. For updates: `memex read <slug>` to get existing content for merge
6. Write merged/new pages via `memex write --source <path>`

### `/memex-distill` (removed)

Replaced by daemon's automatic distillation. No manual distillation step needed.

### `/memex-backfill` (removed)

Replaced by `memex backfill` CLI command.

## 7. Data model changes

### Removed

- `distilled_at TEXT` column from documents table. Source + wiki pages are stored atomically. No intermediate "stored but not distilled" state.
- `memex source list --undistilled` command
- `memex source mark-distilled` command
- `memex hook session-end` command group

### Added

- `Request::Write`, `Request::Ingest`, `Request::Delete`, `Request::LintFix` in daemon protocol
- `Written`, `Deleted`, `LintResult`, `Parsing`, `Distilling`, `Stored` response events (all progress events include `job_id` for correlation)
- `memex ingest --agent <agent>` subcommand
- `memex backfill <agent>` subcommand
- `memex status` command (read-only, shows daemon state and recent ingest activity)
- `ingest_jobs` SQLite table for durable job tracking (see Section 3.4)

## 8. User-visible feedback

The daemon processes ingestion asynchronously after the hook exits. Users need to know the system is working.

**SessionStart hook enhancement:** On the next session start after ingestion completes, the SessionStart hook can report: "memex: ingested 2 new wiki pages from your last session." This uses the existing SessionStart hook infrastructure and the `ingest_jobs` table to detect recently completed jobs.

**`memex status` command:** New read-only command showing recent ingest activity:
```
$ memex status
Daemon: running (pid 12345, 3 workers)
Last ingestion: 2 minutes ago (3 pages created, 1 updated)
Pending jobs: 0
Failed jobs: 0
Wiki: 47 pages, 12 sources
```

**Daemon log:** All ingest activity logged to `~/.memex/daemon.log` with structured entries for debugging.

## 9. Internal session guard

The daemon sets `MEMEX_INTERNAL=1` on all worker subprocesses. This prevents:

- **Expansion sessions** from being ingested
- **Synthesis sessions** from being ingested
- **Distillation sessions** from being ingested

The thin hook checks this env var first. If set, exit immediately. Zero cost.

Belt-and-suspenders: the `InternalSession` filter variant catches daemon prompts even if the env var isn't set (e.g., first user message matches known daemon prompt patterns).

## 10. Token cost analysis

### Per-session ingestion (Extract + Merge)

**Common case — all new pages (~91% of sessions):**

| Component | Tokens |
|-----------|--------|
| Call 1 input: transcript | ~52K |
| Call 1 output: full wiki pages | ~15K |
| Call 2: skipped | — |
| **Total input** | **~52K** |
| **Total output** | **~15K** |

One call. No caching dependency. No worker pinning.

**Merge case (~9% of sessions, 2 of 5 pages need merging):**

| Component | Tokens |
|-----------|--------|
| Call 1 input: transcript | ~52K |
| Call 1 output: full wiki pages | ~15K |
| Call 2 input: proposed + existing pages | ~14K |
| Call 2 output: merged content | ~6K |
| **Total input** | **~66K** |
| **Total output** | **~21K** |

Two independent, stateless calls. Any worker can handle either.

### Comparison with alternatives

| Approach | Input (common) | Input (merge) | Calls | Caching needed? | Worker pinning? |
|----------|---------------|---------------|-------|----------------|----------------|
| Extract + Merge (this design) | ~52K | ~66K | 1-2 | No | No |
| Plan + Execute with caching | ~74K | ~74K | 2 always | Yes | Yes |
| Tool-use (single session) | ~200K | ~200K | 1 session, ~9 round-trips | No | No |
| Plan + Execute, titles in prompt | ~114K | ~114K | 2 always | Yes | Yes |

Extract + Merge is cheapest for the common case and architecturally simplest. The merge case costs more output tokens but occurs infrequently (~9% based on empirical analysis of 978 sessions against 10 wiki pages).

### Merge rate projection

The 9% merge rate is based on a young wiki (10 pages). As the wiki grows, the merge rate will increase. Even at 50% merges, Extract + Merge's average cost (~59K input) is below Plan + Execute (~74K input), and the architectural simplicity (no caching, no pinning) remains.

<!-- AUTONOMOUS DECISION LOG -->
## Decision Audit Trail

| # | Phase | Decision | Classification | Principle | Rationale | Rejected |
|---|-------|----------|---------------|-----------|-----------|----------|
| 1 | CEO | Mode: SELECTIVE EXPANSION | Mechanical | P1+P6 | Plan is well-scoped, surface expansions individually | SCOPE EXPANSION (overkill), HOLD SCOPE (misses improvements) |
| 2 | CEO | Accept distillation quality as post-launch iteration | Mechanical | P6 | User confirmed. Architecture enables quality work regardless | Validation gate before building |
| 3 | CEO | Daemon as default writer with --direct escape hatch | Resolved | P1+P5 | Clean architecture as default, break-glass for emergencies | Full SPOF (no fallback) or full fallback (two paths) |
| 4 | Eng | Address all 12 eng findings in spec update | Mechanical | P1 | All findings are real gaps, none deferrable | Defer any finding |
| 5 | Eng | Serialize ingest jobs touching overlapping pages | Mechanical | P5 | Explicit over clever. Sequential is simpler than optimistic locking | Optimistic locking |
| 6 | Eng | Dedup on content hash, not path | Mechanical | P5 | Correct behavior. Path dedup has known failure modes | Keep path dedup |
| 7 | Eng | Add durable job queue (SQLite table) | Mechanical | P1 | Non-durable queue = silent data loss | Accept data loss risk |
| 8 | DX | Add user-visible feedback for ingest results | Mechanical | P1 | Users need to know the system is working | Silent operation |
| 9 | DX | `memex ingest` as top-level command | Resolved | P5 | Consistent with write/query/backfill, daemon routing transparent | Keep under daemon subcommand |
