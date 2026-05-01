"""Shared SDK setup for the memex-ingest skill drivers.

Both `path_a.py` and `path_b.py` use this module for repo-path
resolution, plugin loading, and the no-op PreToolUse hook.
"""
import os

from claude_agent_sdk import ClaudeAgentOptions
from claude_agent_sdk.types import HookMatcher

REPO_ROOT = os.path.dirname(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
)
PLUGIN_DIR = os.path.join(REPO_ROOT, "plugin")
BIN_DIR = os.path.join(REPO_ROOT, "target", "release")

DEFAULT_TOOLS = [
    "Bash", "Edit", "Read", "Write", "Glob", "Grep", "Skill", "ToolSearch",
]


async def noop_hook(input_data, tool_use_id, context):
    return {"continue_": True}


def prepend_bin_to_path():
    """Make the worktree's `memex` binary findable for the agent's Bash calls."""
    os.environ["PATH"] = BIN_DIR + ":" + os.environ.get("PATH", "")


def build_options(*, can_use_tool, allow_askuserquestion: bool) -> ClaudeAgentOptions:
    """Construct ClaudeAgentOptions with the worktree plugin loaded.

    The `plugins=[{type:local, path:...}]` parameter is required to load
    the worktree's SKILL.md instead of the (stale) marketplace copy at
    ~/.claude/local-marketplaces/memex-dev/.
    """
    tools = list(DEFAULT_TOOLS)
    if allow_askuserquestion:
        tools.append("AskUserQuestion")
    return ClaudeAgentOptions(
        can_use_tool=can_use_tool,
        hooks={"PreToolUse": [HookMatcher(matcher=None, hooks=[noop_hook])]},
        allowed_tools=tools,
        plugins=[{"type": "local", "path": PLUGIN_DIR}],
    )
