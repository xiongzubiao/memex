---
name: memex
description: "Auto-ingest the previous OpenClaw session into your memex wiki on /new or /reset"
metadata:
  {
    "openclaw":
      {
        "emoji": "📦",
        "events": ["command:new", "command:reset"],
        "requires": { "config": ["workspace.dir"] },
        "install": [{ "id": "memex", "kind": "plugin", "label": "Linked from memex install" }]
      }
  }
---

# Memex OpenClaw Hook

Auto-ingests the previous OpenClaw session transcript into your memex wiki when you run `/new` or `/reset`. Mirrors the session-memory hook's wake-up event so the chain captures every conversation as it rolls over.

## Pipeline

1. Fires on `command:new` or `command:reset`.
2. Reads the previous session file from `event.context.previousSessionEntry`.
3. Strips the `.reset.<n>.jsonl` suffix if present, recovering the base transcript path.
4. Spawns `memex ingest --agent openclaw <path>` detached so `/new` / `/reset` returns immediately.

## Requirements

- `memex` on `$PATH` (install with `npm install -g @xiongzubiao/memex@rc`).
- The memex daemon will be auto-spawned by the `ingest` invocation if it isn't already running.
- `workspace.dir` must be set in your OpenClaw config (the default `openclaw configure` flow sets this).

## Disabling

```bash
openclaw hooks disable memex
```

Or remove the plugin entirely:

```bash
openclaw plugins uninstall memex
```
