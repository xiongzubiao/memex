# `/memex-ingest` Skill Redesign — Daemon-Pipelined with Interactive Review

**Status:** Ready for implementation
**Date:** 2026-04-30
**Extends:** `2026-04-24-web-page-ingestion-design.md` (replaces the skill section's manual `source add` + `write` flow)
**Affects:** `cli/src/main.rs`, `cli/src/daemon/handler/ingest.rs`, `cli/src/daemon/protocol.rs`, `~/.claude/local-marketplaces/memex-dev/plugin/skills/memex-ingest/SKILL.md`

## Context and motivation

The current `/memex-ingest` skill (per `2026-04-24-web-page-ingestion-design.md`) prescribes a manual flow: `markitdown <url> | memex source add` → agent proposes pages from raw content → `memex write` per page. This **bypasses the daemon's EXTRACT/MERGE pipeline** entirely.

The autonomous `memex ingest` command (`--agent <a>` or `--source <id>` modes) routes through the daemon worker, which runs the carefully-tuned EXTRACT/MERGE prompts in `cli/src/daemon/worker/prompt.txt`. Those prompts encode subject extraction rules (forbidden slug patterns at lines 268–276, Mode A vs Mode B branching at 203–256, Timeline preservation rules at 280–298) that the agent re-implements ad-hoc when it bypasses the worker.

Observed failure: a session ingesting 65 Confluence pages via the manual skill flow produced output violating the prompt's rules (date-bearing slugs `milestone-3-tasks`, `milestone-v0-3-1`; episode-bearing slugs `mmai-design-milestone-3-and-beyond`; redundant single-subject pairs that should have consolidated as `mmai`, `gpu-checkpoint`, `kubernetes-nvidia-gpu-aws`). The agent defaulted to title-derived slugs because there's no extraction logic in the skill — the LLM-driven subject inference that EXTRACT does was simply absent.

**Goal:** route the interactive skill through the same EXTRACT/MERGE pipeline as autonomous ingest, while preserving user-in-the-loop review and edit before any wiki page is written.

## Non-goals

- Built-in fetching or format conversion (unchanged from `2026-04-24` spec — `markitdown` and other converters stay outside memex).
- Cross-source EXTRACT consolidation (per-source EXTRACT, cross-source MERGE — matches `memex backfill` semantics).
- Re-implementing EXTRACT/MERGE prompts in the skill or anywhere outside `cli/src/daemon/worker/prompt.txt`.
- Bulk URL import, recursive crawl (still one-source-per-invocation).
- Direct body-content editing in any flow. If the user mutates `body` in their plan file, `plan apply` writes whatever is in the file (no validation) — this is unsupported behavior; the daemon does not detect or refuse it. A future enhancement could add a `body_hash` field (with a corresponding plan schema bump from `version: 1` to `version: 2`) to detect and refuse user-edited bodies; deferred until needed.
- Cross-restart resume of an in-progress review. If the chat session ends, the skill's temp plan file is gone and the user re-runs `/memex-ingest` (re-extract is cheap on the daemon's cheap model). Power users wanting resume can wrap the skill with a deterministic path.
- Daemon-side plan storage. The daemon is stateless on plan content — it emits plan JSON to stdout, accepts plan JSON via stdin, never opens or owns a plan file.

## Design overview

```
skill (Claude in chat)              memex CLI / daemon
─────────────────────────────       ────────────────────────────────────
1. acquire content
   markitdown <url> > /tmp/x.md     # skill never reads x.md into context

2. memex source add <url> ──────▶   # idempotent on content hash → docid
   < /tmp/x.md

3. memex source plan <docid> ────▶  # daemon worker:
                                  #   EXTRACT (chunked if needed)
                                  #   for each proposal: dedup search
                                  #   for each overlap: MERGE-dry-run
                                  #   stream plan JSON to stdout
   ◀── plan JSON on stdout
   skill writes to mktemp file

4. memex plan show < plan.json ──▶  # local CLI: read stdin, format text
   ◀── formatted text on stdout
   skill pastes to user

5. skill collects edits via AskUserQuestion (rename slug / drop / accept)
   skill mutates plan file via Edit tool (diff-only, token-efficient)

6. memex plan apply < plan.json ─▶  # daemon:
                                  #   validate plan JSON shape
                                  #   for each non-dropped non-committed:
                                  #     under per-slug lock:
                                  #       check existence + hash
                                  #       write or mark needs-rereview
                                  #   emit summary or refreshed plan on stdout
                                  #   per exit code (see right →)
   ◀── exit 0: `committed N wiki pages` on stdout
        exit 3: refreshed plan JSON on stdout (skill overwrites file)
        exit 4: partial-commit plan JSON on stdout (skill overwrites file
                with `committed: true` and `error` fields populated)

7. on exit 3: skill overwrites plan file with stdout, calls plan show,
   AskUserQuestion to re-confirm new diffs, calls plan apply again
   on exit 4: skill overwrites plan file with stdout, surfaces partial-commit
   summary (count of committed/error), AskUserQuestion to retry or abort

8. memex lint
```

**Net surface delta to memex CLI:** three new subcommands (`source plan`, `plan show`, `plan apply`) under one new noun (`plan`). All three are stdio-based — no path arguments, no daemon-side plan files. No changes to other commands.

## 1. CLI surface

### 1.1 `memex source plan <docid>`

**Purpose:** Run EXTRACT (and MERGE-dry-run for any overlapping slugs) for a stored source. Stream the resulting plan as JSON to stdout.

The verb is `plan` (not `extract`) because the operation runs more than just EXTRACT — it composes EXTRACT with one MERGE-dry-run per overlapping slug. "Plan" describes the output regardless of internal pipeline.

**Inputs:**
- Positional `<docid>`: the docid of a source previously stored via `memex source add`.

**Outputs:**
- **Success with proposals (exit 0):** stdout = plan JSON (single line or pretty-printed; consumer-agnostic). stderr = silent.
- **Empty extract (exit 0):** stdout = empty, stderr = silent. The empty stdout (combined with exit 0) is the signal; the skill surfaces a user-facing message itself.
- **Generic error (exit 1):** stdout = empty. stderr = error message.

