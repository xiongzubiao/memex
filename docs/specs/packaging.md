# Packaging, Install & Release

**Status:** Living design doc — describes the system as implemented (shipped in v0.1.x).
**Last updated:** 2026-05-29

Part of the consolidated memex design set ([`README.md`](README.md)). Distribution/build concern, distinct from the system architecture.

## Distribution model

Memex ships as a Node wrapper package plus per-platform binary packages:

- **Wrapper** `@xiongzubiao/memex` — exposes the `memex` bin (`plugin/bin/memex-wrapper.js`) and declares the platform packages as `optionalDependencies`. npm installs only the one matching the host.
- **Platform packages** `@xiongzubiao/memex-<os>-<arch>` for `darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`. Each carries the prebuilt binary, no `bin` field, `preferUnplugged: true`, and `os`/`cpu` constraints. **Windows is not supported** — the wrapper rejects it with an actionable message.

The wrapper resolves the binary via `require.resolve("<platform-pkg>/package.json")`, with a fallback to the Claude Code marketplace path (`~/.claude/plugins/npm-cache/`), then execs it and passes through the exit code. `scripts/platform-mapping.mjs` is the single source of truth for the `os/arch → package name` mapping.

## postinstall

`plugin/postinstall.js` does **not** download the binary (it comes from the platform package). It:

- Downloads the embedding model, tokenizer, and ONNX runtime, each verified against a pinned SHA-256 (re-download on mismatch).
- Writes `~/.memex/.install-status` (JSON) for error surfacing.
- Opportunistically stops an old daemon on upgrade.
- Runs `memex install` on global installs.

## CLI install surface

- **`memex install [--agents …] [--dry-run]`** (`cli/src/install.rs`) — writes/updates SessionEnd + SessionStart hook configs for `claude-code`, `codex`, `gemini-cli`, `openclaw`, `hermes`, `opencode`. Idempotent (removes existing memex entries before re-adding).
- **`memex doctor`** — checks model/tokenizer/ONNX-runtime presence, daemon health, the agent CLIs on PATH, and `~/.memex/.install-status`; prints `[PASS]`/`[FAIL]`, exits non-zero on critical failures.
- **`memex hook ingest <agent>`** — the session-hook entry point (see [`ingest.md`](ingest.md)).
- **`memex uninstall`** — removes hook configs. Note: npm 11 no longer fires `preuninstall`/`uninstall`/`postuninstall`, so the documented manual flow is `memex uninstall && npm uninstall -g @xiongzubiao/memex`.

## Hooks

`plugin/hooks/hooks.json` (Claude Code, using native `${CLAUDE_PLUGIN_ROOT}` expansion) plus `claude-code.json`, `codex.json`, `gemini-cli.json`. SessionEnd → `memex hook ingest <agent>`; SessionStart → `memex daemon start`. Codex `Stop` hooks parse stdout as JSON, so `memex hook ingest` must emit empty stdout on success.

## Marketplace & manifests

The repo-root `.claude-plugin/marketplace.json` points at the npm package `@xiongzubiao/memex`. The per-agent plugin manifests live under `plugin/` (e.g. `plugin/.claude-plugin/plugin.json`, `plugin/.codex-plugin/`, `plugin/.cursor-plugin/`, `plugin/.openclaw-plugin/`).

## Release

- **`scripts/release.sh [patch|minor|major|pre*]`** — pre-flight (clean tree, on `main` or `plugin-publish-fix`, up to date), bump `plugin/package.json` + `core/Cargo.toml` + `cli/Cargo.toml` + the `optionalDependencies` + the four agent plugin manifests (claude/codex/cursor/openclaw), pin the `memex-core` dependency, refresh `Cargo.lock`, confirm, then commit + tag + push. Run from `main` (or `plugin-publish-fix`); squash-merge a feature branch to `main` *before* tagging, so the tag isn't an orphan.
- **`.github/workflows/release.yml`** — on a `v*` tag: build the 4-platform matrix (linux-arm64 cross-compiled; macOS Intel on `macos-15-intel`), publish an internal GitHub Release with the binaries, then `publish-npm`: verify the tag matches `plugin/package.json` version, run `publish-platform-packages.mjs` (idempotent via an `npm view` precheck) and `publish-wrapper.mjs`.
- **`.github/workflows/verdaccio-rehearsal.yml`** — on PRs touching `plugin/`, `scripts/`, or the release workflow: publish all packages to a local Verdaccio and verify a clean install resolves the wrapper + exactly one platform package and that `memex --help` runs.

Releases require explicit authorization with a named version; automated/auto modes do not authorize an npm publish.
