# Web Page (and General Document) Ingestion

**Status:** Draft (revised post-eng-review)
**Date:** 2026-04-24
**Extends:** `2026-04-19-daemon-ingestion-design.md` (adds a new ingest source type to the existing daemon pipeline)
**Revision:** Folds in P0/P1 fixes from the engineering review. Adds chunking for long documents.

## Context and motivation

Memex ingests knowledge from two sources today:

1. **Agent session transcripts** — daemon-backed, triggered by SessionEnd/Stop hooks, runs `memex ingest --agent <agent>` with a transcript path on stdin.
2. **Local text files** — interactive, via the `/memex-ingest` skill. The skill uses the agent's `Read` tool, then proposes pages and writes them through `memex write`.

Users want to capture knowledge from URLs (articles, docs, posts) and, over time, from other non-text sources (PDFs, Office docs, audio recordings, YouTube URLs). Today the only path is manual: open the page, copy the body, paste into chat, hope the skill extracts it usefully.

This spec adds a generic **document** source type to the existing daemon pipeline, plus a skill path for interactive ingestion. Memex does **not** own the fetching or format-conversion step — users compose a converter of their choice (`markitdown`, `pandoc`, `curl | readability`, etc.) into their shell pipeline, or the skill invokes one on their behalf.

### Why memex does not own the converter

Memex's scope is LLM-based knowledge extraction from text. HTTP fetching and format conversion are separate concerns with mature tools. Wrapping one converter couples memex to that tool's install, error surface, and release cadence. Users already compose Unix pipelines; `converter | memex ingest --source <id>` is idiomatic and lets anyone swap the converter without memex code changes.

### Non-goals

- Built-in fetching. No HTTP client in memex.
- Built-in format conversion. No `memex fetch`. No markitdown, pandoc, or readability as runtime or build dependencies of memex.
- Authenticated / paywalled pages, JS-rendered SPAs, cookies, custom User-Agents. Fully delegated to the user's chosen converter.
- Recursive crawling, bulk URL import. One source per invocation.
- Offline archival of the fetched content beyond the content-addressed copy used for dedup and source retrieval.
- Cross-source alias tracking. If the same content is reached via two URLs (e.g., tracking-param variant and canonical), the first identifier wins as provenance. Acceptable for a personal wiki; logged as a known limitation.

## Design overview

```
# CLI — user composes:
markitdown https://example.com/post | memex ingest --source https://example.com/post
pandoc -t markdown paper.docx | memex ingest --source paper.docx
memex ingest ~/notes.md                 # text/Markdown file: memex reads it directly

# Skill — /memex-ingest https://example.com/post
#   1. skill runs markitdown, captures Markdown
#   2. skill calls `memex source add <url>` to store the source, gets a docid
#   3. skill runs its interactive propose/approve loop
#   4. skill runs `memex write <slug> --source <docid>` per approved page
```

`memex ingest --source` reads Markdown (or any text) from stdin, tags it with the `--source` identifier as provenance, and hands it to the daemon's existing Extract+Merge pipeline — same job queue, same dedup, same validation, same transactional store, same embedding. The new machinery:

1. A new `IngestSource::Document` protocol variant carrying pre-cleaned content.
2. **Chunking** for documents whose content exceeds a single Extract call's comfortable input budget (§3.7 below). Each chunk is Extracted independently; same-slug pages across chunks are merged.
3. A unified Extract prompt envelope (§3.9): `{"segments":[...], "source", "chunk_index"?, "total_chunks"?}`. The same `[TASK: EXTRACT]` system-prompt section handles both transcripts and documents, branching on whether segments carry a `role` field.
4. A refactored `IngestJob` carrying the unified envelope plus reuse of the existing `MergeJob` for cross-chunk fusion. No new `BackendJob` variant.
5. A new CLI helper `memex source add` to support the skill's interactive path without breaking `memex write`'s file-based source model.

Everything downstream of Extract — wiki-side dedup search, Merge, validate, transactional store, embedding — is shared verbatim with the transcript pipeline.

## 1. User-visible surface

### 1.1 CLI — three modes with agent inference

`memex ingest` has three mutually exclusive shapes:

```
memex ingest <path>                                     # auto-detect: transcript (any agent) → use detected parser; otherwise → document
memex ingest --agent <a> <path>                         # explicit override (rare; for unusual files or future agents)
memex ingest --source <id>                              # stdin: content piped in, --source is provenance
```

The positional argument carries the file path for both transcript and document modes. The `--agent` flag is an **optional override** when inference would be wrong or when an unfamiliar transcript format needs to be forced.

**Agent inference (§3.13):** when `--agent` is omitted, memex inspects the file:
- `.json` whose content is a single object with a `messages` array → Gemini CLI transcript.
- `.jsonl` whose first non-empty line has `{"type": "user|assistant|system", "uuid"|"parentUuid": ...}` → Claude Code transcript.
- `.jsonl` whose first non-empty line has `event_msg` or other Codex-specific keys → Codex transcript.
- Anything else → document file mode (memex reads the file as text content).

**Three modes summarized:**

