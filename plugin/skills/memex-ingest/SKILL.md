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

The skill is a thin orchestrator around three CLI subcommands:

1. `memex source add <url>` — store the source content in the raw store; daemon returns a docid.
2. `memex source plan <docid>` — daemon runs EXTRACT (chunked) + MERGE-dry-run for any overlapping wiki slugs; emits plan JSON to stdout.
3. `memex plan show < plan.json` — local renderer; produces a human-readable table + diff blocks.
4. `memex plan apply < plan.json` — daemon validates and writes each non-dropped proposal as a wiki page.

The skill owns its plan file (mktemp); the daemon is stateless on plan content.

## Flow

### Step 1: Acquire content

Use the right converter for the input:
- URLs: `markitdown <url>`
- Local PDFs / docs: `markitdown <path>`
- Plain text: skip the converter, pipe straight into source add

**Do NOT `cat` content into chat.** Pipe directly into `memex source add` so the
source bytes never enter your context. The daemon hashes content for dedup, so
re-adding the same source returns the same docid.

```bash
content_path=$(mktemp /tmp/memex-ingest-content-XXXXXX.md)
markitdown "$url" > "$content_path"
```

### Step 2: Store the source

```bash
docid=$(memex source add "$url" < "$content_path")
```

### Step 3: Generate the plan

```bash
plan_file=$(mktemp /tmp/memex-plan-XXXXXX.json)
trap 'rm -f "$content_path" "$plan_file"' EXIT
memex source plan "$docid" > "$plan_file"
```

Empty plan file (zero bytes) means EXTRACT yielded no extractable subjects;
tell the user and exit cleanly. Invalid JSON (`jq -e . "$plan_file"` fails)
means the daemon connection dropped mid-stream; abort with `exit 1`.

### Step 4: Render and review

```bash
memex plan show < "$plan_file"
```

Paste the output to chat. Then collect edits via AskUserQuestion using these
heuristics by proposal count:

- **1–5 proposals:** Ask per-proposal — `rename slug`, `drop`, `accept`.
- **6–20 proposals:** Ask only about proposals you flag as suspect (slug
  contains a date like `2026-04`, a version qualifier like `v3`, an episode
  word like `milestone`/`session`, or duplicates an obvious subject already
  in the wiki). Other proposals are accepted by default.
- **>20 proposals:** Editor-handoff. Tell the user `the plan is at <path>; open
  in your editor, edit `slug`, `title`, or set `dropped: true`, then say
  "apply"`. Wait for confirmation.

To apply slug or dropped edits, use the **Edit tool** against `$plan_file`
(diff-only — never re-emit the whole plan). Title edits go through the editor
handoff regardless of proposal count.

### Step 5: Apply (with bounded re-review loop)

```bash
rereview_count=0
MAX_REREVIEWS=5
while true; do
  out=$(mktemp /tmp/memex-apply-XXXXXX.json)
  memex plan apply < "$plan_file" > "$out"; rc=$?
  case $rc in
    0)
      cat "$out"   # surface "committed N wiki pages" to chat
      rm -f "$out" "$plan_file"
      break
      ;;
    3)
      rereview_count=$((rereview_count + 1))
      if [ "$rereview_count" -gt "$MAX_REREVIEWS" ]; then
        echo "re-review exhausted after $MAX_REREVIEWS cycles; retry later" >&2
        rm -f "$out"
        exit 1
      fi
      mv "$out" "$plan_file"   # daemon emitted refreshed plan
      memex plan show < "$plan_file"
      # AskUserQuestion: accept new diffs or abort. On abort: exit 1.
      ;;
    4)
      mv "$out" "$plan_file"   # daemon emitted plan with committed/error fields
      # Read $plan_file via Read tool, count `committed: true` and `error`
      # entries, surface a partial-commit summary to chat.
      # AskUserQuestion: retry or abort. On abort: exit 1.
      ;;
    *)
      rm -f "$out"
      exit 1
      ;;
  esac
done
memex lint
```

## Page organization

Wiki pages anchor to **stable subjects**: a person, project, concept, tool,
place, or policy. The slug names the *subject*, not an event or date.

- Prefer fewer, larger pages. New details land as H2 sections on the subject's
  existing page. Split only when a sub-topic is substantial and self-contained
  enough to stand on its own across future sessions.
- Subject slugs, not episode slugs: `caroline` not `caroline-2026-04-meeting`,
  `mmai` not `mmai-design-milestone-3`.
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
  and the **Edit tool** for mutations.
- **Do not** keep `memex source plan` output in chat as a JSON blob — pipe it
  to a temp file (`mktemp`) and only paste what `plan show` surfaces.
- **Do not** edit the proposal `body` field. Body editing is unsupported (the
  daemon doesn't validate); if you need to change content, edit the wiki page
  directly via `memex write` after the apply lands.

## Token efficiency

- Source content goes converter → daemon → worker model; never enters chat.
- Plan rendering is `memex plan show` output (table + diff blocks), not raw JSON.
- Plan mutations use the **Edit tool** against the temp file (diff-only),
  not `jq` re-emission of the whole plan.

## Cross-references

- Spec: `docs/specs/2026-04-30-memex-ingest-skill-redesign.md`
- Implementation plan: `docs/superpowers/plans/2026-04-30-memex-ingest-skill-redesign.md`
- Related skill: `memex-query` (for retrieval after ingestion)
