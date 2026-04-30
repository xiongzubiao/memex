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

1. `memex query --intent "<short context>" "<user question>"` — retrieval + synthesis with `[[page-stem]]` citations.
2. If no results: tell the user their wiki doesn't have this information.

**Always provide `--intent`** on every query to disambiguate the question and improve snippet selection. Intent is short context — what the user actually means — not a second search term.

Correct:

```
memex query --intent "web page load times" "performance optimizations"
```

The user says "performance" but means front-end. Intent narrows the sense.

Incorrect:

```
memex query --intent "performance optimizations to consider" "performance"
```

That just restates the query as intent. The system treats intent as a weighted re-ranking signal, not as another search term — restating washes out the boost.

For raw retrieval without synthesis (no LLM call):

```
memex query --intent "<context>" --raw "<question>"
```

Collection-scoped retrieval:

```
memex query --intent "<context>" "<question>" --collection team-a --collection incidents
```

With no `--collection`, query defaults to the `default` collection.

## Common Mistakes

- Running `memex search` instead of `memex query` — `search` is for title lookup, `query` does full retrieval + synthesis.
- Omitting `--intent`. Without it, ambiguous queries miss the narrowing signal.
- Restating the query as intent — pointless, washes out the boost.
- Not including `[[page-stem]]` citations when synthesizing manually.
- Forgetting `--raw` when you just need to check what pages exist.
