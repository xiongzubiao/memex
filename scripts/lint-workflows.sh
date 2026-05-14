#!/usr/bin/env bash
# scripts/lint-workflows.sh — actionlint over .github/workflows/.
#
# Downloads a pinned actionlint to ~/.cache/memex-tools on first run so devs
# don't need to install anything. Idempotent — fast on every subsequent run.
#
# Usage:
#   scripts/lint-workflows.sh
#   scripts/lint-workflows.sh --force   # re-download actionlint even if cached
#
# Hook it up as a pre-commit step:
#   ln -s ../../scripts/lint-workflows.sh .git/hooks/pre-commit
set -euo pipefail

VERSION="1.7.7"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/memex-tools"
BIN="$CACHE_DIR/actionlint-$VERSION"

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64)   ASSET="actionlint_${VERSION}_linux_amd64.tar.gz" ;;
  Linux/aarch64)  ASSET="actionlint_${VERSION}_linux_arm64.tar.gz" ;;
  Darwin/x86_64)  ASSET="actionlint_${VERSION}_darwin_amd64.tar.gz" ;;
  Darwin/arm64)   ASSET="actionlint_${VERSION}_darwin_arm64.tar.gz" ;;
  *) echo "lint-workflows: unsupported $(uname -s)/$(uname -m)" >&2; exit 1 ;;
esac

if [ "${1:-}" = "--force" ] || [ ! -x "$BIN" ]; then
  mkdir -p "$CACHE_DIR"
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL "https://github.com/rhysd/actionlint/releases/download/v${VERSION}/${ASSET}" -o "$tmp/actionlint.tgz"
  tar xzf "$tmp/actionlint.tgz" -C "$tmp" actionlint
  mv "$tmp/actionlint" "$BIN"
fi

# Only lint workflow YAMLs that are actually staged for commit (when used as
# a pre-commit hook); fall back to all workflows for a manual run.
FILES=()
if git rev-parse --git-dir >/dev/null 2>&1 && [ -n "${GIT_DIR:-}" ]; then
  while IFS= read -r f; do
    case "$f" in .github/workflows/*.yml|.github/workflows/*.yaml) FILES+=("$f");; esac
  done < <(git diff --cached --name-only --diff-filter=ACMR)
fi
if [ "${#FILES[@]}" -eq 0 ]; then
  shopt -s nullglob
  FILES=(.github/workflows/*.yml .github/workflows/*.yaml)
fi
[ "${#FILES[@]}" -eq 0 ] && exit 0

exec "$BIN" "${FILES[@]}"
