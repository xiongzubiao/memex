# Skill edge-case test harness

`sdk-driver.py` exercises `/memex-ingest` end-to-end against a live
daemon, using the **claude-agent-sdk** (Python) so `AskUserQuestion`
answers can be supplied programmatically via the `can_use_tool`
callback.

## Why the SDK and not the CLI

Claude Code's CLI in `-p --input-format stream-json` mode denies
`AskUserQuestion` regardless of `--permission-mode` (verified against
`auto`, `dontAsk`, `bypassPermissions`). The denial is reported in the
session result's `permission_denials` array — `AskUserQuestion` is
gated through the platform's permission UI component, which is absent
in headless mode.

The Agent SDK exposes the `can_use_tool` callback that intercepts
`AskUserQuestion` calls before the platform synthesizes a denial. The
callback returns the user's selections via:

```
PermissionResultAllow(updated_input={"questions": ..., "answers": {...}})
```

Reference:
https://code.claude.com/docs/en/agent-sdk/user-input

## Setup

```
python3 -m venv .venv
.venv/bin/pip install claude-agent-sdk
```

## Run

```
.venv/bin/python sdk-driver.py <test_file> <policy>
```

Policies:
- `accept`     — pick first option containing "accept" or "commit"
- `abort`      — pick first option containing "abort" or "cancel"
- `rename-slug`— pick "rename slug" if offered
- `drop-all`   — pick "drop"
- `retry`      — pick "retry"

## Known limitation

The agent doesn't always ask the SKILL.md-prescribed
"per-proposal: rename slug / drop / accept" questions verbatim — it
sometimes asks higher-level structural questions ("1 combined page
vs N split pages?"). Policy keywords don't always match those
questions, so test scenarios may fall back to first/last option.
This is an interesting finding about agent interpretation of the
skill prompt, not a driver bug. Future work: tighten the SKILL.md
to constrain question shape, or design scenario-specific picker
functions.

## What this tests vs. what unit tests cover

- **Unit tests (cli/src/daemon/handler/plan.rs)** verify the daemon
  handlers' behavior at the Rust level: validation, MERGE-dry-run
  logic, apply staleness paths, etc.
- **CLI smoke (manual)** verifies the CLI subcommands correctly
  marshal Request/Event variants.
- **This driver** verifies the **agent's interpretation** of the
  SKILL.md and the full agent → CLI → daemon → wiki round-trip,
  including AskUserQuestion answer flow.
