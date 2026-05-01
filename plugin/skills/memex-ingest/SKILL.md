---
name: memex-ingest
description: |
  Use when the user provides a URL, file, document, or text to add to their wiki,
  or says "ingest this", "add to wiki", "read this and create pages", or shares
  material for knowledge capture. Routes through the daemon's EXTRACT/MERGE
  pipeline; the user reviews proposed pages before any wiki write happens.
  For non-interactive ingest (hooks, scripts), use `memex ingest --source <id>`
  directly instead.
---

# memex-ingest — interactive source ingestion

This skill turns one source (URL, document, text blob) into one or more wiki
pages with the user reviewing the proposed pages before anything is written.
For agent session transcripts (Claude Code / Codex / Gemini CLI `.jsonl`
files), use `memex backfill <agent>` instead.

**Argument:** the source URL or filesystem path. Examples:
`/memex-ingest https://example.com/article`,
`/memex-ingest ~/Downloads/spec.pdf`.

## How it works

The skill is a thin orchestrator around four CLI subcommands:

1. `memex source add <path-or-url>` — store the source content in the raw store; daemon returns a docid.
2. `memex source plan <docid>` — daemon runs EXTRACT (chunked) + MERGE-dry-run for any overlapping wiki slugs; emits plan JSON to stdout.
3. `memex plan show < plan.json` — local renderer; produces a human-readable table + diff blocks.
4. `memex plan apply < plan.json` — daemon validates and writes each non-dropped proposal as a wiki page.

The skill stores the plan at a deterministic path derived from the docid:
`/tmp/memex-plan-<docid>.json`. The docid is content-addressed (sha256-derived),
so this path is unique per source content and stable across the multi-step flow.
The daemon stays stateless on plan content — it only ever sees the JSON via
stdin/stdout.

## How an agent should execute this

This skill runs as a sequence of **discrete tool calls**, not as a single shell
script. Each Bash tool call is its own subshell — shell variables (`$docid`,
`$plan_file`) do **not** persist between calls. To carry state, the agent
captures the docid from the first call's stdout and **interpolates the literal
docid value** into every subsequent command (using `/tmp/memex-plan-<docid>.json`
where `<docid>` is the actual value, e.g., `/tmp/memex-plan-75908e2.json`).

Between Bash calls, the agent surfaces output to chat for the user to
review, and uses its file-editing tool (whatever is available — Edit
in Claude Code, equivalent in other agents) to mutate the plan JSON
file in-place by single-field swaps. These are agent runtime tools,
not shell commands.

## Flow

### Step 1 — Acquire content and store the source (one Bash call)

For URLs, pipe `markitdown` directly into `source add`. For local files, pipe
the file directly. **Do NOT `cat` the content into chat** — the bytes must go
straight from the converter (or file) into `memex source add`'s stdin.

```bash
# URL case:
docid=$(markitdown "$url" | memex source add "$url")

# Local file case:
docid=$(memex source add "$path" < "$path")

echo "DOCID=$docid"
```

The agent captures the docid from `DOCID=` and uses the literal value (e.g.
`75908e2`) in every subsequent step.

If `memex source add` exits non-zero, paste its stderr to chat and abort.

### Step 2 — Generate the plan (one Bash call, deterministic path)

```bash
memex source plan <docid> > /tmp/memex-plan-<docid>.json
echo "rc=$? size=$(wc -c < /tmp/memex-plan-<docid>.json)"
```

Substitute `<docid>` with the literal value from Step 1.

- **Empty file (size = 0) and rc=0**: EXTRACT yielded no extractable subjects.
  Tell the user, `rm -f /tmp/memex-plan-<docid>.json`, and stop.
- **Non-zero rc**: paste daemon stderr to chat and abort.

### Step 3 — Render and review

Render the plan once for the user:

```bash
memex plan show < /tmp/memex-plan-<docid>.json
```

Paste the output to chat. Then collect the user's review using
**one** of the three paths below. Pick by proposal count and your
agent's available tools.

**Path A — Structured questions (preferred when available, ≤20 proposals):**

If your agent has a chip-style question tool (e.g., Claude Code's
`AskUserQuestion`), use it:

- **1–5 proposals:** ask per-proposal with options `accept` /
  `rename slug` / `drop`. When the user picks `rename slug`, the
  new slug comes back via the `Other` free-text channel.
- **6–20 proposals:** ask only about proposals you flag as suspect
  (slug contains a date like `2026-04`, a version qualifier like
  `v3`, an episode word like `milestone`/`session`, or duplicates
  an obvious wiki subject). Other proposals auto-accept.

**Path B — Chat directives (fallback for any agent, ≤20 proposals):**

If your agent has no structured question tool, end your chat message
with this exact prompt line:

> Reply `apply` to commit as-is, or send specific edits like
> `drop <slug>; rename <slug> to <new-slug>` before approving.

Wait for the user's reply. Parse it as `;`-separated directives:

- `apply` → proceed to Step 4
- `cancel` / `abort` → `rm -f /tmp/memex-plan-<docid>.json` and exit
- `drop <slug>` → set `dropped: true` on that proposal
- `rename <old-slug> to <new-slug>` → set `slug: "<new-slug>"`

