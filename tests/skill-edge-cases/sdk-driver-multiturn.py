#!/usr/bin/env python3
"""Path B multi-turn: deny AskUserQuestion to force chat fallback,
then watch for the directive prompt in agent output and inject a
follow-up `apply` user message to drive the apply step.
"""
import asyncio
import os
import sys

sys.path.insert(0, "/tmp/skill-sdk-venv/lib/python3.14/site-packages")
from claude_agent_sdk import ClaudeAgentOptions, query
from claude_agent_sdk.types import HookMatcher, PermissionResultAllow, PermissionResultDeny

_REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
PLUGIN_DIR = os.path.join(_REPO_ROOT, "plugin")
NEW_BIN_DIR = os.path.join(_REPO_ROOT, "target", "release")


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


async def dummy_hook(input_data, tool_use_id, context):
    return {"continue_": True}


async def main():
    test_file = sys.argv[1]
    user_reply = sys.argv[2] if len(sys.argv) > 2 else "apply"

    can_use_tool, counts = make_can_use_tool()
    os.environ["PATH"] = NEW_BIN_DIR + ":" + os.environ.get("PATH", "")

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

    options = ClaudeAgentOptions(
        can_use_tool=can_use_tool,
        hooks={"PreToolUse": [HookMatcher(matcher=None, hooks=[dummy_hook])]},
        allowed_tools=["Bash", "Edit", "Read", "Write", "Glob", "Grep", "Skill", "ToolSearch"],
        plugins=[{"type": "local", "path": PLUGIN_DIR}],
    )

    final_text = None
    print(f"Spawning SDK session (Path B multi-turn, reply={user_reply!r})", file=sys.stderr)

    async for message in query(prompt=prompt_stream(), options=options):
        mt = type(message).__name__
        if mt == "AssistantMessage":
            for c in getattr(message, "content", []):
                if hasattr(c, "text") and c.text:
                    text = c.text
                    # Detect the chat directive prompt; inject reply
                    if not reply_sent and "Reply `apply`" in text and "drop" in text.lower():
                        print(f"\n[INJECT] agent reached chat prompt → sending {user_reply!r}\n", file=sys.stderr)
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
