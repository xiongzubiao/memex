# memex-ingest skill tests

Two SDK-based drivers exercise the `/memex-ingest` skill end-to-end
against a live `memex daemon`, using **claude-agent-sdk** (Python).

| File | Path | Mode |
|------|------|------|
| `path_a.py` | A | Single-turn. Single policy applied to all proposals. |
| `path_a_mixed.py` | A | Single-turn. Per-slug policy: drop one, rename one, accept one in a single batched call. |
| `path_a_rereview.py` | A | Single-turn. Mutates wiki mid-flight via a `PreToolUse` hook so `plan apply` returns rc=3; verifies the agent re-shows the refreshed plan and applies on second confirm. |
| `path_b.py` | B | Multi-turn. Driver denies `AskUserQuestion` via `can_use_tool`, forcing chat-directive fallback; injects user reply when the agent reaches the directive prompt. |
| `_common.py` | — | Shared SDK options builder, plugin-loading config, no-op `PreToolUse` hook. |

## Why two drivers

The skill has three review paths:

- **Path A** — chip-style structured questions (Claude Code's `AskUserQuestion`). Preferred when available.
- **Path B** — chat-based directives (`apply` / `drop <slug>` / `rename <slug> to <new>`). Fallback for agents without chip tools (Codex, Gemini CLI, etc.).
- **Path C** — editor handoff for >20 proposals or any title edit. The agent surfaces the plan path; the user opens it in their editor.

Path A and Path B differ enough that one combined driver would be all
flag-handling, so they're separate scripts that share `_common.py`.
Path C is exercised by `path_a.py` with a 26-subject input file: the
agent skips the chip path and surfaces the plan path to chat.

## Critical setup detail

The SDK's `ClaudeAgentOptions(plugins=[...])` parameter is **required**
to load the worktree's SKILL.md instead of the (stale) marketplace
copy at `~/.claude/local-marketplaces/memex-dev/`. `_common.py`
handles this; see `build_options()`.

## Setup

```bash
python3 -m venv .venv
.venv/bin/pip install claude-agent-sdk
```

The drivers run from the test directory, so use the venv directly:

```bash
cd tests/skills/memex-ingest
../../../.venv/bin/python path_a.py <fixture> accept
```

(Or activate the venv however you prefer. The drivers don't hardcode a
venv path.)

## Run

```bash
# Path A — chip-driven
.venv/bin/python tests/skills/memex-ingest/path_a.py <fixture.md> <policy>

# Path B — chat-directive multi-turn
.venv/bin/python tests/skills/memex-ingest/path_b.py <fixture.md> "<reply>"
```

`path_a.py` policies (option selection):
- `accept` — accept / commit / approve
- `abort` — abort / cancel / skip
- `rename-slug` — prefer `rename`
- `drop-all` — prefer `drop`
- `retry` — prefer `retry`

`path_b.py` reply forms:
- `apply` — commit as-is
- `drop <slug>; rename <slug> to <new>; apply` — directives, then apply
- `cancel` — discard plan

## Fixture content matters

EXTRACT is non-deterministic. Identical input can yield 3 separate
proposals in one run (subject-based: `python` / `rust` / `go`) and 1
umbrella proposal in another (`tech-stack`). For Path B tests with
specific `drop <slug>` / `rename <slug>` directives, pick content with
truly unrelated subjects (a person, a place, a concept) so EXTRACT
can't consolidate.

## Counter caveat

`Bash` and `Edit` counters in driver output are unreliable: tools in
`allowed_tools` are auto-allowed without going through `can_use_tool`
under the SDK's default permission mode. To verify the pipeline ran,
inspect `~/.memex/wiki/` and `~/.memex/daemon.log.*` after the run.

## Layered coverage

- **Daemon unit tests** (`cli/src/daemon/handler/plan.rs`) — handler
  logic, validation, MERGE-dry-run paths.
- **CLI smoke tests** (manual / scripted) — `Request` / `Event`
  marshaling. `rc=3` (re-review on hash mismatch) and `rc=4` (partial
  commit on per-proposal apply failure) are CLI-verified by mutating
  wiki state between `source plan` and `plan apply`.
- **These drivers** — agent's interpretation of the SKILL.md and the
  full agent → CLI → daemon → wiki round-trip across all three paths.

## Verified scenarios

End-to-end across these drivers + manual CLI smoke:

**Path A (chip-driven):**
- accept-all (3 proposals → 3 wiki pages)
- drop-all (0 wiki pages, plan cleaned up)
- rename via `Other` free-text (rust → oxidation)
- mixed actions in one batched call (drop one + rename one + accept one)
- 26 unrelated subjects → routes to Path C editor handoff

**Path B (chat-directive):**
- `apply` reply (3 proposals → 3 wiki pages)
- `drop X; rename Y to Z; apply` (correct subset committed)
- `cancel` reply (plan removed, no commits)
- invalid directive (`drop nonexistent`) — agent refuses to silently
  ignore, lists actual slugs, offers recovery options

**Daemon / CLI:**
- Re-ingest dedup (content-addressed docid)
- Slug collision in plan → validation error
- Empty extract (trivial / no-subject content) → 0-byte plan, clean exit
- MERGE proposal (existing wiki + overlapping ingest) → daemon emits
  unified diff, apply commits merged content with source attribution
- rc=3 re-review (wiki mutated between plan and apply) → daemon emits
  refreshed plan with new merge_target_hash + regenerated diff. Agent
  walkthrough automated by `path_a_rereview.py` (PreToolUse hook
  intercepts `Bash` to mutate the wiki just before `plan apply`).
- rc=4 partial commit (one target write fails, others succeed)
- rc=4 all-fail (wiki dir read-only) → all proposals errored
- Phased re-MERGE: rename proposal slug to match existing wiki page →
  daemon detects stale state, re-MERGEs in phase 2
- Lint after apply correctly flags dangling `[[link]]` refs
- Stale socket recovery (SIGKILL daemon → CLI auto-respawns)
- Daemon-down → CLI auto-spawns daemon transparently
- Large source (~22KB, 10 distinct subjects) → 10 subject-named proposals

**Bugs caught and fixed during testing:**
- `memex write` produced duplicate frontmatter when stdin already had a
  `---` block (parse_frontmatter strict-deserialize fell back to whole
  content; fixed by using delimiter-only `split_frontmatter` instead).
- Hybrid SKILL.md initially let the agent reframe Path A → Path B for
  "clean-looking" plans. Tightened the decision rule to "no judgment,
  no reframing": if AskUserQuestion is available, use Path A.
