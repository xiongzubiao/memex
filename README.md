# memex

A personal wiki storage and search engine with agent-synthesized Q&A.

Write pages as Markdown with frontmatter, and query them in natural
language. Retrieval uses BM25 + vector fusion with RRF; synthesis runs
through Claude, Codex, or Gemini (your choice) via a long-lived daemon
that keeps the subprocess warm.

```bash
# Write a page
memex write auth-migration --force --quiet <<EOF
---
title: Auth Migration Timeline
...
EOF

# Query it
memex query "when did production rollout begin"
#  Expansion:
#    lex: production deployment
#    vec: when did the production release happen
#    hyde: The production rollout began on [date].
#
#  Production rollout began on 2026-04-16. [[auth-migration]]
#
#  Citations:
#    [[auth-migration]]
```

## Install

See [docs/INSTALL.md](docs/INSTALL.md) for the full walkthrough.
TL;DR:

```bash
git clone <repo>
cd memex
cargo build --release
cp target/release/memex ~/.local/bin/
scripts/install-hooks.sh         # optional: pre-warm on agent session start
memex query "your question here"
```

## Architecture

- `core/` — `memex-core`: BM25 + vector search, embed, storage primitives.
- `cli/` — `memex-cli`: the `memex` binary (CLI + daemon).
- `docs/superpowers/specs/` — design spec.
- `docs/superpowers/plans/` — implementation plans (executed task-by-task).

## Status

- [x] Plan 1: daemon IPC skeleton
- [x] Plan 2: retrieval actor + `memex query --raw`
- [x] Plan 3: Claude synthesis worker
- [x] Plan 4: worker robustness (retry, timeout, restart)
- [ ] Plan 5: Codex + Gemini providers
- [x] Plan 6: query expansion on weak signal
- [x] Plan 7: install hooks + docs

