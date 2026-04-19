#!/usr/bin/env bash
# Register the memex SessionStart hook in whichever agent CLIs are installed.
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

installed=0

# ---- Claude Code ----
if [ -d "$HOME/.claude" ]; then
  target="$HOME/.claude/hooks.json"
  src="$HOOKS_DIR/claude-code.json"
  [ -f "$target" ] || echo '{}' > "$target"

  memex_entry="$(jq '.hooks.SessionStart[0]' "$src")"

  merged="$(jq \
    --argjson entry "$memex_entry" \
    '.hooks //= {}
     | .hooks.SessionStart //= []
     | if (.hooks.SessionStart | any(.[].hooks[]?; .command == "memex daemon start"))
       then .
       else .hooks.SessionStart += [$entry]
       end' "$target")"
  printf '%s\n' "$merged" > "$target"
  echo "✓ Claude Code: registered SessionStart hook in $target"
  installed=$((installed + 1))
fi

# ---- Codex CLI ----
if [ -d "$HOME/.codex" ]; then
  target="$HOME/.codex/hooks.json"
  src="$HOOKS_DIR/codex.json"
  [ -f "$target" ] || echo '{}' > "$target"

  memex_entry="$(jq '.hooks.SessionStart[0]' "$src")"

  merged="$(jq \
    --argjson entry "$memex_entry" \
    '.hooks //= {}
     | .hooks.SessionStart //= []
     | if (.hooks.SessionStart | any(.[].hooks[]?; .command == "memex daemon start"))
       then .
       else .hooks.SessionStart += [$entry]
       end' "$target")"
  printf '%s\n' "$merged" > "$target"
  echo "✓ Codex CLI: registered SessionStart hook in $target"
  echo "  note: also set [features] codex_hooks = true in ~/.codex/config.toml"
  installed=$((installed + 1))
fi

# ---- Gemini CLI ----
if [ -d "$HOME/.gemini" ]; then
  target="$HOME/.gemini/settings.json"
  src="$HOOKS_DIR/gemini.json"
  [ -f "$target" ] || echo '{}' > "$target"

  memex_entry="$(jq '.hooks.SessionStart[0]' "$src")"

  merged="$(jq \
    --argjson entry "$memex_entry" \
    '.hooks //= {}
     | .hooks.SessionStart //= []
     | if (.hooks.SessionStart | any(.[].hooks[]?; .command == "memex daemon start" or (.name == "memex-daemon")))
       then .
       else .hooks.SessionStart += [$entry]
       end' "$target")"
  printf '%s\n' "$merged" > "$target"
  echo "✓ Gemini CLI: registered SessionStart hook in $target"
  installed=$((installed + 1))
fi

if [ "$installed" -eq 0 ]; then
  echo "no agent CLIs detected (looked for ~/.claude, ~/.codex, ~/.gemini)" >&2
  echo "install at least one of: Claude Code, Codex CLI, Gemini CLI, then rerun" >&2
  exit 1
fi

echo
echo "done. memex daemon will pre-warm when you open a new agent session."
