# Memex Agent Instructions

Cross-platform behavioral instructions for any AI agent using the memex plugin.

## Available Skills

- `/memex-query` — Search personal wiki and synthesize answers
- `/memex-ingest` — Read source material and create wiki pages
- `/memex-brainstorm` — Multi-LLM brainstorming via external CLIs
- `memex backfill <agent>` — CLI command to batch-import existing agent sessions via daemon

## Proactive Behavior

These triggers are opt-in — always ask the user before acting.

### Trigger 1: New knowledge produced

When the conversation produces wiki-worthy knowledge (design decisions, debugging insights, discovered patterns), prompt:

> "This looks like useful knowledge for your memex. Want me to add a wiki page about [topic]?"

If yes, draft content and pipe to `memex write`.

### Trigger 2: Stale information detected

When wiki content contradicts the current conversation, compare `updated_at` timestamps and prompt:

> "Your wiki page [title] (last updated [date]) seems outdated — it says X but we're seeing Y. Want me to update it?"

If yes, update via `memex write --force`.

### Trigger 3: Wiki health check

When the user asks about wiki health, issues, or maintenance, run `memex lint` and report findings.

## Session Knowledge Capture

When you discover something significant during this session — a design decision,
a debugging finding, an architectural insight, or a "we tried X and it failed
because Y" — capture it to memex.

Search first: `memex search <topic>`. If a slug is printed, a page exists.
Read it with `memex read <slug>`, then overwrite with updated content via
`memex write --force <slug>` (pipe updated content via stdin).
If no output, create a new page: `memex write <page-name>`.

Do not capture mechanical operations (file reads, test runs, greps).
Capture knowledge that would be valuable if this topic comes up again.

This serves as compaction recovery — if your context gets compacted, query
memex to recover earlier findings you captured.