**Side effects (daemon side):**
- Acquires per-content-hash advisory lock for the duration of EXTRACT + MERGE-dry-run only. Released before stdout is written. The lock prevents two parallel LLM calls on the same source content; it does NOT serialize against `plan apply` (no shared state).
- Calls EXTRACT on the daemon worker (chunked per `cli/src/daemon/handler/ingest.rs` if source exceeds the chunk threshold). For chunked sources, the existing pipeline runs cross-chunk MERGE on same-slug proposals; the plan's `proposals[]` list contains the FINAL consolidated subjects (one entry per slug), not per-chunk fragments.
- For each proposed page whose slug already exists in the wiki, runs MERGE-dry-run. The MERGE LLM task returns the full merged page object (`{slug, title, tags, body}`) per the worker contract at `cli/src/daemon/worker/prompt.txt:330`. **Slug is required to stay constant per the prompt's invariant** ("Each page MUST keep its original slug"); daemon asserts and copies only `title`, `tags`, `body` from MERGE output into the plan proposal. The daemon then locally computes `merge_diff = unified_diff(existing_body, merged_body)` and `merge_target_hash = sha256(existing_body)`. The diff is a deterministic local operation (no LLM call) using a standard unified-diff library (e.g., the `similar` Rust crate).
- Writes nothing to disk for the plan. The plan content is purely streamed.

**Cost transparency.** A single `source plan` call may invoke many LLM tasks: one EXTRACT per chunk, one cross-chunk MERGE per shared subject across chunks, and one MERGE-dry-run per overlap with existing wiki pages. For a 50KB source that fits in one chunk, producing 8 proposals with 3 wiki overlaps: 1 EXTRACT + 0 cross-chunk MERGE + 3 MERGE-dry-run = 4 LLM calls. For a 200KB source split into 4 chunks with the same proposal/overlap counts: 4 EXTRACT + ~3 cross-chunk MERGE (one per shared slug) + 3 MERGE-dry-run = 10 LLM calls. Daemon worker uses a cheaper model (per §6); cost stays low.

### 1.2 `memex plan show`

**Purpose:** Render a plan as human-readable text (table + diffs) for review. Used by the skill to display proposals to the user, and by users who want to inspect a plan file directly.

**Inputs:**
- Plan JSON on **stdin**.

**Flags:**
- `--json` — pass through the raw plan JSON to stdout (for scripts).

**Outputs (default, no `--json`):**
- stdout = formatted text:
  - Header: source identifier, proposal count, merge count.
  - Per-proposal table with columns `# | slug | title | tags | status` (status = `new` or `merge → <existing-slug>`).
  - For each proposal with `merge_target_slug` set: unified diff block below the table.
  - Footer: two-line hint — `To commit: pipe this plan to memex plan apply.` and `To edit: open the plan file in your editor (slug, title, dropped fields), then re-pipe to apply.`
- exit 0 on success.
- exit 1 if stdin is missing or fails JSON validation.

**Side effects:** none — pure local CLI operation. Does not invoke the daemon. Does not open any plan file (skill provides content via stdin).

### 1.3 `memex plan apply`

**Purpose:** Validate a (possibly user-edited) plan and commit each non-dropped proposal as a wiki page write, with re-MERGE for any user-edited slugs that now overlap.

**Inputs:**
- Plan JSON on **stdin**.

**Outputs:**
- **Full commit success (exit 0):** stdout = `committed N wiki pages\n` (where N is the count of newly-committed proposals; for an all-dropped plan with N=0, stdout = `committed 0 wiki pages\n`). stderr = silent.
- **Plan needs re-review (exit 3):** stdout = refreshed plan JSON (with updated `title`/`tags`/`body`/`merge_target_slug`/`merge_target_hash`/`merge_diff` for affected proposals). stderr = silent. Skill captures stdout, overwrites its plan file, and surfaces a user-facing message (e.g., from running `plan show` on the refreshed file).
- **Per-proposal failures (exit 4 — partial commit):** stdout = updated plan JSON (with `committed: true` on successes and `error` populated on failures). stderr = silent. Skill captures stdout, overwrites its plan file, and surfaces a user-facing summary by counting `committed: true` and `error` fields itself. Re-applying skips committed proposals.
- **Plan invalid / source missing (exit 1):** stdout = empty. stderr = error message.

**Plan validation (performed once, at apply start, before per-proposal logic):**
- Plan deserializes against the v1 schema; reject unknown `version`.
- `source.id` resolves to an existing source; reject otherwise.
- `source.content_hash` is a 64-character lowercase hex string; reject otherwise.
- For each proposal: `original_slug` is non-empty; `slug` is non-empty kebab-case; `merge_target_slug` is null OR a non-empty slug; `merge_target_hash` is null OR a 64-char lowercase-hex string; `committed`, `dropped` are booleans; `proposals[].index` values are unique and form a 0-based contiguous range.
- **Coupling rule for merge fields:** `merge_target_slug == null` implies `merge_target_hash == null` (no merge target = no hash). The reverse is **not** required: a proposal can have `merge_target_slug` set with `merge_target_hash == null` — this is the MERGE-dry-run-failure state (§4.5), which signals "we know there's an overlap with this slug but couldn't compute the merge yet." Apply detects null hash and re-runs MERGE-dry-run on the staleness path. Validation rejects only the strictly impossible combination: `merge_target_slug == null AND merge_target_hash != null`.
- **Effective-slug uniqueness:** among all non-dropped proposals (including those with `committed: true`), the `slug` field must be unique. Reject with `slug collision: <slug> appears in proposals X and Y`. Without this check, apply would be order-dependent (a later proposal would see the slug exists because an earlier one just wrote it).

These checks catch user-tampered plans without trying to detect every form of tampering — apply enforces the structural shape that the rest of the logic depends on.

**Per-proposal commit logic** (unified staleness check covers both "unchanged slug + merge" and "edited slug + new overlap"). The per-slug writer lock for `target` is acquired before step 4's existence check and the body re-read/hash. It is released **before** any long-running operation (the re-MERGE LLM call on the stale path) and reacquired only if a follow-up commit happens, so the lock is never held across an LLM call. The lock's purpose is to make the existence-check + write decision atomic on the wiki state, closing the TOCTOU window between read-hash-compare and write/dispatch:

1. Skip if `dropped: true`.
2. Skip if `committed: true` (already written in a prior partial-failure run; do nothing).
3. **Determine target slug:** `target = slug` (user-edited or original — same field).
4. **Acquire per-slug writer lock for `target`. Then determine if this is a merge:** under the lock, check whether `target` exists in the wiki (this final check is what matters — any pre-lock check is advisory).
   - **No** (new page — also covers "user renamed slug from one with `merge_target_slug` set to one without overlap"): clear `merge_target_slug`, `merge_target_hash`, `merge_diff` (they were stale from the original); write `body` to new page via the merge-aware writer (§1.5); on success, set `committed: true` and clear `error` (if previously populated from a failed attempt). Release slug lock.
   - **Yes** (target slug exists): re-read existing wiki page body **under the same lock**, hash it. Compare to plan's `merge_target_hash`:
     - **Hash matches AND `merge_target_slug == target`:** the existing page hasn't changed since MERGE-dry-run, and the merge target is consistent with what the plan recorded. Commit the plan's `body` (trusted as-is — body authenticity is not validated, per Non-goals) via the merge-aware writer (§1.5); on success, set `committed: true` and clear `error`. Release slug lock.
     - **Otherwise:** stale or new overlap, not yet reviewed. Release slug lock (no write happens). Run MERGE-dry-run; update plan's `title`, `tags`, `body`, `merge_diff`, `merge_target_slug`, `merge_target_hash` with the fresh result. Mark proposal as needs-rereview. (`error` field is left as-is — if it was populated from a prior MERGE failure, this successful re-MERGE replaces the bad state but the actual write hasn't happened yet, so clearing happens at write time per the bullets above.)

