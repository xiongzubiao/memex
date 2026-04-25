---
name: memex-ingest
description: |
  Use when the user provides a URL, file, document, or text to add to their wiki,
  or says "ingest this", "add to wiki", "read this and create pages", or shares
  material for knowledge capture. The skill keeps the user in the loop: propose
  page list, get approval, write. For non-interactive ingest (hooks, scripts),
  use `memex ingest --source <id>` directly instead.
---

# memex-ingest — interactive source ingestion

This skill turns one source (URL, document, text blob) into one or more wiki
pages with the user reviewing the proposed pages before anything is written.
For agent session transcripts (Claude Code / Codex / Gemini CLI `.jsonl`
files), use `memex backfill <agent>` instead.

**Argument:** the source URL or filesystem path. Examples:
`/memex-ingest https://example.com/article`,
`/memex-ingest ~/Downloads/spec.pdf`.

## Page organization

Wiki pages anchor to **stable subjects**: a person, project, concept, tool,
place, or policy. The slug names the *subject*, not an event or date.

- Prefer fewer, larger pages. New details land as H2 sections on the subject's
  existing page. Split only when a sub-topic is substantial and self-contained
  enough to stand on its own across future sessions.
- Subject slugs, not episode slugs: `caroline` not
  `caroline-stained-glass-church`; `oauth-migration` not
  `oauth-token-bug-tuesday`.
- Preserve specific facts: dates, times, numbers, names, places, direct quotes.
  These are the retrieval handles — losing them is a failure.
- Cross-link `[[other-slug]]` only to pages you know exist (just written, or
  confirmed via `memex search` / `memex read`). Prefer plain text over a
  guessed slug.

**Worked example.** A meeting-notes file covering an OAuth migration, Alice's
promotion, and a new testing strategy decided at a 2026-03 offsite:

```
bad:  team-offsite-2026-03, alice-promotion-2026-03,
      oauth-migration-offsite-notes, testing-strategy-offsite
good: oauth-migration  (+ H2 "Offsite 2026-03 planning")
      alice            (+ H2 "Promoted to senior, 2026-03")
      testing-strategy (+ H2 "2026-03 revision")
```

The event becomes a date *inside* each subject page, not its own anchor.

## Step 1: acquire the content

> **Required setup before first use:**
>
> ```
> uv tool install 'markitdown[all]'
> ```
>
> The `[all]` extras are load-bearing. Bare `markitdown` rejects PDFs,
> DOCX, audio, etc. with `MissingDependencyException`. Install `uv` from
> https://github.com/astral-sh/uv if you don't already have it — it
> avoids PEP 668 issues on Debian/Ubuntu and is faster than pipx. For
> `agent-browser`, install per its own docs. Memex itself does not
> fetch or convert.

Pick the tool based on what the source is:

- **Local plain text** (`.md`, `.txt`, `.rst`, `.org`) — read with the `Read`
  tool directly. No conversion needed.
- **Local binary doc** (`.pdf`, `.docx`, `.pptx`, `.xlsx`, `.html`, audio) —
  `markitdown <path>`. One tool covers all these formats.
- **Static URL** (blog, docs, news, public README) — `markitdown <url>`.
  Single HTTP request. Handles HTML, PDF, DOCX, audio, YouTube transcripts.
- **JS-rendered URL** (SPA, dashboard, app shell) — `agent-browser` then
  `markitdown -x html`. Static fetch returns a skeleton, need a real
  browser to render.
- **Authenticated / paywalled URL** — `agent-browser` after running
  `/setup-browser-cookies`, then `markitdown -x html`.

### Decision flow for URLs

```
1. content=$(markitdown "$arg")        # try static fetch first
2. inspect $content
3. is it the article body?              ──yes──> use it
   ↓ no (skeleton, "Enable JavaScript", login wall, mostly chrome)
4. fall back to agent-browser
```

**Verified agent-browser invocation** (these are the actual commands, run
`agent-browser --help` to see the full surface):

```
agent-browser open "$arg"
agent-browser wait 2000                              # let JS render
content=$(agent-browser get html body | markitdown -x html)
final_url=$(agent-browser get url)
title=$(agent-browser get title)
agent-browser close
```

