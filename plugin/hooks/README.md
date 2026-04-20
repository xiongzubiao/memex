# Memex hooks

Per-agent hook files for session ingestion (SessionEnd) and daemon
pre-warming (SessionStart).

## Files

- `claude-code.json` — merge into `~/.claude/hooks.json`.
- `codex.json` — merge into `~/.codex/hooks.json`. Also requires setting
  `[features] codex_hooks = true` in `~/.codex/config.toml` (default off).
- `gemini-cli.json` — merge into `~/.gemini/settings.json` under the
  `hooks` key.

## Automatic install

Run `scripts/install-hooks.sh` from the repo root. It auto-detects which
agent CLIs you have installed and merges both hooks into each.
Idempotent.

## Manual install

Copy the relevant JSON into the location above. If the file already has
hook arrays for the same events, append rather than replace.