**Why under the slug lock:** without it, between the hash check and the write, a concurrent writer (another `plan apply` for a different source, or a direct `memex write`) could modify the target page. Apply would write its now-stale merged body, silently clobbering the concurrent edit. Holding the slug lock from existence-check through write ensures the read and write see the same wiki state.

5. After all proposals processed:
   - If any were marked needs-rereview, emit the updated plan JSON to stdout and exit 3. Re-review takes precedence over exit 4 (failures) — the user must act on it next. Per-proposal commits in step 4 are **not** rolled back when exit 3 fires.
   - Else if any failed, emit the updated plan JSON (with `committed: true` on successes and `error` on failures) to stdout and exit 4 (partial commit).
   - Else (all non-dropped non-committed proposals successfully committed), emit `committed N wiki pages\n` to stdout (where N is the number of proposals committed in this run) and exit 0. Skill `rm`s its plan file on rc=0.

**Crash-consistency limitation (accepted v1 tradeoff).** The `committed: true` flag is recorded only in the in-memory plan during apply, then emitted on stdout at exit. If apply crashes between a wiki page write succeeding and stdout being flushed, the wiki page exists with new content but the skill's plan file still shows that proposal as uncommitted.

Window: microseconds — between the wiki write's syscall return and the stdout flush. Not zero (daemon SIGKILL, OS crash, etc. can hit it).

Failure mode on retry: apply re-reads the input plan, sees the affected slug exists in the wiki under lock, hashes the existing body. The hash differs from `merge_target_hash` (because the body is now the previously-merged result, not the pre-merge original). Apply re-runs MERGE-dry-run on the now-merged body + the proposal body, producing a **double-merge** (merge of merge). Apply exits 3 with the double-merged content in the plan.

User recovery procedure:
1. Skill loops back to re-review (exit 3 path).
2. `plan show` renders the diff between the existing wiki page and the new merged body. The user inspects.
3. If the diff looks like a double-merge (duplicated sections, repeated content), the user drops the proposal (`dropped: true`) and re-applies. The wiki retains the originally-merged content from the crashed apply.
4. If the user accepts the double-merge, the wiki gets compounded content. They can fix manually with `memex write --force`.

Severity assessment: low for a personal wiki (one user, low write rate, narrow window, user-visible recovery via `plan show`). Acceptable for v1.

Captured as a TODO in `docs/specs/TODO.md`: future enhancement to add a per-write journal entry would close the window — daemon writes "about to commit slug X" to a journal before the wiki write, replays on restart. Deferred until usage observes the failure mode in practice.

### 1.4 No changes to other CLI commands

`memex source add`, `memex source list`, `memex source show`, `memex source delete`, `memex write`, `memex ingest`, `memex lint` stay as-is. The new flow uses `source add` for storage and the three new subcommands for the review-and-commit cycle. `memex write` is not invoked by the skill — `plan apply` does its own per-page writes through the daemon's existing write pipeline.

### 1.5 Daemon-side merge-aware writer (avoids `handle_write` regression)

The existing `handle_write` (`cli/src/daemon/handler/write.rs:82–130`) strips incoming frontmatter, regenerates `created_at` to now, and sets `sources` to only the supplied `--source` value. Using it directly for `plan apply` would silently regress merged pages: existing source accumulation would be lost and `created_at` would reset on every merge.

`plan apply` therefore uses a **separate writer code path** (call it `apply_proposal_to_wiki`, in a new module under `cli/src/daemon/handler/plan/` or extending `handler/ingest.rs`) with these rules:

- **New page (no existing wiki page at slug):** behave like `handle_write` — synthesize fresh frontmatter with `created_at = updated_at = now`, `sources: ["#<docid>"]`, write body.
- **Merge into existing page (slug exists):** parse existing wiki page's frontmatter; preserve `created_at`; set `updated_at = now`; merge `sources` by appending the new docid (deduped, preserving order); use the plan proposal's `tags` (which MERGE may have updated). Write the merged body with the preserved-and-extended frontmatter.
- Both paths atomic per the existing per-slug writer-lock.

This deliberately avoids using `handle_write` for the merge case. It does not modify `handle_write` or its callers; new behavior lives in the new function.

### 1.6 Daemon protocol additions

Per the existing daemon convention in `cli/src/daemon/protocol.rs:38–115`, requests are tagged enum variants and responses are streamed `Event` values terminated by `Done`. The new subcommands add:

**New `Request` variants:**

```rust
SourcePlan {
    source_id: String,
}
PlanApply {
    plan_json: String,  // raw JSON content; daemon parses
}
```

**New `Event` variants** (interleaved with existing `Queued`, `Progress`, etc.):

```rust
PlanContent { json: String }         // streamed output of source plan or apply (success/exit 3/exit 4 — skill writes to its file)
EmptyExtract { reason: String }      // source plan: no extractable subjects
PlanApplyProgress { slug: String, status: String }   // streamed during apply, advisory only
PlanApplied { committed: Vec<String> }   // apply: full success, no further plan content needed
```

**`PlanApplyProgress` is advisory and not surfaced by the CLI.** The daemon emits these events on the protocol stream; the CLI consumes-and-drops them. They do not appear on stdout (which carries data) or stderr (which is reserved for error messages, per the convention below). Programmatic consumers (future tools, watcher UIs) can read them off the protocol stream directly. A `--progress` flag to opt into surfacing them is deferred until needed.

Existing terminal frames per `protocol.rs:133–140`:
- `Error { code: String, message: String, status: i32 }` for failures.
- `Done { status: i32 }` for completion. Status carries the exit code.

**CLI exit-code mapping** (in the CLI client, after consuming the event stream):
- `source plan`: `PlanContent` then `Done{0}` → write JSON to stdout, exit 0. `EmptyExtract` then `Done{0}` → empty stdout, stderr silent, exit 0. `Error` then `Done{1}` → empty stdout, error message to stderr, exit 1.
- `plan apply`: `PlanApplied{committed}` then `Done{0}` → stdout = `committed {committed.len()} wiki pages\n`, stderr silent, exit 0. `PlanContent` then `Done{3}` → write JSON to stdout, stderr silent, exit 3. `PlanContent` then `Done{4}` → write JSON to stdout, stderr silent, exit 4. `Error` then `Done{1}` → empty stdout, error message to stderr, exit 1.