**Path C — Editor handoff (always for >20 proposals, or any title edit):**

For **>20 proposals**: skip both inline paths (the `AskUserQuestion`
schema caps at 4 questions per call and a directive list of that
size is unwieldy). Tell the user the plan is at
`/tmp/memex-plan-<docid>.json`, ask them to open it in their editor,
edit `slug` / `title` / `dropped` fields, then reply with `apply`.

**Title edits** always go through the editor handoff regardless of
proposal count — the directive language and `AskUserQuestion`
options don't cover title edits.

---

Apply each rename/drop via your agent's file-edit tool against
`/tmp/memex-plan-<docid>.json` — single-field swap on the affected
proposal, diff-only. Never re-emit the whole plan via `jq` or any
rewrite path.

After applying edits, re-run `memex plan show < /tmp/memex-plan-<docid>.json`
and prompt again — the user may want a second pass. Loop until the
user replies `apply` or `cancel`.

### Step 4 — Apply, with bounded re-review loop

The agent runs Step 4 in a loop, capped at `MAX_REREVIEWS = 5`. Each iteration
is one Bash call that runs `memex plan apply` and decides what to do based on
the exit code.

**One iteration:**

```bash
memex plan apply < /tmp/memex-plan-<docid>.json > /tmp/memex-apply-<docid>.json
rc=$?
echo "RC=$rc"
case $rc in
  0)
    cat /tmp/memex-apply-<docid>.json   # "committed N wiki pages"
    rm -f /tmp/memex-plan-<docid>.json /tmp/memex-apply-<docid>.json
    ;;
  3|4)
    mv /tmp/memex-apply-<docid>.json /tmp/memex-plan-<docid>.json
    # Daemon emitted refreshed plan (rc=3) or partial-commit plan (rc=4).
    # Agent inspects via plan show / Read and decides next step.
    ;;
  *)
    rm -f /tmp/memex-apply-<docid>.json
    ;;
esac
```

**Agent decision per `rc`:**

| `rc` | Meaning | Next action |
|------|---------|-------------|
| 0 | Full commit | Stop the loop. Run `memex lint`. |
| 3 | Plan needs re-review (some target hash mismatched) | Increment `rereview_count`. If > `MAX_REREVIEWS` (5), emit `re-review exhausted after 5 cycles; retry later` to stderr and exit 1. Otherwise: render the refreshed plan with `memex plan show < /tmp/memex-plan-<docid>.json`, surface new diffs, and re-collect the user's review via the same Path A / B / C choice as Step 3. On cancel: `rm -f` and exit 1. |
| 4 | Partial commit (some proposals failed) | Read `/tmp/memex-plan-<docid>.json` via the agent's file-read tool. Count proposals with `committed: true` and proposals with `error` populated. Surface a partial-commit summary to chat. Ask the user `retry` or `cancel`. On retry: re-run apply. On cancel: `rm -f` and exit 1. |
| anything else | Hard error | Paste daemon stderr to chat, `rm -f`, exit 1. |

The agent tracks `rereview_count` itself across iterations (in its own
reasoning), since shell variables don't persist across Bash calls.

### Step 5 — Lint

After a successful `rc=0` apply, run `memex lint` once to check for dangling
links and missing cross-references introduced by the new pages.

```bash
memex lint
```

## Page organization

Wiki pages anchor to **stable subjects**: a person, project, concept, tool,
place, or policy. The slug names the *subject*, not an event or date.

- Prefer fewer, larger pages. New details land as H2 sections on the subject's
  existing page. Split only when a sub-topic is substantial and self-contained
  enough to stand on its own across future sessions.
- Subject slugs, not episode slugs: `kubernetes` not `kubernetes-2026-04-meeting`,
  `auth-tokens` not `auth-tokens-redesign-v3`.
- The daemon's EXTRACT prompt enforces these rules; the skill does not need to
  reimplement them.

## Common mistakes — DO NOT

- **Do not** use `memex source add` + `memex write` directly. That bypasses
  the daemon's EXTRACT/MERGE pipeline and produces title-derived slugs that
  violate the worker prompt's subject-extraction rules. Always use the
  `source plan` + `plan show` + `plan apply` triple.
- **Do not** `cat` source content into chat. The bytes go from converter →
  `memex source add` stdin → daemon raw store, and never enter your context.
- **Do not** parse the plan JSON yourself. Use `memex plan show` for rendering
  and your agent's file-edit tool for single-field slug / dropped mutations.
- **Do not** use `mktemp` for the plan file. Each Bash call is its own
  subshell, so a `mktemp`-derived path can't be carried across calls without
  re-deriving it. The deterministic `/tmp/memex-plan-<docid>.json` path is
  recoverable from the docid, which the agent already has.
- **Do not** edit the proposal `body` field. Body editing is unsupported (the
  daemon doesn't validate); if you need to change content, edit the wiki page
  directly via `memex write` after the apply lands.

## Token efficiency

- Source content goes converter → daemon → worker model; never enters chat.
- Plan rendering is `memex plan show` output (table + diff blocks), not raw JSON.
- Plan mutations use your agent's file-edit tool against
  `/tmp/memex-plan-<docid>.json` (single-field swap, diff-only), not
  `jq` re-emission of the whole plan.
