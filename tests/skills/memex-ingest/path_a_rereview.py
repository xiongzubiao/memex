#!/usr/bin/env python3
"""Path A rc=3 re-review walkthrough.

Pre-condition: wiki has `go.md`. The agent will ingest content that
EXTRACT names `go`, producing a merge proposal.

Mid-flight: a `PreToolUse` hook on `Bash` watches for the agent's
`memex plan apply` invocation. Just before that command runs, the hook
mutates `~/.memex/wiki/go.md` externally so the proposal's
`merge_target_hash` becomes stale. The daemon detects the mismatch and
returns rc=3 with a refreshed plan; the agent must re-show the new
plan, re-prompt, and apply on confirmation.

`can_use_tool` doesn't fire for `Bash` under the SDK's default
permission mode — that's why a separate hook channel is needed for
this scenario.
"""
import asyncio
import os
import subprocess
import sys

from _common import build_options, prepend_bin_to_path, BIN_DIR
from claude_agent_sdk import query
from claude_agent_sdk.types import HookMatcher, PermissionResultAllow

MEMEX = os.path.join(BIN_DIR, "memex")
state = {"mutated": False, "askq_calls": 0, "rereview_seen": False}


async def mutate_before_apply(input_data, tool_use_id, context):
    """PreToolUse hook: mutate go.md just before `memex plan apply` runs."""
    if state["mutated"]:
        return {"continue_": True}
    cmd = input_data.get("tool_input", {}).get("command", "") if isinstance(input_data, dict) else ""
    if not cmd:
        cmd = (input_data.get("command", "") if isinstance(input_data, dict) else "")
    if "plan apply" in cmd and "memex" in cmd:
        print("[HOOK] mutating go.md before plan apply runs", file=sys.stderr)
        subprocess.run(
            [MEMEX, "write", "go", "--force"],
            input=b"# Go\n\nMUTATED EXTERNALLY between plan and apply.\n",
            check=False,
        )
        state["mutated"] = True
    return {"continue_": True}


def make_can_use_tool():
    async def can_use_tool(tool_name, input_data, context):
        if tool_name == "AskUserQuestion":
            state["askq_calls"] += 1
            questions = input_data.get("questions", [])
            answers = {}
            for q in questions:
                opts = q.get("options", [])
                labels = [o.get("label", "") for o in opts]
                # Prefer Apply / Accept; the second AskUserQuestion (after rc=3
                # re-review) should also resolve to Apply
                chosen = next(
                    (l for l in labels if "apply" in l.lower() or "accept" in l.lower()),
                    labels[0] if labels else "Accept",
                )
                answers[q.get("question", "")] = chosen
                hdr = q.get("header", "")
                print(
                    f"[AskUserQuestion #{state['askq_calls']}] header={hdr!r} → {chosen!r}",
                    file=sys.stderr,
                )
            return PermissionResultAllow(
                updated_input={"questions": questions, "answers": answers}
            )
        return PermissionResultAllow(updated_input=input_data)

    return can_use_tool


async def main():
    test_file = sys.argv[1] if len(sys.argv) > 1 else "/tmp/edge-merge2.md"

    prepend_bin_to_path()
    # Pre-populate go.md
    subprocess.run(
        [MEMEX, "write", "go", "--force"],
        input=b"# Go\n\nCompiled language with goroutines.\n",
        check=True,
    )
    print("Pre-populated go.md", file=sys.stderr)

    can_use_tool = make_can_use_tool()

    async def prompt_stream():
        yield {
            "type": "user",
            "message": {
                "role": "user",
                "content": (
                    f"Use the /memex-ingest skill to ingest {test_file}. "
                    "Follow SKILL.md exactly."
                ),
            },
        }

    options = build_options(can_use_tool=can_use_tool, allow_askuserquestion=True)
    # Override the no-op hook with the mutate hook for Bash.
    options.hooks = {
        "PreToolUse": [HookMatcher(matcher="Bash", hooks=[mutate_before_apply])]
    }

    final_text = None
    print("=== Path A rc=3 re-review walkthrough ===", file=sys.stderr)
    async for message in query(prompt=prompt_stream(), options=options):
        if hasattr(message, "subtype") and getattr(message, "subtype", None) == "success":
            final_text = getattr(message, "result", "")
        if type(message).__name__ == "AssistantMessage":
            for c in getattr(message, "content", []):
                if hasattr(c, "text") and c.text:
                    if "rc=3" in c.text or "re-review" in c.text.lower() or "refreshed" in c.text.lower():
                        state["rereview_seen"] = True

    print(f"\n=== Result ===", file=sys.stderr)
    print(f"AskUserQuestion calls: {state['askq_calls']}", file=sys.stderr)
    print(f"Wiki mutated mid-flight: {state['mutated']}", file=sys.stderr)
    print(f"Re-review surfaced to chat: {state['rereview_seen']}", file=sys.stderr)
    print(f"Final: {final_text!r}", file=sys.stderr)


if __name__ == "__main__":
    asyncio.run(main())
