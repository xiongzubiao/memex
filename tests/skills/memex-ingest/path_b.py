#!/usr/bin/env python3
"""Path B driver: chat-directive review (cross-agent fallback).

Multi-turn. The driver denies AskUserQuestion via `can_use_tool` so the
agent must use Path B (paste plan show output to chat with the directive
prompt). When the agent reaches that prompt, the driver injects the
configured reply via an asyncio.Queue.

Usage:
    path_b.py <test_file> <reply>

Reply forms:
    apply                                          — commit as-is
    drop <slug>; rename <slug> to <new>; apply     — directives + apply
    cancel                                         — discard plan
"""
import asyncio
import sys

from _common import build_options, prepend_bin_to_path
from claude_agent_sdk import query
from claude_agent_sdk.types import PermissionResultAllow, PermissionResultDeny


def make_can_use_tool():
    counts = {"bash": 0, "edit": 0, "askq_denied": 0}

    async def can_use_tool(tool_name, input_data, context):
        if tool_name == "AskUserQuestion":
            counts["askq_denied"] += 1
            print("[AskUserQuestion DENIED] (force Path B)", file=sys.stderr)
            return PermissionResultDeny(message="Use Path B chat directives.")
        if tool_name == "Bash":
            counts["bash"] += 1
        elif tool_name == "Edit":
            counts["edit"] += 1
        return PermissionResultAllow(updated_input=input_data)

    return can_use_tool, counts


async def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <test_file> <reply>", file=sys.stderr)
        sys.exit(2)
    test_file, user_reply = sys.argv[1], sys.argv[2]

    can_use_tool, counts = make_can_use_tool()
    prepend_bin_to_path()

    user_queue: asyncio.Queue = asyncio.Queue()
    await user_queue.put({
        "type": "user",
        "message": {
            "role": "user",
            "content": (
                f"Use the /memex-ingest skill to ingest this local file: {test_file}. "
                "The binary `memex` is on PATH. Follow the SKILL.md exactly."
            ),
        },
    })

    reply_sent = False

    async def prompt_stream():
        while True:
            msg = await user_queue.get()
            if msg is None:
                return
            yield msg

    options = build_options(can_use_tool=can_use_tool, allow_askuserquestion=False)

    final_text = None
    print(f"Spawning SDK session (Path B, reply={user_reply!r})", file=sys.stderr)

    async for message in query(prompt=prompt_stream(), options=options):
        if type(message).__name__ == "AssistantMessage":
            for c in getattr(message, "content", []):
                if hasattr(c, "text") and c.text:
                    text = c.text
                    if (not reply_sent
                            and "Reply `apply`" in text
                            and "drop" in text.lower()):
                        print(
                            f"\n[INJECT] agent reached chat prompt → sending {user_reply!r}\n",
                            file=sys.stderr,
                        )
                        await user_queue.put({
                            "type": "user",
                            "message": {"role": "user", "content": user_reply},
                        })
                        reply_sent = True
        if hasattr(message, "subtype") and getattr(message, "subtype", None) == "success":
            final_text = getattr(message, "result", "")
            await user_queue.put(None)

    print(f"\n=== Result ===", file=sys.stderr)
    print(f"Counts: {counts}", file=sys.stderr)
    print(f"Reply sent: {reply_sent}", file=sys.stderr)
    print(f"Final: {final_text!r}", file=sys.stderr)


if __name__ == "__main__":
    asyncio.run(main())
