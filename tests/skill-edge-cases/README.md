# Skill edge-case test harness

Two SDK-based drivers exercise `/memex-ingest` end-to-end against a
live daemon, using the **claude-agent-sdk** (Python).

- `sdk-driver.py` — single-turn, parameterized by an answer policy
  for `AskUserQuestion`. Use this for **Path A** scenarios (accept,
  drop, rename-slug, etc.).
- `sdk-driver-multiturn.py` — denies `AskUserQuestion` to force
  **Path B** (chat directives), then watches for the agent's chat
  prompt and injects a user reply (`apply`, `drop X; rename Y to Z`,
  etc.) so the apply step actually runs.

## Critical setup detail

The SDK's `ClaudeAgentOptions(plugins=[...])` parameter is **required**
to load the worktree's SKILL.md instead of the (stale) marketplace
copy at `~/.claude/local-marketplaces/memex-dev/`. Without it the
agent reads the wrong file and your prompt edits are invisible.

```python
options = ClaudeAgentOptions(
    plugins=[{
        "type": "local",
        "path": "/path/to/memex-ingest-skill-redesign/plugin",
    }],
    ...
)
```

Both drivers pass this automatically.

## What this verifies

The skill has three review paths (Path A: AskUserQuestion chips when
available; Path B: chat directives as the cross-agent fallback;
Path C: editor handoff for >20 proposals or any title edit). The
drivers exercise:

- **Path A** (sdk-driver.py): per-proposal AskUserQuestion with
  `accept`/`rename slug`/`drop` chips. Free-text new slug via the
  `Other` channel returned to the agent and applied via the
  agent's file-edit tool against `/tmp/memex-plan-<docid>.json`.
- **Path B** (sdk-driver-multiturn.py): chat-based directives.
  `can_use_tool` denies `AskUserQuestion`, the agent paste plan show
  output to chat with the SKILL.md prompt, the driver injects a
  follow-up reply that the agent parses as `;`-separated directives.
- **Path C** (sdk-driver.py with a 26-subject file): the agent
  routes to editor handoff (no chips, no chat directives) and stops
  pending the user opening the plan file in their editor.

End-to-end pipeline: `source add → source plan → plan show → plan
apply`, including merge proposals against existing wiki pages.

## Setup

```
python3 -m venv .venv
.venv/bin/pip install claude-agent-sdk
```

## Run

```
# Path A (AskUserQuestion enabled)
.venv/bin/python sdk-driver.py <test_file> <policy>

# Path B (AskUserQuestion denied; multi-turn chat injection)
.venv/bin/python sdk-driver-multiturn.py <test_file> "<reply>"
```

`sdk-driver.py` policies (for AskUserQuestion option selection):
- `accept`     — first option containing accept / commit / approve
- `abort`      — first containing abort / cancel / skip
- `rename-slug`— prefer `rename`
- `drop-all`   — prefer `drop`

`sdk-driver-multiturn.py` reply forms:
- `apply` — commit the plan as-is
- `drop <slug>; rename <slug> to <new>` — directives, then `apply`
- `cancel` — discard plan

## Notes on agent variability

EXTRACT is non-deterministic. Identical input content can yield 3
proposals in one run (subject-based: `python` / `rust` / `go`) and
1 proposal in another (umbrella: `tech-stack`). For Path B
multi-turn tests with specific directives, use content with truly
unrelated subjects (a person, a place, a concept) so EXTRACT can't
consolidate under one umbrella.

## Known limitations

- `sdk-driver.py` is single-turn. For Path B scenarios, use
  `sdk-driver-multiturn.py`.
- Counter logging (`Bash calls`, `Edit calls`) only fires through
  `can_use_tool` for `AskUserQuestion`; tools in `allowed_tools`
  are auto-allowed without callback under the SDK's default
  permission mode. Verify pipeline execution via `~/.memex/wiki/`
  contents and `~/.memex/daemon.log.*` instead.

## What this tests vs. what unit tests cover

- **Unit tests** (`cli/src/daemon/handler/plan.rs`) verify daemon
  handlers at the Rust level: validation, MERGE-dry-run, apply
  staleness paths.
- **CLI smoke** (manual) verifies CLI subcommands marshal
  `Request`/`Event` variants correctly. `rc=3` (re-review on hash
  mismatch) and `rc=4` (partial commit) are CLI-smoke-verified —
  see git history for repro recipes.
- **These drivers** verify the **agent's interpretation** of the
  SKILL.md and the full agent → CLI → daemon → wiki round-trip.
