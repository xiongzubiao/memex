# Memex hooks

Per-agent hook templates used by both the published marketplace plugin and the
`memex install` subcommand.

## Files

- `hooks.json` — Claude Code marketplace plugin format (uses `${CLAUDE_PLUGIN_ROOT}` semantics; Claude Code reads this when the plugin is registered via the marketplace).
- `claude-code.json` — merge into `~/.claude/hooks.json` (used by `memex install`).
- `codex.json` — merge into `~/.codex/hooks.json`. Also requires `[features] codex_hooks = true` in `~/.codex/config.toml` (default off).
- `gemini-cli.json` — merge into `~/.gemini/settings.json` under the `hooks` key.

## Install

Recommended:

```bash
memex install
```

Auto-detects which agent CLIs you have installed and merges hooks (and Claude
Code skills) into each. Idempotent — any prior memex entries are replaced with
the current template before fresh entries are appended.

## Manual install

Copy the relevant JSON into the location above. If the file already has hook
arrays for the same events, append rather than replace.
