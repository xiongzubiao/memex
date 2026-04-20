---
name: memex-query
description: |
  Use when the user asks a question that might be answered by their personal wiki,
  references past decisions, or says "check the wiki", "search memex", or "what do
  we know about X".
---

# memex-query — Search and synthesize from personal wiki

## Overview

Query the user's memex wiki for answers. The `memex query` command handles
the full pipeline: retrieval, expansion on weak signal, and synthesis with
citations.

## Pipeline

1. `memex query "<user question>"` — returns a synthesized answer with `[[page-stem]]` citations
2. If no results: tell the user their wiki doesn't have this information

For raw retrieval without synthesis (no LLM call):
- `memex query --raw "<question>"` — returns ranked pages with metadata

## Common Mistakes

- Running `memex search` instead of `memex query` — `search` is for title lookup, `query` does full retrieval + synthesis
- Not including `[[page-stem]]` citations when synthesizing manually
- Forgetting `--raw` when you just need to check what pages exist
