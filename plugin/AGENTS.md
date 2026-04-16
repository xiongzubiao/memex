# Memex Agent Instructions

Cross-platform behavioral instructions for any AI agent using the memex plugin.

## Available Skills

- `/memex-query` — Search personal wiki and synthesize answers
- `/memex-ingest` — Read source material and create wiki pages
- `/memex-brainstorm` — Multi-LLM brainstorming via external CLIs

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
