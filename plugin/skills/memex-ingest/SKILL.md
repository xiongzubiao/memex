---
name: memex-ingest
description: |
  Use when the user provides a file, document, or text to add to their wiki, or says
  "ingest this", "add to wiki", "read this and create pages", or shares material for
  knowledge capture.
---

# memex-ingest — Interactive source ingestion

## Overview

Turn source material into wiki pages. Read the source, propose pages, check for
duplicates, write with `--source` to keep the original searchable.

## Pipeline

1. Read source file using Read tool
2. Summarize what the source contains
3. Ask user: "I see content about X, Y, Z. What wiki pages should I create?"
4. **Duplicate check** for each page: `memex search "<title>"`
   - wiki match found → read existing page, ask user: merge or create new?
   - source-only matches are not duplicates
   - no match → create new page
5. Write each page: `echo "<content>" | memex write "Title" --quiet --source /path`
   Use `--force` when merging into an existing page.
6. Run `memex lint` for wiki-wide health check
7. Report: pages created, lint findings

## Quick Reference

| Flag | Purpose |
|------|---------|
| `--quiet` | Suppress per-page link details during bulk writes |
| `--source /path` | Attach original source file for search coverage |
| `--force` | Overwrite existing page (use after duplicate confirmation) |

## Wiki Page Format

Pages require YAML frontmatter with these fields:

```yaml
---
title: Page Title Here
summary: One-line summary (optional but recommended)
tags: [tag1, tag2]
created_at: 2026-04-15T00:00:00Z
updated_at: 2026-04-15T00:00:00Z
sources: []
---
```

- `sources` — list absolute paths to source files (e.g., `[/home/user/notes.txt]`). Matches the `--source` flag paths.
- Wiki links use **kebab-case slugs**: `[[memmachine-architecture]]` not `[[MemMachine Architecture]]`.
  The slug matches the filename without `.md` (e.g., `wiki/memmachine-architecture.md`).

## Common Mistakes

- Writing without `--source` — the original document won't be searchable
- Skipping the duplicate check — creates redundant wiki pages
- Using `--force` without asking the user first — overwrites silently
- Not running `memex lint` after batch writes — misses dangling links
- Treating source-collection search hits as duplicates — only wiki hits matter
- Using title-case in wiki links — `[[My Page]]` creates dangling links; use `[[my-page]]`
- Missing frontmatter fields — `title`, `tags`, `created_at`, `updated_at` are all required
