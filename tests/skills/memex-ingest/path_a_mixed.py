#!/usr/bin/env python3
"""Path A with per-slug policy: drop one, rename one, accept one.

`path_a.py` applies a single policy to all proposals. This driver tests
the realistic case where the user mixes actions in one Path A review:
drop one slug, rename another, accept the third — all in a single
batched AskUserQuestion call. Verifies that the agent correctly:

1. Issues per-proposal AskUserQuestion (3 per-proposal questions, one batched call).
2. Receives different answers per slug.
3. Applies each via Edit tool to the plan JSON (drop sets `dropped:true`,
   rename swaps the `slug` field).
4. Plan apply commits the right subset.

Configure via PER_SLUG and RENAMES below; defaults match the canonical
`/tmp/edge-multi.md` fixture (go / python / rust).
"""
import asyncio
import os
import sys

from _common import build_options, prepend_bin_to_path
from claude_agent_sdk import query
from claude_agent_sdk.types import PermissionResultAllow

PER_SLUG = {"go": "drop", "python": "rename", "rust": "accept"}
RENAMES = {"python": "py3"}


def pick(question: dict) -> str:
    header = question.get("header", "").lower()
    text = question.get("question", "").lower()
    options = question.get("options", []) or []
    labels = [o.get("label", "") for o in options]

    for slug, action in PER_SLUG.items():
        if slug in header or slug in text:
            for lbl in labels:
                if action in lbl.lower():
                    return lbl

    if "new slug" in text or "rename" in text or "name" in header:
        for slug, new in RENAMES.items():
            if slug in text or slug in header:
                return new
        return next(iter(RENAMES.values()), "renamed")

    for lbl in labels:
        if "apply" in lbl.lower() or "commit" in lbl.lower():
            return lbl
    return labels[0] if labels else "Accept"


def make_can_use_tool():
    askq_calls = []

    async def can_use_tool(tool_name, input_data, context):
        if tool_name == "AskUserQuestion":
            questions = input_data.get("questions", [])
            askq_calls.append(questions)
            answers = {}
            for q in questions:
                chosen = pick(q)
                answers[q.get("question", "")] = chosen
                print(
                    f"[AskUserQuestion] header={q.get('header','')!r} "
                    f"options={[o.get('label') for o in q.get('options',[])]} → {chosen!r}",
                    file=sys.stderr,
                )
            return PermissionResultAllow(
                updated_input={"questions": questions, "answers": answers}
            )
        return PermissionResultAllow(updated_input=input_data)

    return can_use_tool, askq_calls


async def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <test_file>", file=sys.stderr)
        print(
            f"Edit PER_SLUG / RENAMES at the top of {os.path.basename(__file__)} "
            "to match your fixture's slugs.",
            file=sys.stderr,
        )
        sys.exit(2)
    test_file = sys.argv[1]

    can_use_tool, askq_calls = make_can_use_tool()
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
    print(
        f"Spawning SDK session (Path A mixed: PER_SLUG={PER_SLUG} RENAMES={RENAMES})",
        file=sys.stderr,
    )
    async for message in query(prompt=prompt_stream(), options=options):
        if hasattr(message, "subtype") and getattr(message, "subtype", None) == "success":
            final_text = getattr(message, "result", "")

    print(f"\n=== Result ===", file=sys.stderr)
    print(f"AskUserQuestion calls: {len(askq_calls)}", file=sys.stderr)
    print(f"Final: {final_text!r}", file=sys.stderr)


if __name__ == "__main__":
    asyncio.run(main())