If `agent-browser` isn't on PATH, tell the user the page appears JS-rendered
and recommend installing it (or pasting rendered content manually).

### Detect login redirects

A successful render can still land on a login page. Inspect `final_url` and
`title`:

- URL changed to a known SSO host (`id.atlassian.com`, `accounts.google.com`,
  `login.microsoftonline.com`, `github.com/login`, etc.) → login redirect.
- Title or first lines of `content` contain "Log in", "Sign in", "Log in to
  continue" → login wall.

On a detected login wall, **stop and tell the user**:

> "The page is behind authentication. Import your browser's session cookies
> with `/setup-browser-cookies` (pick the relevant domain), then re-run
> `/memex-ingest <url>`."

Do not write a login-page-as-content into memex.

### Always propagate converter failures

Use `set -o pipefail`, or check exit codes explicitly. If both tools fail or
the result is empty, stop and report what failed — do not proceed with empty
content. (The daemon will reject empty stdin with a clear error anyway, but
catching it here saves a round trip and gives a more pointed message.)

## Step 2: store the source, get a docid

Once `$content` and `$arg` (the source identifier) are set, store the source
once:

```
src=$(printf '%s' "$content" | memex source add "$arg")
echo "stored as docid $src"
```

`memex source add` is idempotent on content hash — re-running with the same
content returns the same docid without duplicate storage. Add `--collection
<name>` (repeatable) to scope the source to one or more collections.

Verify with `memex source list` (or `memex source show "$src"`).

## Step 3: propose pages, get approval

Identify the stable subjects in `$content`. Draft a list:

```
- <slug-1> — one-line rationale for why this is a separate page
- <slug-2> — ...
```

Present it to the user. Wait for approval or adjustments. The user might say
"merge slug-1 and slug-2", or "skip slug-3, those facts go on the existing
`alice` page". Apply their feedback before writing.

## Step 4: write each approved page

For each page, dedup against existing wiki content first:

```
# Does a page with this slug or close title already exist?
memex search "<title>"
```

- **Slug printed** → an existing page covers this subject. Read it via
  `memex read <slug>`, confirm with the user that the new content should be
  merged, then write with `--force`:
  ```
  memex write "<title>" --force --source "$src" --quiet < <merged-body>
  ```
- **No output** → it's new. Write without `--force`:
  ```
  memex write "<title>" --source "$src" --quiet < <body>
  ```

The `--source "$src"` flag attaches the source's path identifier to the
page's `sources:` frontmatter list, so `memex source delete` and link
audits can find the reference.

When merging, **preserve every specific fact from both sides**. Extend
existing H2 sections or add new ones. The merged body is the union, not the
intersection.

## Step 5: verify

```
memex lint
```

Catches dangling `[[wiki-links]]`, missing cross-references, untracked files
(disk but no DB row), missing files (DB row but no disk file), and outdated
embeddings. Report findings to the user.

## Frontmatter the wiki page should have

```yaml
---
title: Page Title Here
summary: One-line summary (optional)
tags: [tag1, tag2]
created_at: 2026-04-21T00:00:00Z
updated_at: 2026-04-21T00:00:00Z
sources:
  - https://example.com/article
---
```

Wiki links are kebab-case, matching the filename without `.md`:
`[[oauth-migration]]`, not `[[OAuth Migration]]`.

`memex write --source "$src"` injects the `sources:` entry automatically; you
don't need to put it in the body you pipe in.

## Common mistakes

- **Episode-based slugs** (`alice-promotion-2026-03`) — use `alice` plus an H2
  `## Promoted to senior, 2026-03` instead.
- **A new page per anecdote** — extend the subject page.
- **Dropping dates / names / numbers / quotes during merge.** These are the
  retrieval handles. Treat them as load-bearing.
- **Inventing `[[slug]]` targets** without verifying they exist.
- **Skipping the dedup check**, or `--force`-ing without asking.
- **Piping content to `memex ingest --source`** instead of using the
  `source add` + `write` flow above. That bypasses the propose/approve loop
  this skill exists for.
- **Writing a login-page render as if it were the article.** Detect the
  redirect, stop, ask the user to import cookies.
