---
name: memex-query
description: |
  Use when the user asks a question that might be answered by their personal wiki,
  references past decisions, or says "check the wiki", "search memex", or "what do
  we know about X".
---

# memex-query — Search and synthesize from personal wiki

## Overview

Retrieve knowledge from the user's memex wiki using a probe-then-expand search pattern.
Core principle: probe first with no flags, only expand if the signal is weak.

## Pipeline

1. **Probe**: `memex search "<user question>"`
2. **If signal: strong** → skip to step 5
3. **If signal: weak** → generate typed expansions:
   - `--lex` keyword variants (exact terms the wiki might use)
   - `--vec` semantic reformulations
   - `--hyde` hypothetical document that answers the question
4. **Expanded search**: `memex search "<question>" --lex "..." --vec "..." --hyde "..."`
5. **Read**: `memex read <top docids>`
6. **Synthesize**: answer with citations using `[[page-stem]]` links

## Quick Reference

| Flag | When | What it does |
|------|------|--------------|
| (none) | Always first | Probe: BM25 + vector, returns signal line |
| `--lex` | Weak signal | BM25-only keyword expansion |
| `--vec` | Weak signal | Vector-only semantic expansion |
| `--hyde` | Weak signal | Vector-only hypothetical doc expansion |

## Common Mistakes

- Skipping the probe and going straight to `--lex`/`--vec` — always probe first
- Using the numeric score to judge relevance — use signal, collection, and stem instead
- Expanding when signal is strong — wastes tokens, doesn't improve results
- Not reading wiki pages before answering — search snippets are summaries, not full content
- Forgetting `[[page-stem]]` citations in the synthesized answer

## Rules

- Always start with a probe call (no flags)
- Only expand if signal is weak
- Read wiki pages for structured answers, source documents for detail
- If no results after expansion, tell the user their wiki doesn't have this information
