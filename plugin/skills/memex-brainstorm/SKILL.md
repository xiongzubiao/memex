---
name: memex-brainstorm
description: |
  Use when the user wants to brainstorm, explore design approaches, or get multiple
  perspectives on a topic. Triggers: "brainstorm", "explore approaches", "what are
  the options", "get different perspectives".
---

# memex-brainstorm — Multi-LLM brainstorming

## Overview

Generate and refine ideas by combining proposals from multiple LLM CLIs (if available),
then save the result to the wiki. Falls back to varied-angle single-model brainstorming.

## Discovery

Check available CLIs: `which claude codex gemini 2>/dev/null` (binary names for Claude Code, Codex, Gemini CLI)

## Pipeline

1. **Propose** (parallel): host agent + each available CLI generate proposals
   - `codex --quiet "Propose 3 approaches for: <topic>"`
   - `gemini "Propose 3 approaches for: <topic>"` (Gemini CLI)
   - Host agent also proposes
2. **Merge**: host agent synthesizes all proposals
3. **Review** (parallel): external CLIs critique the merged design
   - `codex --quiet "Review for weaknesses: <merged>"`
   - `gemini "Review for weaknesses: <merged>"` (Gemini CLI)
4. **Iterate**: incorporate feedback, repeat until converged
5. **Write**: `echo "<result>" | memex write "Design Topic"`

## Fallback

If only one CLI is available: single-model brainstorm with varied prompting angles
(optimist, critic, lateral thinker).

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

- `sources` — list absolute paths to source files if applicable.
- Wiki links use **kebab-case slugs**: `[[my-page]]` not `[[My Page]]`.

## Common Mistakes

- Running only one round of propose/review — iterate until feedback converges
- Not writing the result to the wiki — brainstorm output is lost
- Skipping the review step — unreviewed proposals miss blind spots
- Using brainstorm for factual questions — use memex-query instead
- Using title-case in wiki links — `[[My Page]]` creates dangling links; use `[[my-page]]`
- Missing frontmatter fields — `title`, `tags`, `created_at`, `updated_at` are all required
