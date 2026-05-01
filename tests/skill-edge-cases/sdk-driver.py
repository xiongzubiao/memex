#!/usr/bin/env python3
"""
SDK-based test driver. Uses claude-agent-sdk's `can_use_tool` callback
to handle AskUserQuestion, which can't be done via the CLI's stream-json
mode (the permission UI component intercepts it).

Usage:
    skill-sdk-driver.py <test_file> <policy>

policies:
    accept       — pick the first option that says accept/commit
    abort        — pick option that says abort/cancel/skip
    rename-slug  — pick "rename slug" option
    drop-all     — pick "drop" option
    retry        — pick "retry" option
"""
import asyncio
import json
import os
import sys

sys.path.insert(0, "/tmp/skill-sdk-venv/lib/python3.14/site-packages")
from claude_agent_sdk import ClaudeAgentOptions, ResultMessage, query
from claude_agent_sdk.types import (
    HookMatcher,
    PermissionResultAllow,
    PermissionResultDeny,
)

PLUGIN_DIR = "/Users/zxiong/MemVerge/memex-ingest-skill-redesign/plugin"
NEW_BIN_DIR = "/Users/zxiong/MemVerge/memex-ingest-skill-redesign/target/release"


def pick_label(options: list, policy: str) -> str:
    if not options:
        return ""
    labels = [o.get("label", "") for o in options]
    descs = [o.get("description", "") for o in options]
    blob = [f"{l} {d}".lower() for l, d in zip(labels, descs)]

    def find(*keywords):
        for i, b in enumerate(blob):
            if any(k in b for k in keywords):
                return labels[i]
        return None

    if policy == "accept":
        return find("accept", "commit", "approve") or labels[0]
    if policy == "abort":
        return find("abort", "cancel", "skip", "stop") or labels[-1]
    if policy == "rename-slug":
        return find("rename") or find("accept") or labels[0]
    if policy == "drop-all":
        return find("drop", "skip", "discard") or labels[-1]
    if policy == "retry":
        return find("retry", "again") or find("accept") or labels[0]
    return labels[0]


def make_can_use_tool(policy: str):
    bash_calls = []
    edit_calls = []
    askq_calls = []

    async def can_use_tool(tool_name, input_data, context):
        if tool_name == "AskUserQuestion":
            questions = input_data.get("questions", [])
            askq_calls.append(questions)
            answers = {}
            for q in questions:
                chosen = pick_label(q.get("options", []), policy)
                answers[q.get("question", "")] = chosen
                print(
                    f"[AskUserQuestion] header={q.get('header','')!r} → {chosen!r}",
                    file=sys.stderr,
                )
            return PermissionResultAllow(
                updated_input={
                    "questions": questions,
                    "answers": answers,
                }
            )
        if tool_name == "Bash":
            cmd_str = input_data.get("command", "")[:300]
            bash_calls.append(cmd_str)
            print(f"[Bash] {cmd_str}", file=sys.stderr)
        elif tool_name == "Edit":
            edit_calls.append(
                {
                    "file": input_data.get("file_path"),
                    "old": input_data.get("old_string", "")[:80],
                    "new": input_data.get("new_string", "")[:80],
                }
            )
            print(
                f"[Edit] {input_data.get('file_path')}: {input_data.get('old_string','')[:50]!r} -> {input_data.get('new_string','')[:50]!r}",
                file=sys.stderr,
            )
        else:
            print(f"[{tool_name}] {json.dumps(input_data)[:200]}", file=sys.stderr)
        return PermissionResultAllow(updated_input=input_data)

    return can_use_tool, bash_calls, edit_calls, askq_calls


async def dummy_hook(input_data, tool_use_id, context):
    return {"continue_": True}


async def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <test_file> <policy>", file=sys.stderr)
        sys.exit(2)
    test_file = sys.argv[1]
    policy = sys.argv[2]

    can_use_tool, bash_calls, edit_calls, askq_calls = make_can_use_tool(policy)

    # Make the new binary findable from the agent's Bash calls.
    os.environ["PATH"] = NEW_BIN_DIR + ":" + os.environ.get("PATH", "")

    async def prompt_stream():
        yield {
            "type": "user",
            "message": {
                "role": "user",
                "content": (
                    f"Use the /memex-ingest skill to ingest this local file: {test_file}. "
                    "The binary `memex` is on PATH. Follow the SKILL.md exactly. "
                    "Use AskUserQuestion when the skill calls for it; I'm here to answer."
                ),
            },
        }

    options = ClaudeAgentOptions(
        can_use_tool=can_use_tool,
        hooks={"PreToolUse": [HookMatcher(matcher=None, hooks=[dummy_hook])]},
        # Ensure AskUserQuestion is in the toolset
        allowed_tools=[
            "Bash", "Edit", "Read", "Write", "Glob", "Grep",
            "AskUserQuestion", "Skill", "ToolSearch",
        ],
    )

    final_text = None
    print(f"Spawning SDK session (policy={policy})", file=sys.stderr)
    async for message in query(prompt=prompt_stream(), options=options):
        if hasattr(message, "subtype") and getattr(message, "subtype", None) == "success":
            final_text = getattr(message, "result", "")
        else:
            mt = type(message).__name__
            if mt == "AssistantMessage":
                for c in getattr(message, "content", []):
                    if hasattr(c, "text") and c.text:
                        # avoid spamming long text
                        pass

    print(f"\n=== Result ===", file=sys.stderr)
    print(f"Bash calls: {len(bash_calls)}", file=sys.stderr)
    print(f"Edit calls: {len(edit_calls)}", file=sys.stderr)
    print(f"AskUserQuestion calls: {len(askq_calls)}", file=sys.stderr)
    print(f"Final: {final_text!r}", file=sys.stderr)


if __name__ == "__main__":
    asyncio.run(main())
