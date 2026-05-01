#!/usr/bin/env python3
"""Path A driver: AskUserQuestion-driven review (Claude Code's chip UX).

Single-turn. The driver answers AskUserQuestion via `can_use_tool` per a
selection policy, so the agent's review step completes in one round-trip.

Usage:
    path_a.py <test_file> <policy>

Policies:
    accept       — pick the first option containing accept / commit / approve
    abort        — pick the first containing abort / cancel / skip
    rename-slug  — prefer `rename`
    drop-all     — prefer `drop`
    retry        — prefer `retry`
"""
import asyncio
import json
import sys

from _common import build_options, prepend_bin_to_path
from claude_agent_sdk import query
from claude_agent_sdk.types import PermissionResultAllow


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
        return find("edit", "rename") or find("accept") or labels[0]
    if policy == "drop-all":
        return find("drop", "skip", "discard") or labels[-1]
    if policy == "retry":
        return find("retry", "again") or find("accept") or labels[0]
    return labels[0]


def make_can_use_tool(policy: str):
    counts = {"bash": 0, "edit": 0, "askq": 0}

    async def can_use_tool(tool_name, input_data, context):
        if tool_name == "AskUserQuestion":
            questions = input_data.get("questions", [])
            counts["askq"] += 1
            answers = {}
            for q in questions:
                chosen = pick_label(q.get("options", []), policy)
                answers[q.get("question", "")] = chosen
                print(
                    f"[AskUserQuestion] header={q.get('header','')!r} → {chosen!r}",
                    file=sys.stderr,
                )
            return PermissionResultAllow(
                updated_input={"questions": questions, "answers": answers}
            )
        if tool_name == "Bash":
            counts["bash"] += 1
            print(f"[Bash] {input_data.get('command','')[:200]}", file=sys.stderr)
        elif tool_name == "Edit":
            counts["edit"] += 1
            print(
                f"[Edit] {input_data.get('file_path')}: "
                f"{input_data.get('old_string','')[:50]!r} -> "
                f"{input_data.get('new_string','')[:50]!r}",
                file=sys.stderr,
            )
        else:
            print(f"[{tool_name}] {json.dumps(input_data)[:200]}", file=sys.stderr)
        return PermissionResultAllow(updated_input=input_data)

    return can_use_tool, counts


async def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <test_file> <policy>", file=sys.stderr)
        sys.exit(2)
    test_file, policy = sys.argv[1], sys.argv[2]

    can_use_tool, counts = make_can_use_tool(policy)
    prepend_bin_to_path()

    async def prompt_stream():
        yield {
            "type": "user",
            "message": {
                "role": "user",
                "content": (
                    f"Use the /memex-ingest skill to ingest this local file: {test_file}. "
                    "The binary `memex` is on PATH. Follow the SKILL.md exactly."
                ),
            },
        }

    options = build_options(can_use_tool=can_use_tool, allow_askuserquestion=True)

    final_text = None
    print(f"Spawning SDK session (Path A, policy={policy})", file=sys.stderr)
    async for message in query(prompt=prompt_stream(), options=options):
        if hasattr(message, "subtype") and getattr(message, "subtype", None) == "success":
            final_text = getattr(message, "result", "")

    print(f"\n=== Result ===", file=sys.stderr)
    print(f"Counts: {counts}", file=sys.stderr)
    print(f"Final: {final_text!r}", file=sys.stderr)


if __name__ == "__main__":
    asyncio.run(main())
