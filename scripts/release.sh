#!/usr/bin/env bash
# scripts/release.sh — bump version, tag, push. Operator-facing.
#
# Usage: scripts/release.sh [patch|minor|major|prepatch|preminor|premajor|prerelease]
#
# Prerelease forms append `-rc.N` (preid=rc). Use `prepatch` for the first
# rc on a new patch line (e.g. 0.1.0 → 0.1.1-rc.0); use `prerelease` to
# advance the rc counter (0.1.1-rc.0 → 0.1.1-rc.1).
set -euo pipefail

BUMP="${1:-patch}"
case "$BUMP" in
  patch|minor|major|prepatch|preminor|premajor|prerelease) ;;
  *) echo "usage: $0 [patch|minor|major|prepatch|preminor|premajor|prerelease]" >&2; exit 1;;
esac

# Pre-flight.
# Ignore untracked files in the working-tree check — they're usually local
# WIP that shouldn't block a release. Modifications and staged changes still
# block.
[ -z "$(git status --porcelain --untracked-files=no)" ] || { echo "tree not clean (uncommitted changes)" >&2; exit 1; }
BRANCH=$(git rev-parse --abbrev-ref HEAD)
# Release from either main or the publish-fix branch (v0.1.x history lives there).
case "$BRANCH" in main|plugin-publish-fix) ;; *) echo "release must run from main or plugin-publish-fix, got $BRANCH" >&2; exit 1;; esac
git fetch "origin" "$BRANCH" >/dev/null
[ "$(git rev-parse HEAD)" = "$(git rev-parse "origin/$BRANCH")" ] || { echo "$BRANCH is behind origin" >&2; exit 1; }

# Bump version in plugin/package.json (canonical source).
NPM_VERSION_ARGS=("$BUMP")
case "$BUMP" in pre*) NPM_VERSION_ARGS+=("--preid=rc");; esac
( cd plugin && npm version "${NPM_VERSION_ARGS[@]}" --no-git-tag-version >/dev/null )
NEW=$(node -p "require('./plugin/package.json').version")

# Mirror the bump into core/Cargo.toml and cli/Cargo.toml so `memex --version`
# reports the right number. All three files must stay in lockstep.
sed -i.bak -E "s|^version = \"[^\"]+\"|version = \"${NEW}\"|" core/Cargo.toml cli/Cargo.toml
rm -f core/Cargo.toml.bak cli/Cargo.toml.bak
# Cargo rejects prereleases against caret/tilde ranges (`^0.1` won't match
# `0.1.1-rc.0`), so pin the workspace-internal memex-core dependency to the
# exact new version. `=X` is fine for stable releases too.
sed -i.bak -E "s|(memex-core = \\{ path = \"\\.\\./core\", version = )\"[^\"]+\"|\\1\"=${NEW}\"|" cli/Cargo.toml
rm -f cli/Cargo.toml.bak

# Pin optionalDependencies (per-platform binary subpackages) to the new
# version. CI rewrites these transiently at publish time, but the committed
# value matters for local `npm install ./plugin` dev installs — without
# this bump the placeholder would silently go stale across releases.
node -e "
const fs = require('fs');
const p = JSON.parse(fs.readFileSync('plugin/package.json', 'utf8'));
for (const k of Object.keys(p.optionalDependencies || {})) {
  p.optionalDependencies[k] = '${NEW}';
}
fs.writeFileSync('plugin/package.json', JSON.stringify(p, null, 2) + '\n');
"

# Refresh Cargo.lock so committed lockfile matches the new versions.
# Without this, every release commit produces a one-line lockfile drift on
# the next build that has to be cleaned up by hand.
cargo update -p memex-core -p memex-cli --offline >/dev/null 2>&1 || \
  cargo update -p memex-core -p memex-cli >/dev/null

# Confirm.
read -rp "Release v${NEW}? [y/N] " yn
[ "$yn" = "y" ] || {
  git checkout plugin/package.json core/Cargo.toml cli/Cargo.toml Cargo.lock
  echo "aborted" >&2; exit 1;
}

git add plugin/package.json core/Cargo.toml cli/Cargo.toml Cargo.lock
git commit -m "chore: release v${NEW}"
git tag "v${NEW}"
git push origin "$BRANCH" "v${NEW}"

echo "Pushed v${NEW}. Watch CI: gh run watch"