- **Transcript mode** (auto-detected or `--agent <a>`) — positional `<path>` to a transcript file on disk. The agent name picks the parser (JSONL for Claude Code / Codex, JSON for Gemini CLI); memex reads the file and parses it. `--agent` is required only when inference would be wrong (rare).
- **Document file mode** (positional `<path>` that doesn't match any transcript signature) — text or Markdown file on disk. Memex reads the file and uses the path as both the source identifier and the content. No pipe required: `memex ingest ~/notes.md`.
- **Stdin mode** (`--source <id>`) — content piped on stdin. The `<id>` is the provenance identifier (URL, logical name, or path that doesn't exist on disk). For URLs and binary documents that need conversion: `markitdown <url> | memex ingest --source <url>`.

**Argument rules:**
- `--source <id>` accepts any string; validated for shape (§3.2) and stored verbatim as provenance. Stdin is the content itself — pre-cleaned Markdown or text bytes, not a JSON wrapper and not a path.
- `--source` conflicts with `<positional>` and with `--agent`.
- `--agent <a>` requires the positional `<path>`.
- Empty stdin in stdin mode is an **error**, not a silent success (§3.3).

**Hook shims always pass `--agent` explicitly** — they know which agent fired them, so they don't rely on inference. Inference is for ad-hoc human use.

There is no `memex fetch`, no `memex ingest --url`, no memex-side fetching. The three modes cleanly separate "memex reads a transcript or document file" (positional) and "user pipes content with explicit provenance" (stdin).

### 1.2 Hook wiring

Hook payloads (JSON written to a hook subprocess's stdin by Claude Code / Codex / Gemini CLI) are extracted by a **per-agent shim** bundled with the plugin, not by memex itself. The shim reads JSON from stdin, pulls the agent-specific transcript-path field, and invokes `memex ingest --agent <agent> <transcript-path>`. Memex stays out of the hook protocol business.

Three shims live under `plugin/hooks/` in the npm package and ship with the install:

```
plugin/hooks/claude-code-session-end.js
plugin/hooks/codex-stop.js
plugin/hooks/gemini-cli-session-end.js
```

Each is a tiny Node.js script (Node is already present — the plugin is an npm package). Example for Claude Code:

```javascript
#!/usr/bin/env node
// plugin/hooks/claude-code-session-end.js
const fs = require('fs');
const { spawnSync } = require('child_process');

if (process.env.MEMEX_INTERNAL === '1') process.exit(0);

let payload;
try {
  payload = JSON.parse(fs.readFileSync(0, 'utf8'));
} catch (e) {
  console.error(`memex hook: failed to parse stdin JSON: ${e.message}`);
  process.exit(1);
}

const transcriptPath = payload.transcript_path;
if (typeof transcriptPath !== 'string' || !transcriptPath) {
  console.error('memex hook: missing transcript_path in payload');
  process.exit(1);
}

const r = spawnSync('memex',
  ['ingest', '--agent', 'claude-code', transcriptPath],
  { stdio: 'inherit' });
process.exit(r.status ?? 1);
```

Codex reads `.transcript_path` from its `Stop` payload; Gemini CLI reads from its session-end payload. Each shim knows its agent's field shape.

The generated `hooks.json` points at the shim:

```json
{
  "hooks": [{
    "type": "command",
    "command": "node /path/to/plugin/hooks/claude-code-session-end.js",
    "timeout": 10
  }]
}
```

Plugin `postinstall.js` writes the absolute path to the shim at install time. A future `claude-code` hook format change affects only the shim, not the memex binary.

**Consequences of the shim model:**

- `memex ingest --agent X <transcript-path>` works as a plain CLI command. A user backfilling a single transcript runs it directly without constructing a JSON payload.
- Memex CLI has zero knowledge of agent hook JSON schemas.
- Shim startup cost is one Node process per hook fire (~50–100 ms). Hooks are asynchronous; this doesn't affect session-end latency.
- The `MEMEX_INTERNAL=1` guard stays in both the shim (exit fast) and the memex binary (belt-and-suspenders, in case someone invokes `memex ingest --agent` manually with that env set).

### 1.3 Source storage and page writes — clean split

Sources and pages are distinct resources; their commands are split accordingly. `--source` on `memex write` means exactly one thing: a docid of an already-stored source. The legacy `memex write --source <fspath>` mode (read file, store inline, attach) is removed — users chain `memex source add | memex write` when they want that behavior.

The `memex source` noun group is implemented in full so users typing `memex source list`, `memex source show`, or `memex source delete` get sensible behavior, not "command not found":

```
# Store a source document. Prints the allocated docid to stdout.
memex source add <source-path> [--collection <name>] [--verbose]   # stdin = content

# List source documents.
memex source list [--collection <name>] [--json]

# Print the content of a source document by docid or path.
memex source show <ref>                                            # ref = docid or path:<source-path>

# Delete a source document.
memex source delete <ref> [--force]                                # warns if wiki pages reference it

# Write a wiki page, optionally referencing an already-stored source.
memex write <slug> [--source <docid>] [--force] [--quiet]
```

**`memex source add <source-path>`** takes the provenance identifier as its positional argument. Any string: URL, filesystem path, arbitrary label. Reads content from stdin (size-capped per §3.1, UTF-8 validated, control-char sanitized). Stores in the content-addressed content table, upserts a `source` doc_type row with `path = <source-path>`, embeds. Prints the allocated docid on stdout; errors to stderr. Idempotent — repeated identical content returns the same docid without duplicate storage work.

**`memex source list`** prints a table of source documents: `docid | path | title | size | collections | created_at`. Filterable by `--collection <name>`. With `--json`, emits one JSON object per row for scripting. Read-only — bypasses the daemon and queries SQLite directly (mirrors `memex search` and `memex read`).

**`memex source show <ref>`** prints a source document's content to stdout. `<ref>` is either a docid (e.g., `src-abc123`) or `path:<source-path>` to look up by the original identifier. Read-only — bypasses the daemon. The `path:` prefix is required when looking up by source path so the parser can disambiguate an http URL like `path:https://...` from a docid.

**`memex source delete <ref> [--force]`** removes a source document and its content. Daemon mutation (single-writer). Before deletion, the daemon scans wiki pages for frontmatter `sources:` entries that reference this source's path; if any are found, the user sees `N wiki pages reference this source: <slug1>, <slug2>, ...` and must pass `--force` to proceed. The wiki pages themselves are not modified — their `sources:` entries become dangling, which `memex lint` reports.

**`memex write --source <docid>`** attaches the referenced source to the page. Rejects values that don't look like a docid (not a filesystem path, not free text). If the user wants to attach a local file, they chain commands explicitly:

```bash
docid=$(memex source add /path/to/file.md < /path/to/file.md)
printf '%s' "$body" | memex write my-page --source "$docid"
```

One extra line for full explicitness — no flag with two meanings.

Typical skill usage (URL case):

```bash
content=$(markitdown "$url") || { echo "converter failed"; exit 1; }
[ -n "$content" ] || { echo "empty content from converter"; exit 1; }
src=$(printf '%s' "$content" | memex source add "$url")

# ...propose/approve...

for page in approved_pages; do
    printf '%s' "$body" | memex write "$slug" --source "$src" --quiet
done
```

One source storage call, N page writes, all referring to the same docid. The skill never juggles filesystem temp files; it never passes a URL as a `--source` argument to `memex write`; it never pipes content to `memex ingest`.

### 1.4 Skill

The skill is the place to be opinionated about which converter fits which page. Memex CLI stays format-agnostic; the skill picks the right tool based on the kind of source.

`plugin/skills/memex-ingest/SKILL.md` gets a new "Source acquisition" section:

> ## Source acquisition
>
> Pick the right tool for the source:
>
> | Source | Tool | Why |
> |--------|------|-----|
> | URL — static page (blog, docs, news, README on a public site) | `markitdown <url>` | Fast (one HTTP request), handles HTML/PDF/DOCX/audio/YouTube. |
> | URL — JS-rendered SPA, dashboard, or content that requires client-side rendering | `agent-browser` then `markitdown` | Static fetch returns a skeleton; need a real browser to render. |
> | URL — authenticated / paywalled | `agent-browser` (after `setup-browser-cookies`) then `markitdown` | Cookies needed; static fetch fails or returns login page. |
> | Local `.pdf` / `.docx` / `.pptx` / `.xlsx` / `.html` / audio | `markitdown <path>` | One tool covers all these formats. |
> | Local `.md` / `.txt` / `.rst` / `.org` | the `Read` tool directly | No conversion needed. |
>
> **Install once:** `uv tool install 'markitdown[all]'` (the `[all]` extras enable PDF, DOCX, PPTX, XLSX, audio/transcription, YouTube, etc.). `uv` installs markitdown in an isolated environment with its bin on PATH, avoids PEP 668 issues on Debian/Ubuntu, and is much faster than pipx. Get `uv` from https://github.com/astral-sh/uv if you don't already have it. For `agent-browser`, install per its own docs. Memex does not fetch or convert; these tools do.
>
> **Decision flow for URLs:**
>
> 1. Run `markitdown "$arg"` first — it's fast and covers most pages.
> 2. Inspect the output. If it's the article body (substantial text, headings, paragraphs), use it. If it's empty, mostly navigation/footer chrome, contains "Enable JavaScript" prompts, or otherwise looks like a skeleton, the page is JS-rendered or auth-gated.
> 3. Fall back to `agent-browser` (verified syntax — these are the actual commands, not pseudo-shapes):
>    ```
>    agent-browser open "$arg"
>    agent-browser wait 2000                           # let JS render
>    content=$(agent-browser get html body | markitdown -x html)
>    final_url=$(agent-browser get url)
>    title=$(agent-browser get title)
>    ```
>    `agent-browser` is optional. If it's not on PATH (`command -v agent-browser` fails), tell the user the page appears JS-rendered and recommend installing it (or pasting the rendered content manually).
>
> 4. **Detect login redirects.** A successful render can still land on a login page if the site requires auth. Inspect `final_url` and `title` / `content`:
>    - URL changed to a known SSO host (`id.atlassian.com`, `accounts.google.com`, `login.microsoftonline.com`, `github.com/login`, etc.) — login redirect.
>    - Title or content prominently contains "Log in", "Sign in", "Log in to continue" — login wall.
>
>    On a detected login wall, **stop and tell the user**:
>    > "The page is behind authentication. Import your browser's session cookies with `/setup-browser-cookies` (pick the relevant domain), then re-run `/memex-ingest <url>`."
>
>    Do not write a login-page-as-content into memex.
> 4. **Always use `set -o pipefail` or explicit exit checks** so converter failures propagate. If both tools fail or the result is empty, stop and report what failed — do not proceed with empty content.
>
> **Once content is acquired, the flow is uniform.** Store the source once, then write pages referencing the docid:
>
> ```
> src=$(printf '%s' "$content" | memex source add "$arg")
>
> # ...propose/approve loop...
>
> for page in approved_pages; do
>     printf '%s' "$body" | memex write "$slug" --source "$src" --quiet
> done
> ```
>
> Do **not** pipe the acquired content to `memex ingest --source` — that runs the daemon's non-interactive pipeline and skips the propose/approve step the skill exists for.

No other skill changes. Subject identification, dedup search, propose/approve, and per-page writes are unchanged.

### 1.5 Two ingestion modes, one toolkit

| Mode | Entrypoint | User involvement | LLM pipeline |
|------|-----------|------------------|--------------|
| Automated | `markitdown <url> \| memex ingest --source <url>` | None after shell command | Daemon Extract+Merge with chunking (non-interactive) |
| Interactive | `/memex-ingest <url>` inside an agent session | Propose → approve → write per page | Direct writes via `memex write --source <docid>` (no Extract/Merge) |

The two modes never collide: the skill stores the source via `memex source add` and writes pages via `memex write --source <docid>`; it never calls `memex ingest --source`.

## 2. Protocol change

### 2.1 `IngestSource` enum

`Request::Ingest` is generalized to carry a typed source. The IPC protocol has no version field on any request variant — memex's CLI and daemon ship from the same binary, plugin `postinstall.js` runs `memex daemon stop` on upgrade so the daemon relaunches, and there are no external IPC clients to version against. Protocol evolution happens by changing the wire shape directly; mismatched binaries fail with a parse error, not a version-negotiated error.

```rust
enum IngestSource {
    Transcript {
        path: String,
        agent: TranscriptAgent,   // ClaudeCode | Codex | GeminiCli
    },
    Document {
        source_path: String,
        content: String,
    },
}

enum Request {
    // ...
    Ingest {
        source: IngestSource,
        collections: Vec<String>,
        memex_root: String,
    },
    // ...
}
```

### 2.2 New helper commands in protocol

```rust
enum Request {
    // ...existing + Ingest (updated above)...
    SourceAdd {
        source_path: String,
        content: String,
        collections: Vec<String>,
        memex_root: String,
    },
    SourceDelete {
        // ref_ is either "src-..." (docid) or "path:<source-path>".
        // Read-only verbs (list, show) bypass the daemon and query
        // SQLite directly; only mutations live in the protocol.
        ref_: String,
        force: bool,
        memex_root: String,
    },
    Write {
        title: String,
        content: String,
        tags: Vec<String>,
        source: Option<String>,          // docid; None = no source attached
        force: bool,
        memex_root: String,
    },
    // ...
}

// Response:
Event::SourceAdded { docid: String }
Event::SourceDeleted {
    docid: String,
    source_path: String,
    dangling_wiki_pages: Vec<String>,    // pages whose `sources:` frontmatter now points nowhere
}
```

`Write`'s `source` field takes a docid or `None`. The legacy `sources: Vec<String>` field (filesystem paths) is removed; the daemon handler's source-from-filesystem codepath is deleted. Users chain `memex source add | memex write` instead.

`SourceDelete` requires `force=true` if any wiki pages reference the source's path via their frontmatter `sources:` field. Without `force`, the daemon returns `bad_request` listing the referencing pages so the user can decide whether to remove the references first or force-delete and accept the dangling links. Read-only verbs (`memex source list`, `memex source show`) bypass the daemon entirely.

## 3. Daemon pipeline for document ingestion

### 3.1 Payload size cap

Default **5 MB** (`fetch_max_bytes`, tunable via `~/.memex/config.toml`). This reflects the real size range of documents that fit through a single Extract call comfortably (blog post ≈ 50KB, long-form report ≈ 500KB, book chapter ≈ 2MB). 5 MB covers 99% of real inputs and keeps the IPC transport (newline-delimited JSON over Unix socket) stable at its current 5s read timeout.

Enforced at three layers:

- **Client** streams stdin with `.take(fetch_max_bytes + 1)`. If the extra byte is read, fails with `source_too_large` before sending to the daemon. No "read everything, check, maybe OOM" path.
- **Daemon handler** re-validates `content.len() <= fetch_max_bytes` on IPC receipt.
- **Server read timeout** is raised from 5s → 30s for the `Ingest` and `SourceAdd` request types (larger payloads than queries). Other request types keep the current 5s.

Documents larger than 5 MB are rejected with a clear error pointing at the config option. If you're ingesting books, bump the cap and accept the transport cost.

### 3.2 Input sanitization

| Check | Rule | Failure |
|-------|------|---------|
| `source_path` length | ≤ 2048 chars | `invalid_source_path` |
| `source_path` control chars | reject `\0`, `\n`, `\r`, any ASCII < 0x20 except `\t` | `invalid_source_path` |
| `content` encoding | must be valid UTF-8 | `invalid_encoding` |
| `content` size | ≤ `fetch_max_bytes` | `source_too_large` |
| `content` emptiness | non-empty after trim | **`empty_source` — error, not Done** |

Empty-content-as-error is load-bearing: the most common upstream failure is a converter that exited non-zero but produced empty stdout (404, auth, JS-page). Silently succeeding hides the real problem. The CLI also sets a non-zero exit code so `set -o pipefail` propagates the failure.

### 3.3 Secret redaction

Inbound content flows through the existing transcript-cleaner secret-redaction pass (regex for `sk-...`, `AKIA...`, `password/token/secret = ...`). Same implementation; reused verbatim.

### 3.4 Content-hash dedup

SHA-256 over the redacted (post-§3.3) content. If a row with this hash exists in the `source` doc_type, respond `Done` and skip. Consequences:

- Re-ingesting identical content: no-op, no LLM cost.
- Same content reached via two different `--source` identifiers: second is deduped; first wins provenance. **Known limitation**, documented.
- Source content changed: new hash ⇒ new run, produces updated wiki pages via the standard wiki-side dedup+merge flow.
- **Truncation + dedup interaction** (fixed by chunking in §3.7): because chunking processes the whole document, there is no "tail dropped silently on first ingest, permanently excluded by dedup on retry" pathology. The redacted content hashed is always the complete content.

### 3.5 Durable job queue — schema change

The `ingest_jobs` schema is rewritten for clean semantics. SQLite doesn't support `ALTER COLUMN RENAME` in older versions reliably, so the migration is drop-and-recreate:

```sql
DROP TABLE IF EXISTS ingest_jobs;
CREATE TABLE ingest_jobs (
    job_id        TEXT PRIMARY KEY,
    job_type      TEXT NOT NULL CHECK (job_type IN ('transcript', 'document')),
    source_path   TEXT NOT NULL,          -- file path for transcripts, identifier for documents
    agent         TEXT,                   -- 'claude-code' | 'codex' | 'gemini-cli' for transcripts; NULL for documents
    content_hash  TEXT NOT NULL,
    memex_root    TEXT NOT NULL,
    collections   TEXT NOT NULL DEFAULT '[]',  -- JSON array
    status        TEXT NOT NULL DEFAULT 'pending',
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    error         TEXT
);
```

Pre-existing rows are **discarded**. Justification: rows in `ingest_jobs` are transient — they represent in-flight or recently-completed ingest work. Dropping them forces re-ingestion of any not-yet-completed jobs, which is the correct behavior after a schema break anyway (old rows' semantics differ from new).

`pending_ingest_jobs()` returns:

```rust
struct PendingJob {
    job_id: String,
    job_type: JobType,                  // Transcript | Document
    source_path: String,
    agent: Option<TranscriptAgent>,     // Some for transcript jobs, None for document
    memex_root: String,
    content_hash: String,
    collections: Vec<String>,
}
```

Recovery dispatches on `job_type`:

- **transcript** — re-read content from `source_path` on disk, same as today.
- **document** — look up content by `content_hash` from the content-addressed store (see §3.12 for the invariant that makes this safe).

### 3.6 Job identity

For documents, `job_id = sha256(source_path || '\0' || content_hash)[0..16]`. This:

- Makes `INSERT OR IGNORE` correctly dedup identical (source, content) pairs.
- Treats a changed-content retry as a **new** job — no silent-drop pathology from the path-only job_id used today.

For transcripts, the existing path-hash-only derivation is kept (a transcript file is append-only during the session, and a content change is effectively a new session file).

### 3.7 Chunking

Documents whose estimated token count exceeds a single-call budget are split into section-aware chunks. Each chunk is Extracted independently; cross-chunk same-slug pages are merged (§3.10).

**Token estimation.** `estimated_tokens = content.chars().count() / 3` (conservative for English Markdown; no tokenizer dependency).

**Parameters.**

| Param | Default | Meaning |
|-------|---------|---------|
| `chunk_target_tokens` | 30_000 | Trigger chunking when content exceeds this, and aim to size chunks near this |
| `chunk_hard_cap_tokens` | 50_000 | Absolute per-chunk maximum; force split even mid-section |
| `max_chunks` | 20 | Safety cap; exceeding this fails with `document_too_large_for_chunking` |

At defaults, a 600K-char (≈200K-token) book chapter produces ~7 chunks.

**Algorithm.**

```
chunk(content, target, hard_cap):
  1. If estimated_tokens(content) ≤ target: return [content]
  2. Split content into "segments" by walking top-level Markdown headings
     (lines matching `^(#|##) `). Each heading starts a new segment.
     If no headings: fall back to paragraph splits (\n\n+).
     If no paragraphs: fall back to line splits.
  3. Greedy pack: iterate segments, accumulate into current chunk.
     - If adding the next segment would exceed target AND current chunk
       is non-empty: flush current, start fresh.
     - If a single segment exceeds hard_cap: split it further at ¶ or line
       boundaries (recursive), never mid-code-fence (detect unclosed
       triple-backtick regions and back up).
     - After the walk, flush any remainder.
  4. If chunks.len() > max_chunks: fail with document_too_large_for_chunking.
```

**Code-fence safety.** A chunk boundary that falls inside a ``` ... ``` block is shifted backward to the most recent `\n\n` before the opening fence. Prevents broken syntax confusing Extract.

**Chunk context preservation.** Each chunk carries `(chunk_index, total_chunks, source_path)` metadata. The Extract prompt uses this to tell the LLM it's seeing a fragment of a larger document (§3.9).

### 3.8 Worker dispatch

`IngestJob` is refactored to carry a unified segment envelope used by both transcript and document Extract calls. No new `BackendJob` variant is added.

```rust
/// One unit of LLM-visible content. Optional fields convey the asymmetry
/// between transcripts (per-turn role + timestamp) and documents (single
/// segment, no role).
struct ExtractSegment {
    index: Option<usize>,        // 1-based ordering hint; array position is canonical
    role: Option<String>,         // speaker identity for transcripts
    timestamp: Option<String>,    // ISO 8601 when known
    text: String,
}

struct ChunkPosition {
    index: usize,                 // 0-based
    total: usize,                 // total chunks in this document
}

struct IngestJob {
    segments: Vec<ExtractSegment>,
    source: String,               // URL/path provenance, always populated
    chunk: Option<ChunkPosition>, // None for transcripts and single-chunk docs
    reply: oneshot::Sender<IngestResult>,
}
```

Wrapping `index` and `total` in `ChunkPosition` makes the "set together" invariant type-level — there's no way to construct a chunk that knows its index but not the total.

Transcripts populate one segment per `TranscriptTurn` (role + timestamp). Single-chunk documents populate one segment with just `text`, plus `source`. Multi-chunk documents dispatch one `IngestJob` per chunk with `chunk: Some(ChunkPosition { index, total })`.

Chunks fan out to the worker pool concurrently using the existing `BackendJob::Ingest` variant. Existing dispatch, timeout, and retry policy apply.

**Timeout recalibration.** The existing `timeout_sec` default (300s) is tuned for transcripts ≤ 50K tokens. For document chunks at `chunk_target_tokens = 30K` the same 300s is fine. Keep default; no new config key needed.

### 3.9 Unified Extract prompt — segments envelope

A single prompt builder `build_extract_prompt(&IngestJob)` emits one envelope shape for both transcript and document Extract calls:

```json
// transcript example
{
  "segments": [
    {"index": 1, "role": "user", "timestamp": "2026-04-25T12:00:00Z", "text": "..."},
    {"index": 2, "role": "assistant", "timestamp": "2026-04-25T12:00:01Z", "text": "..."}
  ],
  "source": "/home/user/.claude/projects/.../session.jsonl"
}

// document chunk example
{
  "segments": [{"text": "<chunk markdown>"}],
  "source": "https://example.com/doc",
  "chunk_index": 1,
  "total_chunks": 4
}
```

The `[TASK: EXTRACT]` section in `cli/src/daemon/worker/prompt.txt` is rewritten to describe this envelope and branch on field presence:

- **Mode A — transcript** (segments carry `role`): existing transcript-extraction rules apply unchanged — turn-by-turn (segment-by-segment) scanning, mandatory `## Timeline` H2 for any subject with dated events, "one page per speaker", relative-date resolution against per-segment `timestamp`.
- **Mode B — document** (segments lack `role`): use Markdown H1/H2 as section/page-boundary hints; `## Timeline` becomes opt-in (only when the document IS event-narrative — changelogs, news, minutes, retrospectives — and has dated events); the "one page per speaker" rule does not apply.
- **Common rules** (both modes): subject-anchored slugs (forbidden patterns: multi-subject, date-bearing, episode words), preserve specific facts, output strict JSON `{"pages":[...]}`.

When `total_chunks > 1`, the prompt instructs the model to extract only subjects fully or substantially covered within this chunk; cross-chunk merging happens after all chunks return (§3.10).

Output per chunk is capped per the existing `handler.rs:611` limits (10 pages, 20K chars/page). For a chunked document with N chunks, the total-page budget grows to N × 10. Known MVP limitation: if a single chunk contains > 10 distinct subjects, the tail is dropped for that chunk only.

### 3.10 Cross-chunk page merge

After all N chunks return, the handler flattens results to `Vec<ExtractedPage>` and groups by slug.

- **Unique slug** — used as-is; fed to the wiki-side dedup search (§3.11).
- **Slug appears in ≥ 2 chunks** — these are fragments of the same subject from different parts of the doc. Run a **fragment-merge** pass:

  ```
  BackendJob::Merge(MergeJob { existing: fragments[0], proposed: fragments[1] })
    → merged_01
  BackendJob::Merge(MergeJob { existing: merged_01, proposed: fragments[2] })
    → merged_012
  ...
  ```

  Chained Merge calls. Reuses the existing `MergeJob` pipeline with a minor prompt variant acknowledging "these are fragments of one document, not independently authored pages." N-1 Merge calls for N fragments. Typically rare (a well-structured doc has subjects in contiguous sections, one chunk).

After cross-chunk merge, the flat deduplicated `Vec<ExtractedPage>` is handed to the existing wiki-dedup + Merge pipeline (§3.11).

### 3.11 Post-Extract: identical to transcript path

Steps 3.6–3.9 of `2026-04-19-daemon-ingestion-design.md` (wiki dedup search, optional wiki-side Merge, validate output, transactional store + embedding) apply verbatim. The `source` doc_type row records `source_path` in the `path` column. Source content is read from the content-addressed store (`get_content(&hash)`) — not the filesystem — so non-filesystem identifiers (URLs, logical names) work transparently.

Title resolution for the source row:

1. If content starts with `# Heading`, use that as the source title.
2. Else derive from `source_path`: URL → last non-empty path segment, slug-normalized; file path → file stem; other → identifier verbatim.

### 3.12 Transaction ordering (crash safety)

```
BEGIN TRANSACTION                                      ── pre-ack ──
  1. insert_content() for the redacted document content
  2. insert_ingest_jobs row (job_type='document', content_hash bound)
COMMIT
  [ACK to client with Queued]

  [chunking runs in handler; no DB state]
  [N IngestJob jobs dispatched to workers (one per chunk)]
  [all chunks return]
  [cross-chunk merge (may involve MergeJob workers)]
  [wiki-side dedup search]
  [wiki-side MergeJobs]

BEGIN TRANSACTION                                      ── post-extract ──
  3. upsert_document() for source in source doc_type
  4. For each extracted wiki page:
     a. insert_content()
     b. upsert_document()
     c. insert chunks + embeddings
  5. Update backlinks
COMMIT
  [filesystem writes for wiki pages, as before]
```

For transcript jobs, step 1 is replaced by "read transcript from `source_path` on disk." For document jobs, step 1 establishes the invariant: **if an `ingest_jobs` row exists for a document job, its referenced `content_hash` is present in the content table.** On crash recovery, the daemon looks up content by hash and resumes from the chunking step.

### 3.13 Agent detection (CLI-side inference)

When the user runs `memex ingest <path>` without `--agent`, the CLI inspects the file to decide between transcript mode (and which agent's parser to use) and document file mode. Detection runs at CLI parse time, before sending any IPC request to the daemon. The daemon never performs inference — it always receives a fully-typed `IngestSource::{Transcript, Document}` request.

**Detection function** lives in `core/src/transcript.rs` next to the existing per-agent parsers, with this signature:

```rust
pub fn detect_transcript_agent(path: &Path) -> Option<TranscriptAgent>;
```

Returns `Some(agent)` if the file matches a known transcript signature; `None` if it doesn't.

**Algorithm:**

| Match | Returns |
|-------|---------|
| Extension `.json` AND content is one JSON object with `"messages"` array | `Some(GeminiCli)` |
| Extension `.jsonl` AND first non-empty line is JSON with `"type"` and (`"uuid"` or `"parentUuid"`) | `Some(ClaudeCode)` |
| Extension `.jsonl` AND first non-empty line is JSON with `"event_msg"` or `"response_id"` | `Some(Codex)` |
| Anything else (other extensions, malformed JSON, no matching keys) | `None` |

**CLI dispatch logic:**

```
if --agent is set:
    transcript mode with the explicit agent
elif positional <path> is set:
    if detect_transcript_agent(path) is Some(agent):
        transcript mode with detected agent
    else:
        document file mode (read the file as text)
elif --source <id> is set:
    stdin mode
else:
    error: specify <path> or --source
```

**Failure modes and recovery:**

- **Wrong detection** (file matches one signature but is actually for another agent): the parse step inside the daemon will fail with a clear error citing the agent name. User retries with explicit `--agent`. Recoverable.
- **Format change** (e.g., Codex schema rev that no longer has `event_msg`): inference returns `None`, file gets ingested as a document — user notices "this looks like raw JSONL, not extracted knowledge." User retries with explicit `--agent` once they realize. Logged as a known limitation; tested by including a few format-change-resistant signature keys, not just one.
- **`.jsonl` extension on a non-transcript file** (e.g., a JSONL backup of database rows): first line won't match any signature → `None` → document mode. Memex tries to extract knowledge from raw JSONL — output may be poor quality but doesn't crash. Acceptable — exotic case.

**Hook shims do not use inference.** They pass `--agent` explicitly because the hook event tells them which agent fired (`SessionEnd` from Claude Code, `Stop` from Codex, etc.). Inference is for human ad-hoc use only.

## 4. Configuration

New `[ingest]` section in `~/.memex/config.toml`:

```toml
[ingest]
fetch_max_bytes = 5242880          # 5 MB payload cap for document content
chunk_target_tokens = 30000        # target chunk size (triggers chunking above this)
chunk_hard_cap_tokens = 50000      # force-split threshold
max_chunks = 20                    # safety cap per ingest
```

All optional; defaults apply when absent.

## 5. Error handling

| Condition | Error code | Layer | Status |
|-----------|-----------|-------|--------|
| `--source` missing | clap error | CLI | exit 2 |
| `source_path` > 2048 chars or has control chars | `invalid_source_path` | CLI + daemon | exit 1 |
| stdin > `fetch_max_bytes` | `source_too_large` | CLI (streamed) + daemon | exit 1 |
| stdin not valid UTF-8 | `invalid_encoding` | CLI + daemon | exit 1 |
| stdin empty (after trim) | **`empty_source`** | CLI + daemon | exit 1 |
| content_hash already stored | (not an error) `Done` | Daemon | exit 0 |
| chunking overflows `max_chunks` | `document_too_large_for_chunking` | Daemon | exit 1 |
| IPC payload read timeout | `payload_timeout` | Daemon | exit 1 |
| Request shape invalid / unknown op | `bad_request` (serde parse error) | Daemon | exit 1 |
| Extract subprocess crash/timeout/auth | existing codes | Worker | exit 1 |

All terminal failures update the `ingest_jobs` row to `status='failed'` with the error message.

## 6. Skill changes

Covered in §1.3. Summary: new "Source acquisition" section in `plugin/skills/memex-ingest/SKILL.md` instructing agents to:

1. Run the converter with `set -o pipefail` (or explicit exit check); abort on non-zero exit or empty output.
2. Call `memex source add <arg>` once (with content on stdin) to store the source and get a docid.
3. Continue the existing propose/approve flow, using `memex write <slug> --source <docid>` per approved page.

No other skill changes.

## 7. Testing

**Unit tests.**

Protocol:
- `IngestSource::Document` serialize/deserialize round-trip.
- `Ingest` deserializes cleanly for both `IngestSource::Transcript` and `IngestSource::Document` variants.
- Malformed `Ingest` requests (missing fields, unknown variants, wrong types) fail with a clear `bad_request` error carrying the serde parse message.

Validation:
- `source_path` length + control-char rejection.
- Payload size cap enforced at client (stream cap) and daemon.
- Empty content → `empty_source` error, not `Done`.
- Non-UTF-8 content → `invalid_encoding`.

Chunking:
- Short content → single chunk, no chunking overhead.
- Long content with headings → section-aware splits at H1/H2 boundaries.
- Long content without headings → paragraph splits.
- Chunk boundary inside a code fence → boundary shifted before the fence.
- `max_chunks` exceeded → `document_too_large_for_chunking`.
- Token-estimation function sanity: known strings produce expected estimates.

Cross-chunk merge:
- Unique slugs across chunks → pass through unchanged.
- Same slug in 2 chunks → single MergeJob runs.
- Same slug in 4 chunks → N-1 chained MergeJobs run.

Job identity:
- Same `(source_path, content_hash)` → same job_id, `INSERT OR IGNORE` dedups.
- Same `source_path`, different `content_hash` → different job_id, runs.

Recovery:
- Document job row exists ⇒ `content_hash` present in content table (invariant test).
- Startup with a mix of pending transcript + document jobs → both requeue correctly and dispatch by `job_type`.

Source storage + write path:
- `memex source add <path>` stores source, returns docid on stdout.
- Re-adding identical content returns same docid, no duplicate storage.
- `memex write --source <docid>` attaches source without filesystem resolution.
- `memex write --source <value-that-is-not-a-docid>` → rejected with clear error.
- `memex write` without `--source` writes a page with no source attached (no regression from today).

**Integration tests (no network, no converters).**

- Fixture Markdown (≈5K chars) → one chunk → Extract dispatched → wiki pages stored.
- Fixture Markdown (≈300K chars) → multiple chunks → chunks extracted → cross-chunk merge if needed → wiki pages stored.
- Fixture with intentional slug collision across chunks → fragment-merge runs → one unified page stored.
- Empty content stdin → `empty_source` error, no DB writes.
- Oversize content → `source_too_large` at client and re-enforced at daemon.
- Restart daemon mid-ingest (simulated) → document job replayed from content hash, completes.

**Regression tests (mandatory).**

- `memex ingest --agent claude-code <fixture.jsonl>` end-to-end: transcript parsed, pipeline runs, wiki pages stored. (Replaces the old hook-stdin-JSON test; that contract is gone.)
- `memex ingest <fixture.md>` end-to-end (document file mode): memex reads the file, runs the document pipeline, wiki pages stored.
- Hook shim smoke test: feed a fixture JSON payload to `node plugin/hooks/claude-code-session-end.js`, assert it invokes `memex` with the transcript path as the positional argument. One shim test per agent.

**Prompt eval.**

A small eval harness with 3–5 representative documents (blog post, API doc, design doc, academic abstract, README). Runs Extract with the document prompt variant, asserts output is valid YAML and produces ≥ 1 sensible page per fixture. Not a quality bar (no judging), but a smoke test against prompt regressions.

**Documentation smoke tests** (not CI, run manually):

- `markitdown <url> | memex ingest --source <url>`
- `pandoc -t markdown paper.docx | memex ingest --source paper.docx`
- `/memex-ingest <url>` end-to-end in a Claude Code session.

## 8. Future extension path

Because memex does not own fetching, new source kinds are zero-memex-code additions: users pipe any converter's Markdown output in.

Memex-side follow-ups worth considering, deferred from this spec:

- **Canonical URL normalization** before dedup (strip `utm_*` tracking params). Lossy in some cases; warrants a separate decision.
- **Per-source-kind prompt tuning** — if a future eval shows per-kind prompt variants improve extraction quality, reintroduce a `source_kind` enum threaded through the protocol, storage, and prompt builder. The current spec explicitly rejects this as speculative.
- **Streaming IPC transport** for payloads > 5 MB. Only needed if book-length ingestion becomes common.
- **`memex read <url-like-ref>`** — today URL-provenance sources are readable only by docid; adding URL-shape ref resolution is a small lookup helper. (`memex source show path:<url>` works today; the gap is making the bare `memex read <url>` form Just Work for source rows.)

## 9. Data model impact

**`ingest_jobs` is dropped-and-recreated** with semantic column names (`source_path`, `job_type`, `agent`, `content_hash`, `collections`). Pre-existing rows are discarded — acceptable because `ingest_jobs` rows are transient (in-flight or recently-completed ingest work). Any in-flight jobs from before the upgrade must be re-triggered.

No other schema changes. `documents`, `content`, and embedding tables are untouched.

## 10. Rollout

Single PR. No feature flag. No install automation for converters — users install them.

README update under Prerequisites:

```
- Optional: any HTML→Markdown, PDF→Markdown, or document-to-Markdown converter,
  for ingesting non-text sources. `markitdown` is a convenient one-stop choice
  (https://github.com/microsoft/markitdown). Install with
  `uv tool install 'markitdown[all]'` — the `[all]` extras pull in support
  for PDF, DOCX, PPTX, XLSX, audio (with transcription), and YouTube. `uv`
  installs the tool in an isolated environment, places its bin on PATH,
  avoids PEP 668 issues on Debian/Ubuntu, and is much faster than pipx.
  Get `uv` from https://github.com/astral-sh/uv. Memex itself does not
  fetch or convert; it expects pre-cleaned Markdown on stdin.
```

**Upgrade procedure** — plugin `postinstall.js` runs `memex daemon stop` so the daemon relaunches on the new binary with the new request shape, and regenerates `hooks.json` to point at the per-agent Node.js shims under `plugin/hooks/`. Users installing via `npm install` get this transparently. Users who manually cargo-built memex need to run `memex daemon stop` themselves and re-wire hooks by hand.

**No backward compatibility guarantees.** Pre-1.0 project. The IPC protocol has no version field; binaries that speak different request shapes fail with parse errors at the wire. Old-style hooks that invoke `memex ingest --agent <agent>` without a transcript path fail with a clear "ingest --agent requires a transcript path positional argument" error — loudly, not silently. Existing `ingest_jobs` rows are discarded during the schema migration; in-flight jobs from before the upgrade are lost and must be re-triggered.

## 11. Known MVP limitations

Explicit list, surfaced in the docs:

1. **Same content via two identifiers → second dedupes.** First identifier wins provenance. Acceptable for personal wiki; future work for audit trails.
2. **Per-chunk page cap (10 pages × 20K chars) still applies.** A chunk with > 10 distinct subjects loses the tail. Unlikely in practice because chunking follows section boundaries; a 30K-token section rarely has 10+ completely distinct subjects.
3. **IPC payload cap is 5 MB** — ingesting a book requires bumping `fetch_max_bytes` and tolerating the larger serialize/deserialize cost on the IPC transport.
4. **URL-provenance sources are not resolvable by URL ref.** Use docid. Fix is a small follow-up.
5. **Chunk quality is prompt-bounded.** A badly written document (no headings, no structure) falls back to paragraph/line splits; Extract still works but may produce more cross-chunk slug collisions. The fragment-merge machinery (§3.10) handles collisions but uses LLM calls.
