---
name: memex-ingest
description: |
  Use when the user provides a file, document, or text to add to their wiki, or says
  "ingest this", "add to wiki", "read this and create pages", or shares material for
  knowledge capture.
---

# memex-ingest — Interactive source ingestion

Turn a source document into wiki pages with the user in the loop: propose, get
approval, write. For session transcripts, use `memex backfill` instead (runs
through the daemon silently). This skill is for everything else — notes,
articles, meeting minutes, research docs.

**Arguments**: first positional is the source file path. Optional
`--source <path>` overrides the provenance path recorded on each page
(useful when the input file is a copy).

## Page organization

Wiki pages anchor to **stable subjects** — a person, project, concept,
tool, place, or policy. The slug names the subject, not an event or date.

- Prefer fewer, larger pages; capture new details as H2 sections on the
  subject's existing page. Split only when a sub-topic is substantial and
  self-contained enough to stand alone across future sessions.
- Subject slugs, not episode slugs: `caroline` not `caroline-stained-glass-church`;
  `oauth-migration` not `oauth-token-bug-tuesday`.
- Preserve specific facts — dates, times, numbers, names, places, direct
  quotes. These are the retrieval handles; losing them is a failure.
- Cross-link (`[[other-slug]]`) only to pages you know exist (just
  written, or confirmed via `memex search`/`memex read`). Prefer plain
  text over a guessed slug.

**Example.** A file of notes covering an OAuth migration, Alice's
promotion, and a new testing strategy — decided at a 2026-03 offsite:

```
bad:  team-offsite-2026-03, alice-promotion-2026-03,
      oauth-migration-offsite-notes, testing-strategy-offsite
good: oauth-migration (+ H2 "Offsite 2026-03 planning"),
      alice (+ H2 "Promoted to senior, 2026-03"),
      testing-strategy (+ H2 "2026-03 revision")
```

The event becomes a date inside each subject page, not its own anchor.

## Pipeline

1. `Read` the source file.
2. Identify stable subjects in the content (people, projects, concepts,
   tools, places). Draft a page list: `<slug> — <rationale>`.
3. Present the list to the user. Wait for approval or adjustments.
4. For each approved page, run `memex search "<title>"` to dedup:
   - slug printed → `memex read <slug>`, confirm merge with user.
   - no output → create new.
5. Write:
   - new: `echo "<content>" | memex write "Title" --quiet --source <path>`
   - merge: `echo "<merged-content>" | memex write "Title" --force --quiet --source <path>`
   - On merge, preserve every specific fact from both sides. Extend
     existing H2 sections or add new ones.
6. Run `memex lint` and report created/updated pages plus findings.

## Session collections

For daemon-backed session ingestion:

- `memex ingest --agent codex --collection team-a --collection incidents`
- `memex backfill codex --collection team-a --collection incidents`

When no `--collection` is provided, ingest/backfill assign documents to
`default`.

## Frontmatter

```yaml
---
title: Page Title Here
summary: One-line summary (optional)
tags: [tag1, tag2]
created_at: 2026-04-21T00:00:00Z
updated_at: 2026-04-21T00:00:00Z
sources: [/abs/path/to/source]
---
```

Wiki links are kebab-case, matching the filename without `.md`:
`[[oauth-migration]]`, not `[[OAuth Migration]]`.

## Common mistakes

- Episode-based slugs (`alice-promotion-2026-03`) — use `alice` + an H2.
- A new page per anecdote — extend the subject page instead.
- Dropping dates/names/numbers/quotes during merge.
- Inventing `[[slug]]` targets without verifying they exist.
- Skipping the dedup check, or `--force`-ing without asking.
