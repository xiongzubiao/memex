# Memex SessionStart hooks

Pre-warms the memex daemon when an agent session starts. The hook runs
`memex daemon start`, which exits 0 cleanly if a daemon is already running
(single-instance enforced by the flock at `~/.memex/daemon.lock`), so
repeat invocations are safe.

## Files

- `claude-code.json` — merge into `~/.claude/hooks.json` (or a plugin's
  hooks declaration).
- `codex.json` — merge into `~/.codex/hooks.json`. Also requires setting
  `[features] codex_hooks = true` in `~/.codex/config.toml` (default off).
- `gemini.json` — merge into `~/.gemini/settings.json` under the `hooks`
  key.

## Automatic install

The easier way: run `scripts/install-hooks.sh` from the repo root. It
auto-detects which of the three agent CLIs you have installed and merges
the right hook into each. Idempotent — running twice is the same as once.

## Manual install

Copy the relevant JSON into the location above. If the file already has a
`hooks.SessionStart` array, append (don't replace) — memex's hook should
coexist with any existing hooks.