**Stderr convention:** stderr is reserved for error messages (exit 1 path). Informational status (empty extract, re-review needed, partial commit) is signaled via exit code + stdout content; consumers (the skill, scripts, humans) compute user-facing messages from those signals.

`plan show` is a **local CLI operation**, not a daemon request. It reads stdin, parses JSON, formats text, writes stdout. No daemon involvement.

These new variants reuse the daemon's existing job queue, worker pool, and writer-lock infrastructure. No changes to the worker prompt or to the `BackendJob` taxonomy.

**Progress events for long-running operations.** The daemon may emit advisory progress events while EXTRACT/MERGE LLM calls are in flight. The existing `Parsing { transcript_path }` and `Distilling { transcript_path }` event variants in `protocol.rs` carry a transcript-specific field that doesn't fit document-plan operations, so the implementation choice is one of: (a) generalize those variants to a polymorphic source identifier (breaks existing transcript-ingest CLI clients), (b) introduce new variants (`PlanProgress { stage, source_id }`) parallel to the transcript ones, or (c) keep the daemon silent for source plan / plan apply and only emit the new `PlanApplyProgress` variant defined above. Recommendation: **(c)** for v1 — minimal protocol surface, no compat risk, and the operations are bounded enough that silence is acceptable. Same handling: the CLI consumes-and-drops `PlanApplyProgress`. Long EXTRACTs (30+ seconds for chunked sources) appear silent at the CLI level; programmatic consumers can read the daemon protocol stream directly.

**Exit-code summary** (consolidated; matches the per-command outputs in §1.1 / §1.2 / §1.3):

| Exit | Meaning                                                              | Subcommand          | Stdout shape          |
|------|----------------------------------------------------------------------|---------------------|------------------------|
| 0    | Success with proposals (`source plan`)                               | `source plan`       | plan JSON              |
| 0    | Empty extract — no extractable subjects (`source plan`)              | `source plan`       | empty                  |
| 0    | Render success (`plan show`)                                         | `plan show`         | formatted text         |
| 0    | Full commit success (`plan apply`)                                   | `plan apply`        | `committed N wiki pages` |
| 1    | Generic error (daemon spawn failed, source missing, plan invalid, malformed JSON, EXTRACT failed) | all three   | empty                  |
| 3    | Plan needs re-review (only `plan apply`)                             | `plan apply`        | refreshed plan JSON    |
| 4    | Partial commit (only `plan apply`)                                   | `plan apply`        | updated plan JSON with `committed`/`error` populated |

Exit 2 is intentionally NOT used — there is no "plan exists" conflict in the skill-owned design (each skill session creates its own temp file via `mktemp`).

## 2. Plan JSON schema

Streamed between skill and daemon via stdin/stdout. Skill stores it in its own temp file during the review window.

```json
{
  "version": 1,
  "source": {
    "id": "src-abc123",
    "identifier": "https://memverge.atlassian.net/.../page",
    "content_hash": "abc123def456789012345678901234567890123456789012345678901234abcd",
    "size_bytes": 12345
  },
  "created_at": "2026-04-30T19:42:00Z",
  "proposals": [
    {
      "index": 0,
      "slug": "mmai",
      "title": "MMAI",
      "tags": ["ai", "platform"],
      "body": "...full markdown body...",
      "merge_target_slug": "mmai",
      "merge_target_hash": "f1e2d3c4b5a6978869504c3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f",
      "merge_diff": "--- a/mmai\n+++ b/mmai\n@@ ...\n+## New section\n+...",
      "dropped": false,
      "committed": false,
      "original_slug": "mmai"
    },
    {
      "index": 1,
      "slug": "gpu-checkpoint",
      "title": "GPU Checkpoint",
      "tags": ["gpu", "checkpoint"],
      "body": "...",
      "merge_target_slug": null,
      "merge_target_hash": null,
      "merge_diff": null,
      "dropped": false,
      "committed": false,
      "original_slug": "gpu-checkpoint"
    }
  ]
}
```

**Field semantics:**

