#!/usr/bin/env bash
# Register memex hooks in whichever agent CLIs are installed.
#
# Auto-detects ~/.claude, ~/.codex, ~/.gemini and merges the appropriate
# hook JSON into each agent's settings file. Idempotent — running twice is
# the same as once.
#
# Requirements: bash, jq.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOKS_DIR="$REPO_ROOT/plugin/hooks"

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required. Install via 'brew install jq' (macOS) or your package manager." >&2
  exit 1
fi

if [ ! -d "$HOOKS_DIR" ]; then
  echo "error: hooks directory not found at $HOOKS_DIR" >&2
  exit 1
fi

# merge_hooks <target> <src> <event> <dedup_command>
# Merges hooks for a single event type from src into target, deduplicating
# by checking if any existing hook already contains the dedup_command.
merge_hooks() {
  local target="$1" src="$2" event="$3" dedup_cmd="$4"

  local entry
  entry="$(jq --arg ev "$event" '.hooks[$ev][0]' "$src")"
  [ "$entry" = "null" ] && return

  local merged
  merged="$(jq \
    --argjson entry "$entry" \
    --arg ev "$event" \
    --arg dedup "$dedup_cmd" \
    '.hooks //= {}
     | .hooks[$ev] //= []
     | if (.hooks[$ev] | any(.[].hooks[]?; .command | contains($dedup)))
       then .
       else .hooks[$ev] += [$entry]
       end' "$target")"
  printf '%s\n' "$merged" > "$target"
}

installed=0

# ---- Claude Code ----
if [ -d "$HOME/.claude" ]; then
  target="$HOME/.claude/hooks.json"
  src="$HOOKS_DIR/claude-code.json"
  [ -f "$target" ] || echo '{}' > "$target"

  merge_hooks "$target" "$src" "SessionStart" "memex daemon start"
  merge_hooks "$target" "$src" "SessionEnd" "memex ingest"
  echo "✓ Claude Code: registered hooks in $target"
  installed=$((installed + 1))
fi

# ---- Codex CLI ----
if [ -d "$HOME/.codex" ]; then
  target="$HOME/.codex/hooks.json"
  src="$HOOKS_DIR/codex.json"
  [ -f "$target" ] || echo '{}' > "$target"

  merge_hooks "$target" "$src" "SessionStart" "memex daemon start"
  merge_hooks "$target" "$src" "Stop" "memex ingest"
  echo "✓ Codex: registered hooks in $target"
  echo "  note: also set [features] codex_hooks = true in ~/.codex/config.toml"
  installed=$((installed + 1))
fi

# ---- Gemini CLI ----
if [ -d "$HOME/.gemini" ]; then
  target="$HOME/.gemini/settings.json"
  src="$HOOKS_DIR/gemini-cli.json"
  [ -f "$target" ] || echo '{}' > "$target"

  merge_hooks "$target" "$src" "SessionStart" "memex daemon start"
  merge_hooks "$target" "$src" "SessionEnd" "memex ingest"
  echo "✓ Gemini CLI: registered hooks in $target"
  installed=$((installed + 1))
fi

if [ "$installed" -eq 0 ]; then
  echo "no agent CLIs detected (looked for ~/.claude, ~/.codex, ~/.gemini)" >&2
  echo "install at least one of: Claude Code, Codex, Gemini CLI, then rerun" >&2
  exit 1
fi

echo
echo "done. Sessions will auto-ingest and daemon will pre-warm on session start."
