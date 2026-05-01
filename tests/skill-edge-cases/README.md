# Skill edge-case test harness

`sdk-driver.py` exercises `/memex-ingest` end-to-end against a live
daemon, using the **claude-agent-sdk** (Python).

## Critical setup detail

The SDK's `ClaudeAgentOptions(plugins=[...])` parameter is **required**
to load the worktree's SKILL.md instead of the (stale) marketplace
copy at `~/.claude/local-marketplaces/memex-dev/`. Without this,
the agent reads the wrong file and your prompt edits are invisible.

```python
options = ClaudeAgentOptions(
    plugins=[{
        "type": "local",
        "path": "/path/to/memex-ingest-skill-redesign/plugin",
    }],
    ...
)
```

The driver passes this automatically. If you adapt it for another
project, double-check the plugin path resolves to the source-of-truth
SKILL.md.

## What this verifies

The skill's flow is **agent-agnostic** — it uses chat-based directives
(`"Reply 'apply' to commit as-is, or send 'drop <slug>; rename <slug>
to <new>'"`) instead of Claude-specific tools like `AskUserQuestion`.
This means the same skill should work with codex, gemini-cli, or any
agent that can read/write to chat.

The driver verifies:
- The agent loads and follows the SKILL.md
- `source add → source plan → plan show → plan apply` runs end-to-end
- The agent surfaces the chat prompt for review (matches the literal
  text in SKILL.md)
- AskUserQuestion can be answered via `can_use_tool` callback when the
  agent does choose to use it

## Setup

```
python3 -m venv .venv
.venv/bin/pip install claude-agent-sdk
```

## Run

```
.venv/bin/python sdk-driver.py <test_file> <policy>
```

Policies (for `AskUserQuestion` answer selection if the agent uses it):
- `accept`     — pick first option containing "accept" / "commit" / "apply"
- `abort`      — pick first option containing "abort" / "cancel"
- `rename-slug`— pick "rename slug" if offered
- `drop-all`   — pick "drop"

## Known limitations

The driver is single-turn: it sends one initial user message and reads
the agent's response stream. For chat-based review the agent ends its
turn waiting for a follow-up user message — the driver doesn't yet
send that follow-up. Multi-turn support (sending `apply` or
`drop X; rename Y to Z` after the agent's first chat output) would
extend the loop to detect "agent waiting" and inject the next message.

## What this tests vs. what unit tests cover

- **Unit tests (`cli/src/daemon/handler/plan.rs`)** verify the daemon
  handlers' behavior at the Rust level: validation, MERGE-dry-run
  logic, apply staleness paths, etc.
- **CLI smoke (manual)** verifies the CLI subcommands correctly
  marshal Request/Event variants.
- **This driver** verifies the **agent's interpretation** of the
  SKILL.md and the full agent → CLI → daemon → wiki round-trip.