- `version`: schema version. v1 only; bump on any breaking change.
- `source.id`: the source's docid (returned by `memex source add`).
- `source.identifier`: the user-supplied identifier passed to `memex source add` — may be a URL, a filesystem path, a logical label, or any string. Not constrained to URL form.
- `source.content_hash`: lowercase hex digest of sha256(source content). 64 characters, no algorithm prefix.
- `source.size_bytes`: byte length of the source content. Informational; used by `plan show` for the header line (e.g., `4823 bytes → 3 proposals`).
- `created_at`: ISO-8601 UTC timestamp when the plan was generated by `source plan`. Informational/audit only; not consumed by validation or apply logic.
- `proposals[]`: the EXTRACT output, one entry per proposed wiki page.
  - `index`: 0-based stable identifier for user-facing references. Assigned by `source plan` in proposal order (after cross-chunk MERGE consolidation); immutable across re-applies. Used by `plan show` output ("proposal 0", "proposal 1") and by validation error messages (e.g., the slug-collision message in §1.3 — `slug collision: <slug> appears in proposals X and Y`). Not user-mutable.
  - `slug`, `title`, `tags`, `body`: as emitted by EXTRACT. For overlap cases, MERGE-dry-run may update `title`, `tags`, and `body` (slug must stay constant per the prompt's "Each page MUST keep its original slug" rule, asserted by daemon). The daemon copies only `title`, `tags`, `body` from MERGE output into the plan; slug is left unchanged.
  - `merge_target_slug`: the existing slug this proposal merges into. `null` = new page.
  - `merge_target_hash`: lowercase hex digest of sha256(body at `merge_target_slug` at MERGE-dry-run time). 64 characters, no algorithm prefix. `null` if no merge.
  - `merge_diff`: unified diff between `existing_body` (at `merge_target_slug`) and the MERGE'd `body`, computed locally by the daemon (not by the LLM). `null` if no merge.
  - `dropped`: user-editable. `true` = skip on apply.
  - `committed`: set by `apply` after a successful per-page write; emitted in the apply stdout JSON. On re-apply, the daemon skips proposals where `committed: true`.
  - `original_slug`: immutable. Captured at extract time. Informational/audit only — apply does NOT branch on `slug == original_slug`; it routes purely on under-lock target-existence + hash check (§1.3 step 4). Kept in the schema so users and `plan show` can see what slug EXTRACT originally proposed vs. any user edits.
  - `error` (optional, string): per-proposal failure cause. Populated by either `source plan` (when MERGE-dry-run fails for an overlap proposal — see §4.5) or `plan apply` (when a per-proposal commit fails — see §4.4). Cleared on a successful retry of that proposal. The two error origins have different field invariants:
    - **`source plan` MERGE-dry-run failure (§4.5):** `body` is the un-merged EXTRACT output. The daemon also nullifies `merge_target_hash` and `merge_diff` so `plan apply` re-runs MERGE-dry-run on the staleness-check path — this prevents the un-merged `body` from ever being written into an existing wiki page.
    - **`plan apply` write failure (§4.4):** `body` is already a successful merge (the staleness check passed before the write attempt). The daemon preserves `merge_target_hash` and `merge_diff` so a retry takes the matched-hash commit path without a redundant re-MERGE.

**User-mutable fields:** `slug`, `dropped`. Optionally `title` (advanced). All other fields are read-only from the skill's side; the daemon mutates `title`, `tags`, `body`, `merge_diff`, `merge_target_slug`, `merge_target_hash`, `committed`, `error` during `source plan` (MERGE-dry-run output) and `plan apply` (re-MERGE on staleness, commit, error), emitting the updated plan on stdout for exit 3 and exit 4. (`title` is mutated by both user and daemon; the daemon's MERGE output overwrites the user's edit on the staleness path.)

## 3. Skill flow

The skill becomes a thin orchestrator around the three new commands. The bash below is **conceptual pseudocode**, not a runnable script. Most of the skill's per-state work is not bash at all — it's the agent (Claude) using its `AskUserQuestion` tool, its `Edit` tool, and pasting text to chat. The bash here shows the control flow; comments mark where the agent steps in. Lines like `# Skill: AskUserQuestion to ...` are not invocations of bash functions — they describe what the SKILL.md tells the agent to do at that point in the flow.

```bash
# Step 1: acquire content (unchanged)
content_path=""; plan_file=""
trap 'rm -f "$content_path" "$plan_file"' EXIT
content_path=$(mktemp /tmp/memex-ingest-content-XXXXXX.md)
markitdown "$url" > "$content_path"
# Skill author note: do NOT `cat $content_path` — keeps source content
# out of Claude's context. Pipe directly into source add.

# Step 2: store source
src=$(memex source add "$url" < "$content_path"); rc=$?
if [ $rc -ne 0 ]; then
  # Skill: paste source-add stderr to chat, abort. Trap removes content_path.
  exit 1
fi

# Step 3: create plan (skill owns the file)
plan_file=$(mktemp /tmp/memex-plan-XXXXXX.json)
memex source plan "$src" > "$plan_file"; rc=$?
case $rc in
  0)
    if [ ! -s "$plan_file" ]; then
      # Empty extract. Surface user-facing message ourselves.
      echo "no extractable subjects in $url"
      rm -f "$plan_file"
      exit 0
    fi
    # Validate JSON parseability — guards against partial-stream corruption
    # if the daemon connection dropped mid-write.
    if ! jq -e . "$plan_file" >/dev/null 2>&1; then
      # Client-side check, no daemon stderr to inherit; skill emits the message itself.
      echo "plan file is not valid JSON; daemon may have crashed mid-stream" >&2
      exit 1
    fi
    ;;
  *)
    # Skill: paste source-plan stderr to chat, abort.
    exit 1
    ;;
esac

# Step 4: review loop
while true; do
  # Skill: render plan via `memex plan show < $plan_file`, paste output to chat.
  # Skill: AskUserQuestion to collect edits (rename slug / drop / accept).
  # Skill: for slug/dropped edits, use Edit tool to mutate $plan_file (diff-only).
  #        For title edits, advise editor-handoff; user runs $EDITOR on $plan_file.
  # Skill: if user dropped all proposals or aborted, exit (trap rms file).
  # Skill: if user approved, break.
  break  # placeholder — actual exit/break is decided by the agent above
done

# Step 5: apply (with bounded re-review loop on exit 3, partial-commit visibility on exit 4)
rereview_count=0
MAX_REREVIEWS=5
while true; do
  apply_out=$(mktemp /tmp/memex-apply-XXXXXX.json)
  memex plan apply < "$plan_file" > "$apply_out"; rc=$?
  case $rc in
    0)
      cat "$apply_out"   # surface "committed N wiki pages" to chat
      rm -f "$apply_out" "$plan_file"
      break
      ;;
    3)
      rereview_count=$((rereview_count + 1))
      if [ "$rereview_count" -gt "$MAX_REREVIEWS" ]; then
        # Skill: tell user "wiki page is mutating faster than review can keep up;
        #        re-run /memex-ingest after concurrent writers calm down".
        # Message goes to stderr because it accompanies an error exit (exit 1);
        # per the spec's stdout/stderr convention, stdout is for success only.
        echo "re-review exhausted after $MAX_REREVIEWS cycles; retry later" >&2
        rm -f "$apply_out"
        exit 1   # exhaustion is a failure; trap removes plan_file
      fi
      mv "$apply_out" "$plan_file"   # overwrite with refreshed plan
      # Skill: render via `memex plan show < $plan_file`; AskUserQuestion to accept new diffs or abort.
      # Skill: if user aborted, exit 1 (trap removes plan_file). If accepted, loop continues to next apply.
      ;;
    4)
      mv "$apply_out" "$plan_file"   # overwrite with committed/error fields populated
      # Skill: read $plan_file via Read tool, count `committed: true` and proposals with `error`,
      #        surface a partial-commit summary to chat, AskUserQuestion to retry or abort.
      # Skill: if user aborted, exit 1 (trap removes plan_file). If retry, loop continues to next apply.
      ;;
    *)
      rm -f "$apply_out"
      # Skill: paste apply stderr to chat, abort.
      exit 1
      ;;
  esac
done

# Step 6: lint
memex lint
```

### 3.1 Rendering proposals

The skill calls `memex plan show < $plan_file` to render the plan, then pastes the output to chat. The skill does not parse the plan JSON itself — `plan show` owns the canonical presentation format.

Expected output of `memex plan show`:

```
PLAN: https://example.com/article (4823 bytes → 3 proposals, 1 merge)

# | slug             | title                       | tags          | status
--+------------------+-----------------------------+---------------+----------------------
0 | mmai             | MMAI                        | ai, platform  | merge → mmai (diff↓)
1 | gpu-checkpoint   | GPU Checkpoint              | gpu           | new
2 | release-process  | MMAI/MVTCO Release Process  | release       | new

--- diff: mmai (proposal 0 → existing wiki page) ---
+## Design (Milestone 3 and Beyond)
+
+The MMAI scheduling design adds a new layer ...

To commit: pipe this plan to `memex plan apply`.
To edit:   open the plan file (skill's temp path) in your editor (slug, title, dropped fields), then re-pipe to apply.
```

Format details (column widths, diff rendering) live in the `plan show` implementation. This spec doesn't pin them.

### 3.2 Edit collection

Three modes by proposal count:

- **1–5 proposals:** AskUserQuestion with per-proposal options (`rename slug`, `drop`, `accept`). One question per proposal that needs user input.
- **6–20 proposals:** AskUserQuestion only for proposals the skill flags as suspect (slug violates EXTRACT forbidden patterns: contains a date, version qualifier, or episode word). Other proposals accepted by default.
- **>20 proposals:** editor handoff. Skill says: "open `<plan-file-path>` in your editor, change `slug`, `title`, or set `dropped: true`, then say 'apply'." Skill awaits user confirmation, then proceeds. Title edits flow through this path regardless of proposal count.

### 3.3 Title edits

Inline AskUserQuestion offers `rename slug` and `drop` only. To change a proposal's `title`, the user enters the editor-handoff path: `/memex-ingest --editor <url>` forces editor-handoff mode regardless of proposal count.

### 3.4 Token efficiency

The skill author MUST NOT `cat` the source content into Claude's context:

```bash
markitdown <url> > /tmp/content.md
memex source add <url> < /tmp/content.md   # content goes to daemon, not Claude
```

For **rendering** the plan to the user, the skill calls `memex plan show < $plan_file` and pastes the output to chat. The skill does not parse the plan JSON itself.

For **mutations** (slug renames, dropped flags), the skill uses the **Edit** tool against the plan file (diff-only). The Edit tool reads only the relevant region of the file, mutates a small substring, and writes it back. Per-mutation token cost stays near-constant regardless of plan size.

The source content's bulk stays in the daemon's content-addressed raw store and the worker model's context — never enters Claude's chat context. Avoid `jq`-pipeline regenerations of the whole plan.

## 4. Error handling and concurrency

### 4.1 Plan file lifecycle

The skill owns its plan file end-to-end:
- Created via `mktemp` at the start of step 3.
- Overwritten on `plan apply` exit 3 (refreshed plan from stdout) and exit 4 (committed/error fields populated).
- Mutated by the skill's Edit tool during review.
- Deleted on apply exit 0 success, or via shell trap on skill exit/error (including re-review exhaustion).

The daemon does not read, write, or know about the file. No GC, no orphan management, no `memex plan list` — these aren't memex's concern. If the skill crashes mid-review and leaves a `/tmp/memex-plan-XXXXXX.json` behind, normal `/tmp` cleanup (system reboot or `tmpwatch`) eventually removes it.

### 4.2 Concurrency: two `source plan` on the same source

Per-content-hash advisory lock during EXTRACT/MERGE prevents two simultaneous LLM calls. Second invocation blocks until the first releases, then runs its own EXTRACT (since plans aren't shared). Each gets its own plan content on stdout. Skills running in parallel each have their own temp file. No shared state to race on.

This is intentional — re-running `source plan` on the same source is rare and the cost is bounded by the cheap-model LLM rate.

### 4.3 Concurrency: wiki changes between extract and apply

The `merge_target_hash` field on each merge proposal records the existing wiki page's hash at MERGE-dry-run time. The unified staleness check in §1.3 step 4 handles this:
- Apply re-reads each target slug's existing body **under the per-slug writer lock**, hashes it.
- If hash matches `merge_target_hash` AND `merge_target_slug == target`, the merged body in the plan is still valid — commit. Both conditions are required: hash check ensures the existing page hasn't been modified externally; slug check ensures the user hasn't edited the proposal's slug to point at a different existing page that happens to share the same body hash.
- If either condition fails, re-run MERGE-dry-run, refresh `title`/`tags`/`body`/`merge_target_slug`/`merge_target_hash`/`merge_diff` in plan, mark needs-rereview, exit 3 with refreshed plan on stdout.

For non-merge proposals (`merge_target_slug: null`) where the target slug now unexpectedly exists in the wiki: the same logic detects "slug exists" under the lock and falls into the staleness-check path — runs MERGE-dry-run against the new existing page and exits 3.

**Bounded re-review (livelock protection).** Under hot concurrent edits, exit 3 could fire on every apply cycle indefinitely. The skill enforces `MAX_REREVIEWS = 5`. When exceeded: the skill emits a stderr message (`re-review exhausted after 5 cycles; retry later` — stderr because it accompanies an error exit, per the stdout/stderr convention), pastes a chat message explaining the situation ("the target wiki page is being modified faster than review can keep up; pause concurrent writers and re-run /memex-ingest"), and exits 1 — **does not** fall through to `memex lint` or any success-tail behavior. The EXIT trap removes the plan file along with the content file. Re-extraction on the cheap worker is inexpensive (per §6), so re-running from scratch is cheap.

### 4.4 Apply partial failure

Per-page writes are atomic per file (filesystem-canonical wiki). If 5 of 7 writes succeed and 2 fail (e.g., transient lock contention, embedding model unavailable):
- Each successful write is recorded as `committed: true` in the in-memory plan.
- Failed proposals get `error` populated.
- After processing all proposals, daemon emits the updated plan JSON on stdout and exits 4. stderr is silent (per stderr convention in §1.6).
- Skill captures stdout, overwrites its plan file, derives a partial-commit summary by counting `committed: true` and `error` fields, and surfaces the summary to the user.
- Re-applying skips `committed: true` proposals and retries the failed ones.

### 4.5 EXTRACT/MERGE LLM failures

- EXTRACT yields zero pages: empty stdout, stderr silent, exit 0. Skill detects the empty-stdout signal and surfaces a user-facing message itself.
- EXTRACT fails (LLM error, timeout): empty stdout, stderr `extract failed: <reason>`, exit 1. No plan content emitted.
- MERGE-dry-run fails for an overlap proposal: that proposal's `error` field gets populated; `merge_target_hash` and `merge_diff` are nullified (so apply cannot mistakenly take the "already reviewed merge" path); `merge_target_slug` may stay populated as informational; `body` stays as the un-merged EXTRACT output. `plan show` renders the proposal with an `[ERROR]` status. User options: drop, rename to a different slug, or retry `plan apply` — which sees `merge_target_hash: null` plus an existing slug in the wiki and re-runs MERGE-dry-run on the staleness check path.

### 4.6 Daemon-down failure modes

The CLI uses `connect_or_spawn` (`cli/src/daemon/client.rs:46`) which auto-starts the daemon if not running. Real failure modes:
- **Daemon spawn fails** (port/socket conflict, missing binary): stderr from `connect_or_spawn`; exit 1.
- **Daemon crashes mid-stream during `source plan`:** CLI sees connection drop. The skill's plan file may be empty (no bytes streamed) or contain partial JSON (some bytes received before the drop). The skill's JSON-validity check in §3 step 3 catches the partial case and triggers an abort path (`exit 1`); the EXIT trap then removes the plan file. User re-runs.
- **Daemon crashes mid-stream during `plan apply`:** partial wiki writes may have committed; skill's plan file (the input) is unchanged. On retry, apply re-reads the input plan, sees existing wiki pages for already-written slugs, runs MERGE-dry-run, exits 3 — user reviews and accepts (potentially producing a double-merge per the §1.3 crash-consistency limitation).

### 4.7 Watcher behavior — read-only with respect to ingestion

The daemon's filesystem watcher (`cli/src/daemon/watcher.rs`, bound to `wiki_dir` and `raw_dir` per `cli/src/daemon/server.rs:319–322`) **observes new files for indexing only**. It does not trigger EXTRACT, MERGE, or any wiki page creation.

The skill's plan file (in `/tmp` or wherever the skill puts it) is not under any watched directory.

After `memex source add`: the source is indexed for search via the watcher's `index_raw_file` path; no wiki pages are created. After `memex plan apply`: wiki page writes are indexed by both apply's own indexing call and (eventually) by the watcher; both are idempotent on the documents table. This separation is what makes interactive review safe — the daemon never auto-extracts a source the user is still reviewing.

### 4.8 Daemon environment requirements

Inherited from existing memex architecture, called out here for completeness:
- `<memex_root>` must support POSIX rename atomicity (atomic wiki writes via tmp + rename).
- `<memex_root>` must be on a local filesystem. Advisory locks (per-content-hash, per-slug) are unreliable on NFS/SMB. The daemon refuses to start if `<memex_root>` is on a network FS (`fs_kind::is_network_fs` check).

The skill's plan file lives in `/tmp` (or wherever `mktemp` puts it), independent of `<memex_root>`. No additional FS requirement on `/tmp` beyond what `mktemp` already requires.

## 5. Migration from the current skill

The current `/memex-ingest` skill at `~/.claude/local-marketplaces/memex-dev/plugin/skills/memex-ingest/SKILL.md` is replaced wholesale. The new SKILL.md:

1. Drops the "Step 1–5" propose/approve loop documentation.
2. Replaces with the bash flow in §3.
3. Keeps the content-acquisition guidance (§3.1 of the old skill — markitdown/agent-browser, login redirect detection).
4. Updates the "Common mistakes" section to forbid `memex source add` + `memex write` direct flow (the skill must use `source plan` + `plan show` + `plan apply`).
5. Adds the token-efficiency note (§3.4 above).

**No data migration required.** Memex is a greenfield project — there is no production deployment with legacy wiki pages produced by the old skill. The 65 wrongly-extracted Confluence pages from the observed failure session were exploratory dogfooding, not user data. They can be deleted by hand and re-ingested through the new flow when the implementation lands.

## 6. Why daemon EXTRACT (not skill EXTRACT)

A "skill EXTRACT" alternative would have the skill load the canonical EXTRACT prompt from disk and have Claude execute extraction itself, eliminating the daemon round-trip. We considered and rejected it for two reasons:

1. **Cost.** The daemon worker uses a cheaper model (e.g., `gpt-4o-mini`) for EXTRACT/MERGE. Source-content reading at daemon model rates (~$0.15/M input tokens) is ~20× cheaper than at Claude rates (~$3/M input tokens). For a 10KB source: daemon path ~$0.034, skill-EXTRACT path ~$0.075. Daemon is ~2× cheaper despite using more total tokens.
2. **Skill simplicity and prompt stability.** Skill EXTRACT requires Claude to reliably emit valid plan JSON following the EXTRACT prompt's rules. Subtle prompt-following bugs would be invisible until production. Daemon EXTRACT keeps the prompt as the daemon's tested code path, with a single source of truth.

The cost: about 30% more total tokens across the two agents, but at favorable model-mix economics.

## 7. Testing strategy

### 7.1 Unit tests (Rust)

Add to `cli/src/daemon/handler/ingest.rs` (or a new sibling module `plan.rs`):

- Plan JSON serialize/deserialize round-trip; reject unknown `version`.
- Slug overlap detection across proposed pages and existing wiki state.
- `merge_target_hash` staleness check: matches → commit, differs → exit 3.
- Edit detection (informational): `slug != original_slug` indicates a user edit (visible in `plan show`); the apply path does not branch on this.
- Edge cases: zero proposals from `source plan` (empty stdout, stderr silent, exit 0); all-dropped plan in `plan apply` (commits nothing, exits 0 with stdout = `committed 0 wiki pages`); single-chunk vs multi-chunk source.
- `plan show` rendering: golden-output tests for the table format, diff block formatting, footer hints. Cover plans with 0 / 1 / many proposals; with and without merge_targets; with `error` field set on a proposal.

### 7.2 Integration tests (`cli/tests/e2e/`)

- **Happy path:** `source add → source plan → plan show → plan apply` with no edits → verify wiki pages exist with expected slugs/bodies/sources frontmatter; assert `plan apply` stdout matches `^committed <N> wiki pages$\n` where `<N>` is the literal expected proposal count (e.g., `committed 3 wiki pages` for a 3-proposal plan; the regex should use `[0-9]+` if matching loosely, but the test should pin a specific number).
- **Edit-and-retry (rename to existing slug):** `source plan` → mutate plan slug to overlap with seeded existing page → `plan apply` → exit 3 with refreshed plan on stdout → second `plan apply` (re-piping the refreshed plan) → success.
- **Edit slug to non-overlap (merge metadata cleared):** `source plan` produces a proposal with `merge_target_slug` set → user renames slug to one with no overlap → `plan apply` clears `merge_target_slug`/`merge_target_hash`/`merge_diff` and writes as new page.
- **Concurrent source plan:** two `source plan` invocations on the same source race; per-content-hash lock serializes them; both eventually succeed and emit their own plan content (each potentially different due to LLM nondeterminism).
- **Concurrent plan apply on same content:** two `plan apply` calls with the same input plan; both run their per-slug locks; one wins per slug, the other sees the existing page under the lock and goes through staleness check (likely exit 3).
- **External wiki change:** between `source plan` and `plan apply`, modify the wiki page that `merge_target_slug` points to; subsequent `plan apply` detects via `merge_target_hash`, exits 3 with refreshed plan on stdout.
- **Partial-commit recovery:** simulate failure on proposal #2 of 4 (e.g., embedding model unavailable); apply exits 4 with stdout = updated plan (3 committed, 1 error). Re-piping the same plan to apply skips committed ones, retries the failed one; on success, exit 0.
- **Source deleted between plan and apply:** create plan, then `memex source delete <docid>`, then pipe plan to `plan apply` → exit 1 with `source missing`.
- **All-dropped plan:** mark every proposal `dropped: true`, pipe to `plan apply` → exit 0, stdout = `committed 0 wiki pages`, no wiki writes.
- **Empty extract:** `source plan` for a navigation-only source; exit 0, empty stdout, stderr silent.
- **Large source:** chunked EXTRACT (synthetic source > chunk threshold); cross-chunk MERGE produces FINAL consolidated proposals (one entry per slug, not per chunk); `plan apply` commits.
- **`plan show` default render:** plan JSON on stdin → table + diffs on stdout.
- **`plan show --json`:** JSON pass-through is byte-identical to input.
- **`plan show` invalid input:** malformed JSON on stdin; exit 1 with error.
- **Merge-aware writer preserves `created_at`:** seed an existing wiki page with `created_at: 2024-01-01`; run plan apply that merges into it; verify the resulting page still has `created_at: 2024-01-01` and `updated_at` is fresh.
- **Merge-aware writer accumulates sources:** seed an existing page with `sources: ["#src-old"]`; merge in a proposal sourced from `src-new`; verify page now has `sources: ["#src-old", "#src-new"]`.

**Architecture-specific tests (skill-owned plan file via stdio):**
- **`source plan` outputs to stdout, not a file:** invoke `memex source plan <docid>` and assert exit-0 stdout is the plan JSON; assert no file is created under `<memex_root>/plans/` (directory should not even exist as a daemon-managed location).
- **`plan show` reads from stdin:** pipe a plan JSON via stdin (`memex plan show < plan.json`); assert exit 0 with formatted text on stdout. Calling `plan show` with no stdin should exit 1.
- **`plan apply` reads from stdin and writes refreshed plan to stdout on exit 3:** pipe a plan with a slug edited to overlap with a seeded existing page; assert exit 3 and stdout contains the refreshed plan JSON.
- **`plan apply` writes updated plan to stdout on exit 4:** simulate one proposal failing during apply; assert exit 4 and stdout contains plan JSON with `committed: true` set on the successful proposals and `error` populated on the failed one.
- **Daemon does not create `<memex_root>/plans/`:** after a full source plan + plan apply cycle, assert `<memex_root>/plans/` directory is absent (the skill's temp file lives elsewhere; daemon never owns this path).
- **MERGE-dry-run failure leaves plan in correct state:** force MERGE LLM to fail on an overlap proposal during `source plan`; assert plan stdout has the proposal's `error` populated, `merge_target_hash` and `merge_diff` nullified, `merge_target_slug` populated as informational, `body` is the un-merged EXTRACT output. `plan apply` on this plan re-runs MERGE-dry-run on the staleness path (because hash is null) — verify behavior on the retry.
- **Partial-stream corruption (daemon crashes mid-source-plan):** simulate daemon connection drop mid-output during `source plan`; assert the skill's JSON-validity check (`jq -e .`) catches the partial JSON and aborts (exit 1); the EXIT trap removes the partially-written plan file. Skill exits with a clear error message; the on-disk wiki state is unchanged.
- **Re-review exhaustion exits 1 and cleans up:** drive `plan apply` to exit 3 more than `MAX_REREVIEWS` times (e.g., by mutating the target wiki page on every retry); assert the skill exits 1 (exhaustion is a failure outcome, NOT a fall-through to lint); assert stderr contains the `re-review exhausted` message; assert both `plan_file` and `content_path` are removed by the EXIT trap.

### 7.3 Skill-level walkthrough (manual)

Skills aren't unit-testable. Manual checklist:

- Single-URL ingest with 3 proposals; verify chat UX renders table + diffs + AskUserQuestion correctly.
- Editor-handoff mode (>20 proposals); verify skill prompts user to edit the temp file path and resumes after.
- User aborts mid-review (presses Ctrl+C); skill's trap removes the temp plan file.
- `memex lint` runs cleanly post-apply.

## 8. Open questions

1. **Editor-handoff mode for AskUserQuestion.** The 1–5 / 6–20 / >20 thresholds are heuristics. Could be a config setting (`~/.memex/config.toml` or similar) once usage data accumulates.
2. **Cross-source EXTRACT (option C from brainstorming).** Per-source EXTRACT misses opportunities to consolidate when multiple sources clearly cover the same subject (e.g., 3 Confluence pages all about `mmai`). Adding cross-source EXTRACT would require batching sources before the EXTRACT call, which exceeds context budget for >5–10 sources. Defer to a future spec.
3. **`memex prompt extract` for skill-side EXTRACT** (per §6). If the model-cost gap closes (Claude becomes cheaper, or memex switches to a more expensive worker model), skill-EXTRACT becomes attractive again. The CLI surface change would be small; revisit if economics shift.

## Appendix A: Comparison to current skill

| Aspect                        | Current skill (`2026-04-24` design)                | This redesign                                           |
|-------------------------------|----------------------------------------------------|---------------------------------------------------------|
| Subject extraction            | Agent guesses from raw markdown                    | Daemon worker runs EXTRACT prompt                       |
| Slug discipline               | None (title-derived; agent free-form)              | EXTRACT prompt enforces (no episode/date/multi-subject) |
| Cross-source consolidation    | None (each `memex write` independent)              | MERGE folds overlapping slugs                           |
| Token cost (10KB source)      | ~25KB at Claude rates (~$0.075)                    | ~33KB across daemon (cheap) + Claude (~$0.034)          |
| Skill complexity              | Bash loop with manual page proposals               | Three CLI calls (`source plan`, `plan show`, `plan apply`) + AskUserQuestion + Edit tool |
| Plan storage                  | n/a                                                | Skill-owned temp file (mktemp); daemon stateless on plan |
| Failure observed              | Subject extraction skipped entirely; ~10 of 65 Confluence pages had clear EXTRACT-rule violations; the rest were 1:1 title-derived rather than subject-anchored | Subject of this redesign |
| Resume across chat restart    | None                                               | None (re-run is cheap; deferred to skill author if wanted) |

## Appendix B: Why skill-owned plan file (and not daemon-owned)

An earlier draft had the daemon own a plan file at `<memex_root>/plans/<content_hash[:16]>.json`. Review surfaced that the dominant complexity source was **two writers (skill via Edit tool, daemon via direct file ops) on one file**: lock-domain races, path-validation, mid-flight `rm` semantics, read-only preflight, atomic-cross-files crash consistency, and lock-order invariants between per-content-hash and per-slug locks.

Switching to skill-owned (skill writes/reads its own temp; daemon stateless on plan content via stdio) eliminates that entire class of issues. The cost is small: re-extract on chat restart (cheap on the cheap worker model), no `memex plan list` for orphans (already out of scope), and ~10 lines of skill bash for temp-file lifecycle.
