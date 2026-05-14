"""Hermes hook: auto-ingest finished sessions into memex.

Hermes emits `session:start` with both `session_id` and `session_key`, but
`session:end` and `session:reset` only carry `session_key`. We remember the
mapping by writing a tiny per-session marker file at
`~/.hermes/hooks/memex/state/<safe_key>.id` containing the session_id, then
read+unlink it on end/reset. One file per active session — O(1) per event,
and an orphan from a never-ended session is one tiny file that gets reaped
the next time the hook loads (entries older than 7 days are removed).

Hermes does not rename the session file on end/reset (each session writes
to its own `session_<session_id>.json`), so the live path is safe to hand
to `memex ingest` — no snapshot needed.

Failures are swallowed — the hook never raises and never blocks hermes.
"""
from __future__ import annotations

import os
import re
import stat
import subprocess
import time
from pathlib import Path

HERMES_HOME = Path(os.environ.get("HERMES_HOME") or (Path.home() / ".hermes"))
SESSIONS_DIR = HERMES_HOME / "sessions"
STATE_DIR = HERMES_HOME / "hooks" / "memex" / "state"
ORPHAN_MAX_AGE_SECS = 7 * 24 * 60 * 60
_SAFE_KEY_RE = re.compile(r"[^A-Za-z0-9._-]")


def _marker_path(session_key: str) -> Path:
    return STATE_DIR / f"{_SAFE_KEY_RE.sub('_', session_key)}.id"


def _reap_orphans() -> None:
    if not STATE_DIR.is_dir():
        return
    cutoff = time.time() - ORPHAN_MAX_AGE_SECS
    for entry in STATE_DIR.iterdir():
        try:
            # lstat (not is_file) so a symlink left in STATE_DIR can't redirect
            # the unlink at an arbitrary file the user happens to own.
            st = entry.lstat()
            if stat.S_ISREG(st.st_mode) and st.st_mtime < cutoff:
                entry.unlink()
        except Exception:
            pass


def _ingest(session_id: str) -> None:
    src = SESSIONS_DIR / f"session_{session_id}.json"
    if not src.exists():
        return
    try:
        subprocess.Popen(
            ["memex", "ingest", "--agent", "hermes", str(src)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )
    except Exception:
        pass


async def handle(event_type: str, context: dict) -> None:
    session_key = context.get("session_key")
    session_id = context.get("session_id")

    if event_type == "session:start" and session_key and session_id:
        try:
            STATE_DIR.mkdir(parents=True, exist_ok=True)
            _marker_path(session_key).write_text(session_id, encoding="utf-8")
        except Exception:
            pass
        _reap_orphans()
        return

    if event_type in ("session:end", "session:reset") and session_key:
        marker = _marker_path(session_key)
        try:
            sid = marker.read_text(encoding="utf-8").strip()
        except Exception:
            return
        try:
            marker.unlink()
        except Exception:
            pass
        if sid:
            _ingest(sid)
