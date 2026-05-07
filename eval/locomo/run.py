#!/usr/bin/env python3
"""LoCoMo evaluation for Memex using the `memex` CLI.

Ingestion goes through `memex ingest` and queries through `memex query`.
The backing agent (claude-code, codex, gemini-cli) and model are selected
via daemon config env vars so the benchmark can still compare agents.

Reuses judge prompt, metrics, checkpointing from
github.com/mem0ai/memory-benchmarks (submodule at eval/memory-benchmarks).
Output is conformant with the memory-benchmarks web UI.

Usage:
# Default daemon backend (claude-code)
    python3 eval/locomo/run.py --project-name memex-sonnet \
        --judge-model gpt-5 --conversations 0

    # Override daemon backend + model
    python3 eval/locomo/run.py --project-name memex-codex \
        --backend codex --model gpt-5.4-mini --judge-model gpt-5
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import shutil
import subprocess
import sys
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

# --- Submodule imports ---
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "memory-benchmarks"))

from benchmarks.locomo.prompts import (  # noqa: E402
    CATEGORY_NAMES,
    JUDGE_SYSTEM_PROMPT,
    get_answer_generation_prompt,
    get_judge_prompt,
    get_judge_prompt_with_evidence,
    preprocess_answer,
)
from benchmarks.locomo.run import (  # noqa: E402
    get_sorted_sessions,
    download_dataset,
    compute_locomo_metrics,
    display_results,
    load_evidence_lookup,
)
from benchmarks.common.llm_client import LLMClient  # noqa: E402
from benchmarks.common.utils import (  # noqa: E402
    GracefulShutdown,
    cutoff_label,
    parse_cutoffs,
    save_result_json,
    setup_logging,
)


# ---------------------------------------------------------------------------
# memex CLI wrapper
# ---------------------------------------------------------------------------

MEMEX_BIN = os.environ.get("MEMEX_BIN") or shutil.which("memex") or "memex"

# Keep all generated artefacts (results, logs) under eval/ regardless of CWD.
EVAL_DIR = Path(__file__).resolve().parent.parent


def _daemon_env(memex_root, backend=None, model=None, worker_count=None):
    """Build subprocess env: MEMEX_ROOT + optional daemon-worker overrides."""
    env = os.environ.copy()
    env["MEMEX_ROOT"] = str(memex_root)
    if backend is not None:
        env["MEMEX__DAEMON__WORKER__BACKEND"] = backend
    if model is not None:
        env["MEMEX__DAEMON__WORKER__MODEL"] = model
    if worker_count is not None:
        env["MEMEX__DAEMON__WORKER__MAX_COUNT"] = str(worker_count)
    return env


def memex_backfill(memex_root, staging_dir, agent_format="claude-code",
                   backend=None, model=None, timeout=21600, worker_count=None):
    """Run `memex backfill <agent_format> --path <dir>`.

    The daemon auto-spawns on first call and dispatches all discovered
    *.jsonl files. Real parallelism is bounded by the daemon's worker pool
    (`daemon.worker.max_count`, default: CPU count; override via
    MEMEX__DAEMON__WORKER__MAX_COUNT or the `worker_count` arg here).
    Daemon-side content-hash dedup makes this idempotent: re-runs skip
    already-ingested sessions.
    """
    env = _daemon_env(memex_root, backend=backend, model=model,
                      worker_count=worker_count)
    t0 = time.time()
    try:
        cmd = [MEMEX_BIN, "backfill", agent_format, "--path", str(staging_dir)]
        r = subprocess.run(
            cmd,
            capture_output=True, text=True,
            env=env, timeout=timeout,
        )
        return r.returncode, r.stdout, r.stderr, int((time.time() - t0) * 1000)
    except subprocess.TimeoutExpired:
        return 1, "", "TIMEOUT", int((time.time() - t0) * 1000)


_EXPANSION_RE = re.compile(r"^Expansion:[ \t]*\n(?:[ \t]+\S.*\n?)*", re.MULTILINE)


def _parse_query_stdout(stdout):
    """Strip the leading Expansion block and trailing Citations block, return the answer text."""
    text = _EXPANSION_RE.sub("", stdout, count=1)
    idx = text.rfind("\nCitations:")
    if idx != -1:
        text = text[:idx]
    return text.strip()


def _split_raw_context_entries(raw_context):
    """Parse the JSON array emitted by `memex query --raw`.

    Each element is an entry dict (rank/doc_type/signal/id/title/body);
    `doc_type` may be `wiki` or `source`. Falls back to an empty list
    on parse error.
    """
    s = raw_context.strip() if raw_context else ""
    if not s:
        return []
    try:
        parsed = json.loads(s)
    except json.JSONDecodeError:
        return []
    return parsed if isinstance(parsed, list) else []


def _slice_raw_context_entries(entries, cutoff):
    """Return the first `cutoff` entry dicts as a JSON array string."""
    if cutoff <= 0:
        return ""
    return json.dumps(entries[:cutoff], indent=2)


def memex_query(memex_root, question, top_k=20, raw=False,
                 backend=None, model=None, timeout=120):
    """Run `memex query <question>` and return (answer, rc, wall_ms).

    When `raw=True`, skip memex's synthesis step and return the retrieved
    context (top-k source chunks with rank/doc_type/signal headers) as
    the answer. Tests retrieval quality independent of synthesis.
    """
    env = _daemon_env(memex_root, backend=backend, model=model)
    cmd = [MEMEX_BIN, "query", question, "--top-k", str(top_k)]
    if raw:
        cmd.append("--raw")
    t0 = time.time()
    try:
        r = subprocess.run(
            cmd, capture_output=True, text=True, env=env, timeout=timeout,
        )
        wall_ms = int((time.time() - t0) * 1000)
        if r.returncode != 0:
            return "", r.returncode, wall_ms
        if raw:
            # Raw mode: stdout is the concatenated context. Pass through as
            # the answer — the judge then evaluates whether the gold info is
            # present in the retrieved context.
            return r.stdout.strip(), r.returncode, wall_ms
        return _parse_query_stdout(r.stdout), r.returncode, wall_ms
    except subprocess.TimeoutExpired:
        return "TIMEOUT", 1, int((time.time() - t0) * 1000)


def memex_daemon_stop(memex_root):
    """Stop the per-conv daemon so the next conv starts fresh."""
    env = _daemon_env(memex_root)
    try:
        subprocess.run([MEMEX_BIN, "daemon", "stop"],
                       capture_output=True, text=True, env=env, timeout=15)
    except subprocess.TimeoutExpired:
        pass


# ---------------------------------------------------------------------------
# Session → Claude Code JSONL
# ---------------------------------------------------------------------------

def _turn_text(turn):
    text = turn.get("text", "")
    blip = turn.get("blip_caption", "")
    query = turn.get("query", "")
    if query and blip:
        photo_tag = f"[Sharing image - query: {query}. The image shows: {blip}]"
    elif query:
        photo_tag = f"[Sharing image - query for: {query}]"
    elif blip:
        photo_tag = f"[Sharing image that shows: {blip}]"
    else:
        photo_tag = ""
    if photo_tag:
        text = f"{text} {photo_tag}" if text else photo_tag
    return text


def _parse_locomo_session_datetime(date_str):
    """Parse LoCoMo session date like '1:56 pm on 8 May, 2023' as UTC."""
    s = (date_str or "").strip()
    for fmt in ("%I:%M %p on %d %B, %Y", "%I:%M %p on %d %b, %Y"):
        try:
            return datetime.strptime(s, fmt).replace(tzinfo=timezone.utc)
        except ValueError:
            continue
    return None


def _turn_timestamp_iso(session_dt, dia_id, fallback_index):
    """Return the session timestamp for every turn.

    LoCoMo provides one datetime per session; per-turn times are not in the
    dataset. Mem0's reference harness (`eval/memory-benchmarks/.../locomo/run.py`)
    passes the session epoch to every turn for the same reason. We previously
    synthesized per-turn seconds from the dia_id suffix, which implied a
    precision the dataset doesn't support and could mislead the extractor
    into inferring spurious durations between adjacent Timeline entries.
    Dialogue order is already preserved by JSONL line order; sort-keying
    uses `dia_id` (see `_dia_id_sort_key`) rather than the timestamp.
    """
    del dia_id, fallback_index
    if session_dt is None:
        return None
    return session_dt.isoformat().replace("+00:00", "Z")


def _dia_id_sort_key(dia_id, fallback_index):
    """Sort key for LoCoMo dia_id like D1:2; malformed/missing ids fall back."""
    m = re.match(r"^[A-Za-z]*(\d+):(\d+)$", str(dia_id or ""))
    if m:
        return (0, int(m.group(1)), int(m.group(2)), fallback_index)
    return (1, fallback_index, 0, fallback_index)


def write_session_jsonl(path, session_num, date_str, turns, conv_idx, speaker_a, speaker_b):
    """Write a LoCoMo session as a Claude Code-format JSONL transcript.

    LoCoMo roles are deterministic: speaker_a -> user, speaker_b -> assistant.
    """
    session_id = f"locomo-conv{conv_idx}-session{session_num}"
    session_dt = _parse_locomo_session_datetime(date_str)
    session_iso = (
        session_dt.isoformat().replace("+00:00", "Z")
        if session_dt is not None
        else None
    )
    has_assistant_turn = False

    with open(path, "w") as f:
        ordered_turns = sorted(
            enumerate(turns),
            key=lambda p: _dia_id_sort_key(p[1].get("dia_id", ""), p[0]),
        )
        for i, (_, t) in enumerate(ordered_turns, start=1):
            speaker = t.get("speaker", "unknown")
            turn_ts = _turn_timestamp_iso(session_dt, t.get("dia_id", ""), i - 1)
            text = _turn_text(t)
            if not text:
                continue
            if speaker == speaker_a:
                turn_type = "user"
            elif speaker == speaker_b:
                turn_type = "assistant"
            else:
                # LoCoMo should only contain speaker_a/speaker_b.
                # Keep unknown speakers visible but mapped to assistant to avoid
                # dropping turns.
                turn_type = "assistant"
            if turn_type == "user":
                obj = {
                    "type": "user",
                    "sessionId": session_id,
                    "message": {"role": speaker, "content": text},
                }
                if turn_ts is not None:
                    obj["timestamp"] = turn_ts
                f.write(json.dumps(obj) + "\n")
            else:
                has_assistant_turn = True
                obj = {
                    "type": "assistant",
                    "sessionId": session_id,
                    "message": {
                        "role": speaker,
                        "content": [{"type": "text", "text": text}],
                    },
                }
                if turn_ts is not None:
                    obj["timestamp"] = turn_ts
                f.write(json.dumps(obj) + "\n")

    # Ensure at least one assistant turn (required by parse_claude_code_session's
    # non-substantive filter). Insert a stub if the session had only one speaker.
    if not has_assistant_turn:
        with open(path, "a") as f:
            stub = {
                "type": "assistant",
                "sessionId": session_id,
                "message": {"role": speaker_b, "content": [{"type": "text", "text": "(end of session)"}]},
            }
            if session_iso is not None:
                stub["timestamp"] = session_iso
            f.write(json.dumps(stub) + "\n")


# ---------------------------------------------------------------------------
# Ingestion
# ---------------------------------------------------------------------------

def stage_conversation_sessions(conv_idx, entry, staging_dir, max_sessions=None):
    """Write session JSONLs for one conversation. No daemon calls.

    Returns (path_to_session_subdir, total_turn_count).
    """
    conversation = entry["conversation"]
    speaker_a = conversation["speaker_a"]
    speaker_b = conversation["speaker_b"]
    sorted_sessions = get_sorted_sessions(conversation)
    if max_sessions is not None:
        sorted_sessions = sorted_sessions[:max_sessions]
    # Lay out as `<staging>/projects/conv_<N>/session_<M>.jsonl` so the
    # claude-code agent glob (`projects/*/*.jsonl`) matches under --path.
    session_dir = Path(staging_dir) / "projects" / f"conv_{conv_idx}"
    session_dir.mkdir(parents=True, exist_ok=True)

    turn_count = 0
    for session_key, date_str, turns in sorted_sessions:
        if not turns:
            continue
        num = session_key.replace("session_", "")
        path = session_dir / f"session_{num}.jsonl"
        write_session_jsonl(path, num, date_str, turns, conv_idx, speaker_a, speaker_b)
        turn_count += len(turns)
    return session_dir, turn_count


# ---------------------------------------------------------------------------
# Search + Answer + Judge
# ---------------------------------------------------------------------------

def _build_ev_ctx(evidence_lookup, conv_idx, evidence_refs):
    """Concatenate gold-evidence snippets for a question (empty if none)."""
    if not evidence_lookup:
        return ""
    parts = []
    for ref in evidence_refs:
        snippet = evidence_lookup.get((conv_idx, ref))
        if snippet:
            parts.append(snippet)
    return "\n".join(parts).strip()


async def _judge_answer(judge_llm, category, question, gold, generated_answer,
                        ev_ctx, logger=None, question_id=""):
    """Run the judge once. Returns (correct: bool, reason: str)."""
    if ev_ctx:
        prompt = get_judge_prompt_with_evidence(
            category, question, gold, generated_answer, ev_ctx)
    else:
        prompt = get_judge_prompt(category, question, gold, generated_answer)
    raw = await judge_llm.generate_structured(
        system=JUDGE_SYSTEM_PROMPT, user=prompt)
    if not isinstance(raw, dict) or "label" not in raw:
        if logger is not None:
            logger.warning(
                "[%s] judge returned malformed response (no label): %r",
                question_id, raw)
        return False, ""
    correct = raw.get("label", "").upper() == "CORRECT"
    return correct, raw.get("reasoning", "")


async def eval_raw_cutoffs(*, question, category, gold, ev_ctx, raw_entries,
                            cutoffs, answerer_llm, judge_llm, predict_only,
                            logger=None, question_id=""):
    """Run answerer + judge for each cutoff against `raw_entries`.

    Cutoffs that produce identical slices (e.g. cutoff > len(raw_entries))
    share one answerer+judge call; the result is replicated across the
    cutoffs in that group. Saves ~40% of LLM calls when cutoff list spans
    above the actual retrieval size.
    """
    slice_groups = {}
    for c in cutoffs:
        slice_groups.setdefault(min(c, len(raw_entries)), []).append(c)

    async def eval_slice(slice_size, cutoffs_for_slice):
        sliced_context = _slice_raw_context_entries(raw_entries, slice_size)

        if sliced_context:
            if answerer_llm is not None:
                gen_prompt = get_answer_generation_prompt(
                    question, [{"memory": sliced_context}],
                    reference_date="2023")
                generated_answer = await answerer_llm.generate(system="", user=gen_prompt)
                if "ANSWER:" in generated_answer:
                    generated_answer = generated_answer.rsplit("ANSWER:", 1)[-1].strip()
            else:
                generated_answer = sliced_context
        else:
            generated_answer = "I don't know"

        if predict_only:
            return [
                (cutoff_label(c), {
                    "generated_answer": generated_answer,
                    "memories_evaluated": slice_size,
                })
                for c in cutoffs_for_slice
            ]

        correct, reason = await _judge_answer(
            judge_llm, category, question, gold, generated_answer,
            ev_ctx, logger=logger, question_id=question_id)
        return [
            (cutoff_label(c), {
                "judgment": "CORRECT" if correct else "WRONG",
                "score": 1.0 if correct else 0.0,
                "generated_answer": generated_answer,
                "memories_evaluated": slice_size,
                "reason": reason,
            })
            for c in cutoffs_for_slice
        ]

    groups = await asyncio.gather(
        *[eval_slice(s, cs) for s, cs in slice_groups.items()])
    return dict(item for group in groups for item in group)


async def process_question(qa, qa_idx, conv_idx, memex_root,
                           judge_llm, answerer_llm, cutoffs, logger,
                           predict_only=False, evidence_lookup=None,
                           backend=None, model=None, raw_query=False,
                           top_k=200, retrieval_sem=None):
    question_id = f"conv{conv_idx}_q{qa_idx}"
    question = qa["question"]
    category = qa["category"]
    gold = preprocess_answer(category, str(qa.get("answer", "")))

    result = {
        "question_id": question_id,
        "conversation_idx": conv_idx,
        "category": category,
        "category_name": CATEGORY_NAMES.get(category, "unknown"),
        "question": question,
        "ground_truth_answer": gold,
        "evidence": qa.get("evidence", []),
        "input_tokens": 0,
        "output_tokens": 0,
        "api_ms": 0,
        "num_turns": 0,
        "search_latency_ms": 0,
    }

    ev_ctx = _build_ev_ctx(evidence_lookup, conv_idx, qa.get("evidence", []))

    async def gated_retrieve(top_k_arg, raw):
        """Run memex_query under retrieval_sem if provided (sem caps daemon
        concurrency; LLM calls run unbounded so they finish as fast as the
        per-client AsyncLimiter(rpm) allows)."""
        async def _run():
            return await asyncio.to_thread(
                memex_query, memex_root, question, top_k=top_k_arg,
                raw=raw, backend=backend, model=model)
        if retrieval_sem is not None:
            async with retrieval_sem:
                return await _run()
        return await _run()

    cutoff_results = {}
    if raw_query:
        raw_answer, rc, wall_ms = await gated_retrieve(top_k, raw=True)
        result["search_latency_ms"] = wall_ms
        raw_entries = []
        if rc != 0:
            logger.warning("[%s] memex_query rc=%d (%dms): %s",
                           question_id, rc, wall_ms, raw_answer[:200])
        elif raw_answer == "TIMEOUT":
            logger.warning("[%s] memex_query TIMEOUT (%dms)", question_id, wall_ms)
        elif raw_answer == "":
            logger.warning("[%s] memex_query returned empty (%dms)",
                           question_id, wall_ms)
        else:
            raw_entries = _split_raw_context_entries(raw_answer)
            if not raw_entries:
                logger.warning(
                    "[%s] memex_query output unparseable as JSON array; "
                    "falling back to empty retrieval", question_id)
        result["retrieval"] = {
            "search_query": question,
            "search_results": raw_entries,
            "search_latency_ms": wall_ms,
            "total_results": len(raw_entries),
        }
        cutoff_results = await eval_raw_cutoffs(
            question=question, category=category, gold=gold, ev_ctx=ev_ctx,
            raw_entries=raw_entries, cutoffs=cutoffs,
            answerer_llm=answerer_llm, judge_llm=judge_llm,
            predict_only=predict_only,
            logger=logger, question_id=question_id)
    else:
        async def eval_cutoff_synth(c):
            label = cutoff_label(c)
            generated_answer, rc, wall_ms = await gated_retrieve(c, raw=False)
            if rc != 0:
                logger.warning(
                    "[%s] memex_query rc=%d at cutoff %d (%dms): %s",
                    question_id, rc, c, wall_ms, generated_answer[:200])
                generated_answer = "I don't know"
            elif generated_answer in ("", "TIMEOUT"):
                logger.warning(
                    "[%s] memex_query empty/TIMEOUT at cutoff %d (%dms)",
                    question_id, c, wall_ms)
                generated_answer = "I don't know"

            if predict_only:
                return label, wall_ms, {
                    "generated_answer": generated_answer,
                    "memories_evaluated": c,
                }

            correct, reason = await _judge_answer(
                judge_llm, category, question, gold, generated_answer,
                ev_ctx, logger=logger, question_id=question_id)
            return label, wall_ms, {
                "judgment": "CORRECT" if correct else "WRONG",
                "score": 1.0 if correct else 0.0,
                "generated_answer": generated_answer,
                "memories_evaluated": c,
                "reason": reason,
            }

        gathered = await asyncio.gather(*[eval_cutoff_synth(c) for c in cutoffs])
        total_wall_ms = sum(w for _, w, _ in gathered)
        cutoff_results = {label: r for label, _, r in gathered}
        result["search_latency_ms"] = total_wall_ms

    result["cutoff_results"] = cutoff_results
    return result


# ---------------------------------------------------------------------------
# CLI + Main
# ---------------------------------------------------------------------------

AGENT_FORMATS = ("claude-code", "codex", "gemini-cli", "openai-api")


def parse_args():
    p = argparse.ArgumentParser(
        description="LoCoMo benchmark for Memex (reuses github.com/mem0ai/memory-benchmarks)")
    p.add_argument("--project-name", required=True,
                   help="Identifier for this run; used in output paths and log names.")
    p.add_argument("--backend", default="openai-api", choices=AGENT_FORMATS,
                   help="Daemon worker backend (sets MEMEX__DAEMON__WORKER__BACKEND).")
    p.add_argument("--model", default=None,
                   help="Daemon worker model (sets MEMEX__DAEMON__WORKER__MODEL). "
                        "If unset, the daemon picks the backend-appropriate "
                        "default (claude-sonnet-4-6 for claude-code, gpt-5.4-mini "
                        "for codex/openai-api, gemini-3-flash-preview for gemini-cli).")
    p.add_argument("--judge-model", default="gpt-5",
                   help="Model used to judge answers.")
    p.add_argument("--judge-provider", default="openai",
                   help="Provider for the judge model.")
    p.add_argument("--raw-query", action="store_true",
                   help="Use raw retrieved context as the answer (skips memex synthesis).")
    p.add_argument("--answerer-model", default=None,
                   help="Answerer model in --raw-query mode (default: --judge-model).")
    p.add_argument("--answerer-provider", default=None,
                   help="Answerer provider in --raw-query mode (default: --judge-provider).")
    p.add_argument("--conversations", default="0,1,2,3,4,5,6,7,8,9",
                   help="Comma-separated conversation indices to run.")
    p.add_argument("--top-k", type=int, default=200,
                   help="Entries retrieved per query (must be >= max cutoff).")
    p.add_argument("--top-k-cutoffs", default="10,20,50,200",
                   help="Comma-separated cutoffs to score.")
    p.add_argument("--dataset-path", default=None,
                   help="Path to locomo10.json (default: auto-download).")
    p.add_argument("--output-dir", default=str(EVAL_DIR / "results" / "locomo"),
                   help="Output directory (default: <eval>/results/locomo).")
    p.add_argument("--predict-only", action="store_true",
                   help="Generate answers without judging.")
    p.add_argument("--evaluate-only", action="store_true",
                   help="Re-judge existing answers; skip retrieval and answerer.")
    p.add_argument("--rejudge", action="store_true",
                   help="With --evaluate-only: re-judge even if results already exist.")
    p.add_argument("--reanswer", action="store_true",
                   help="Regenerate answers from cached retrieval (useful for "
                        "adding new cutoffs or swapping the answerer model).")
    p.add_argument("--resume", action="store_true",
                   help="Skip questions that already have a result file.")
    p.add_argument("--debug", action="store_true",
                   help="Verbose logging.")
    p.add_argument("--run-id", default=None,
                   help="Reuse a specific run ID for resume.")
    p.add_argument("--categories", default="1,2,3,4",
                   help="Comma-separated category numbers to include.")
    p.add_argument("--with-evidence", action="store_true",
                   help="Pass gold evidence to the judge prompt.")
    p.add_argument("--max-sessions", type=int, default=None,
                   help="Max sessions per conv (for quick testing).")
    p.add_argument("--max-questions", type=int, default=None,
                   help="Max questions per conv (for quick testing).")
    p.add_argument("--max-workers", type=int, default=os.cpu_count() or 4,
                   help="Caps parallel conv backfills and concurrent retrievals. "
                        "LLM calls are bounded by --rpm instead. Default: CPU count.")
    p.add_argument("--rpm", type=int, default=200,
                   help="Requests/min cap per LLM client. Raise on tier-3+ keys.")
    p.add_argument("--score-debug", action="store_true",
                   help="Include score breakdowns in output.")
    p.add_argument("--keep-daemon", action="store_true",
                   help="Leave per-conv daemons running after queries finish.")
    return p.parse_args()


async def async_main():
    args = parse_args()
    log_dir = EVAL_DIR / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    log_file = str(log_dir / f"memex-locomo_{datetime.now().strftime('%Y%m%d_%H%M%S')}.log")
    logger = setup_logging("memex-locomo", log_file=log_file, debug=args.debug)

    cutoffs = parse_cutoffs(args.top_k_cutoffs)
    if args.raw_query and args.top_k < max(cutoffs):
        raise SystemExit(
            f"--top-k ({args.top_k}) must be >= max cutoff ({max(cutoffs)})")
    categories = [int(c) for c in args.categories.split(",")]
    indices = [int(c) for c in args.conversations.split(",")]

    output_dir = os.path.join(args.output_dir, f"predicted_{args.project_name}")
    os.makedirs(output_dir, exist_ok=True)

    # One MEMEX_ROOT per conversation — each conv is a closed world. Sharing
    # a root across convs collided identically-named speakers ("John" appears
    # in 3 locomo convs as 3 different people) into one wiki page, because
    # ingest-path dedup is global. Per-conv roots sidestep that entirely.
    # Honor $MEMEX_ROOT as the *base* directory; each conv gets a subdir
    # below it. AF_UNIX sockets cap at ~108 bytes, so keep the suffix short.
    memex_root_base = Path(os.environ["MEMEX_ROOT"]) if os.environ.get("MEMEX_ROOT") else (
        Path(os.environ.get("TMPDIR", "/tmp")) / f"memex-locomo-{args.project_name}")
    memex_root_base.mkdir(parents=True, exist_ok=True)

    def memex_root_for_conv(conv_idx):
        r = memex_root_base / f"conv_{conv_idx}"
        r.mkdir(parents=True, exist_ok=True)
        return r

    agent_label = args.backend or "daemon-default"
    if args.model:
        agent_label = f"{agent_label}-{args.model}"
    print(f"MEMEX LoCoMo Benchmark | project={args.project_name}")
    print(f"  Memex binary: {MEMEX_BIN}")
    print(f"  Daemon backend: {agent_label}")
    print(f"  Judge: {args.judge_model} ({args.judge_provider})")
    print(f"  Conversations: {args.conversations}")
    print(f"  Top-k: {args.top_k}")
    print(f"  Cutoffs: {args.top_k_cutoffs}")
    if args.max_questions:
        print(f"  Max questions: {args.max_questions}")
    if args.predict_only:
        print(f"  Mode: predict-only (no judging)")
    if args.evaluate_only:
        print(f"  Mode: evaluate-only (re-judge existing results)")
    print(f"  Output: {output_dir}")
    print(f"  Wiki root base: {memex_root_base} (per-conv subdirs)")

    if args.dataset_path:
        dataset_path = args.dataset_path
    else:
        dataset_path = download_dataset(str(EVAL_DIR / "datasets" / "locomo"), logger)
    with open(dataset_path) as f:
        dataset = json.load(f)

    # Build evidence lookup if requested
    evidence_lookup = None
    if args.with_evidence:
        evidence_lookup = load_evidence_lookup(dataset_path)
        print(f"  Evidence lookup: {len(evidence_lookup)} entries")

    answerer_model = args.answerer_model or args.judge_model
    answerer_provider = args.answerer_provider or args.judge_provider

    judge_llm = None
    answerer_llm = None
    if not args.predict_only:
        judge_llm = LLMClient(model=args.judge_model, provider=args.judge_provider,
                              rpm=args.rpm)
    if args.raw_query or args.reanswer:
        answerer_llm = LLMClient(
            model=answerer_model,
            provider=answerer_provider,
            rpm=args.rpm)
    shutdown = GracefulShutdown()

    all_evaluations = []

    # --evaluate-only: re-judge existing predict results
    if args.evaluate_only:
        sem = asyncio.Semaphore(max(1, args.max_workers))

        async def rejudge_one(p):
            async with sem:
                try:
                    text = await asyncio.to_thread(Path(p).read_text)
                    data = json.loads(text)
                except (json.JSONDecodeError, OSError) as e:
                    logger.warning("Failed to load %s: %s", p, e)
                    return None
                if data.get("category") not in categories:
                    return None
                if data.get("cutoff_results") and not args.rejudge:
                    return data
                gold = data["ground_truth_answer"]
                ev_ctx = _build_ev_ctx(
                    evidence_lookup, data["conversation_idx"],
                    data.get("evidence", []))
                qid = data.get("question_id", "")

                async def judge_cutoff(c):
                    label = cutoff_label(c)
                    answer = data.get("cutoff_results", {}).get(label, {}).get(
                        "generated_answer", "")
                    if not answer:
                        for cr in data.get("cutoff_results", {}).values():
                            answer = cr.get("generated_answer", "")
                            if answer:
                                break
                    if not answer:
                        return label, None
                    correct, reason = await _judge_answer(
                        judge_llm, data["category"], data["question"], gold,
                        answer, ev_ctx, logger=logger, question_id=qid)
                    return label, {
                        "judgment": "CORRECT" if correct else "WRONG",
                        "score": 1.0 if correct else 0.0,
                        "generated_answer": answer,
                        "memories_evaluated": data.get("cutoff_results", {}).get(
                            label, {}).get("memories_evaluated", 0),
                        "reason": reason,
                    }

                judged = await asyncio.gather(*[judge_cutoff(c) for c in cutoffs])
                new_cutoffs = {label: r for label, r in judged if r is not None}
                if new_cutoffs:
                    data["cutoff_results"] = new_cutoffs
                await asyncio.to_thread(save_result_json, str(p), data)
                return data

        paths = sorted(Path(output_dir).rglob("conv*_q*.json"))
        results = await asyncio.gather(*[rejudge_one(p) for p in paths])
        all_evaluations.extend([r for r in results if r is not None])

        if all_evaluations:
            metrics = compute_locomo_metrics(all_evaluations, cutoffs)
            display_results(metrics, cutoffs)
        print(f"\nRe-judged {len(all_evaluations)} questions")
        return

    # --reanswer: re-run answerer + judge against the cached retrieval in
    # each existing per-question file. Useful when adding cutoffs the
    # original run didn't compute, or swapping the answerer model.
    if args.reanswer:
        sem = asyncio.Semaphore(max(1, args.max_workers))
        skipped = 0

        async def reanswer_one(p):
            nonlocal skipped
            async with sem:
                try:
                    text = await asyncio.to_thread(Path(p).read_text)
                    data = json.loads(text)
                except (json.JSONDecodeError, OSError) as e:
                    logger.warning("Failed to load %s: %s", p, e)
                    return None
                if data.get("category") not in categories:
                    return None
                raw_entries = data.get("retrieval", {}).get("search_results", [])
                if not raw_entries:
                    logger.warning(
                        "No cached retrieval for %s; skipping reanswer", p)
                    skipped += 1
                    return None
                ev_ctx = _build_ev_ctx(
                    evidence_lookup, data["conversation_idx"],
                    data.get("evidence", []))
                # Skip cutoffs that already have results to avoid redundant
                # answerer+judge LLM calls. Only compute missing cutoffs and
                # merge with existing.
                #
                # A cutoff counts as "done" only if it has both an answer
                # AND (when judging) a judgment. Predict-only runs persist
                # `generated_answer` without `judgment`/`score`; without the
                # judgment check, --reanswer would silently treat predict-only
                # cached entries as complete and roll up unjudged metrics
                # as 0.0.
                existing_cutoffs = data.get("cutoff_results") or {}

                def _cutoff_complete(label):
                    cur = existing_cutoffs.get(label) or {}
                    if not cur.get("generated_answer"):
                        return False
                    if not args.predict_only and "judgment" not in cur:
                        return False
                    return True

                missing_cutoffs = [
                    c for c in cutoffs if not _cutoff_complete(cutoff_label(c))
                ]
                if missing_cutoffs:
                    new_results = await eval_raw_cutoffs(
                        question=data["question"],
                        category=data["category"],
                        gold=data["ground_truth_answer"],
                        ev_ctx=ev_ctx,
                        raw_entries=raw_entries,
                        cutoffs=missing_cutoffs,
                        answerer_llm=answerer_llm,
                        judge_llm=judge_llm,
                        predict_only=args.predict_only,
                        logger=logger,
                        question_id=data.get("question_id", ""))
                    merged = dict(existing_cutoffs)
                    merged.update(new_results)
                    data["cutoff_results"] = merged
                    await asyncio.to_thread(save_result_json, p, data)
                return data

        paths = [str(p) for p in sorted(Path(output_dir).rglob("conv*_q*.json"))]
        results = await asyncio.gather(*[reanswer_one(p) for p in paths])
        all_evaluations.extend([r for r in results if r is not None])

        if all_evaluations:
            metrics = compute_locomo_metrics(all_evaluations, cutoffs)
            display_results(metrics, cutoffs)

            timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
            unified_path = os.path.join(
                args.output_dir, f"locomo_results_{timestamp}.json")
            save_result_json(unified_path, {
                "metadata": {
                    "benchmark": "locomo",
                    "project_name": args.project_name,
                    "run_id": args.run_id,
                    "timestamp": timestamp,
                    "judge_model": args.judge_model,
                    "judge_provider": args.judge_provider,
                    "answerer_model": answerer_model if args.raw_query else args.model,
                    "answerer_provider": answerer_provider if args.raw_query else args.backend,
                    "total_questions": len(all_evaluations),
                    "top_k": args.top_k,
                    "top_k_cutoffs": [cutoff_label(c) for c in cutoffs],
                    "categories": categories,
                    "questions": None,
                    "with_evidence": args.with_evidence,
                    "judge_only": False,
                    "previous_results_file": None,
                    "merged_from_questions": None,
                },
                "metrics_by_cutoff": metrics,
                "evaluations": all_evaluations,
            })
            print(f"\nResults saved to: {unified_path}")
        print(f"\nRe-answered {len(all_evaluations)} questions"
              + (f" (skipped {skipped} without cached retrieval)"
                 if skipped else ""))
        return

    if args.resume:
        for p in sorted(Path(output_dir).rglob("conv*_q*.json")):
            try:
                data = json.loads(p.read_text())
                if data.get("category") in categories:
                    all_evaluations.append(data)
            except (json.JSONDecodeError, KeyError):
                continue
        print(f"  Resumed {len(all_evaluations)} existing results")

    existing_ids = {e["question_id"] for e in all_evaluations}

    # Phase 1: stage all session JSONLs (fast, local filesystem).
    valid_indices = [ci for ci in indices if ci < len(dataset)]
    staged_roots = {}
    total_turns = 0
    for conv_idx in valid_indices:
        conv_stage_root = Path(output_dir) / "staged" / f"conv_{conv_idx}"
        conv_stage_root.mkdir(parents=True, exist_ok=True)
        session_dir, turns = stage_conversation_sessions(
            conv_idx, dataset[conv_idx], conv_stage_root,
            max_sessions=args.max_sessions)
        staged_roots[conv_idx] = conv_stage_root
        total_turns += turns
    logger.info("Staged %d conversations, %d total turns", len(valid_indices), total_turns)

    # Phase 2+3: per-conversation backfill→queries pipeline, all convs in
    # parallel. Each conv has its own MEMEX_ROOT so backfill for conv A and
    # queries for conv B (already-backfilled) can overlap safely — they hit
    # different daemons, different DBs, different files.
    #
    # --max-workers is the *total* agent-subprocess budget for backfill and
    # the total in-flight-question budget for queries (disjoint pools: agents
    # are local claude subprocesses; questions are OpenAI requests in
    # raw_query mode). Split the backfill budget across parallel convs:
    #   conv_parallelism = min(max_workers, #convs)
    #   per_daemon_workers = max_workers // conv_parallelism
    conv_parallelism = max(1, min(args.max_workers, len(valid_indices)))
    per_daemon_workers = max(1, args.max_workers // conv_parallelism)
    logger.info("Pipeline budget: %d convs in parallel × %d workers/daemon "
                "during backfill; %d questions in flight during queries "
                "(max_workers=%d)",
                conv_parallelism, per_daemon_workers, args.max_workers,
                args.max_workers)
    conv_backfill_sem = asyncio.Semaphore(conv_parallelism)
    # Caps concurrent retrieval (memex daemon hits) only. LLM calls during
    # answerer/judge are bounded by each LLMClient's own AsyncLimiter(rpm),
    # so releasing the sem before LLM calls lets ~max_workers retrievals
    # run in parallel while LLM throughput maxes out at the rate limit.
    retrieval_sem = asyncio.Semaphore(args.max_workers)
    results_lock = asyncio.Lock()
    backfill_failures = []

    async def query_one_conv(conv_idx):
        if shutdown.requested:
            return
        entry = dataset[conv_idx]
        qa_pairs = [q for q in entry.get("qa", []) if q.get("category") in categories]
        if args.max_questions is not None:
            qa_pairs = qa_pairs[:args.max_questions]
        pending = [(qi, qa) for qi, qa in enumerate(qa_pairs)
                   if f"conv{conv_idx}_q{qi}" not in existing_ids]
        if not pending:
            return
        logger.info("[conv %d] querying %d QA pairs", conv_idx, len(pending))
        completed = 0

        conv_root = memex_root_for_conv(conv_idx)

        async def run_one(qi, qa):
            nonlocal completed
            if shutdown.requested:
                return
            result = await process_question(
                qa, qi, conv_idx, conv_root,
                judge_llm, answerer_llm, cutoffs, logger,
                predict_only=args.predict_only,
                evidence_lookup=evidence_lookup,
                backend=args.backend, model=args.model,
                raw_query=args.raw_query, top_k=args.top_k,
                retrieval_sem=retrieval_sem)

            qid = result["question_id"]
            qpath = os.path.join(output_dir, f"conv_{conv_idx}", f"{qid}.json")
            os.makedirs(os.path.dirname(qpath), exist_ok=True)
            save_result_json(qpath, result)

            async with results_lock:
                all_evaluations.append(result)
                existing_ids.add(qid)
                completed += 1
                if completed % 10 == 0:
                    label = cutoff_label(cutoffs[0])
                    scores = [e.get("cutoff_results", {}).get(label, {}).get("score", 0)
                              for e in all_evaluations]
                    logger.info("[conv %d] %d/%d llm=%.3f",
                                conv_idx, completed, len(pending),
                                sum(scores)/len(scores) if scores else 0.0)

        await asyncio.gather(*[run_one(qi, qa) for qi, qa in pending])

    async def process_conv_pipeline(conv_idx):
        # Backfill (gated by conv_backfill_sem so total agent subprocesses stay bounded)
        async with conv_backfill_sem:
            if shutdown.requested:
                return
            conv_root = memex_root_for_conv(conv_idx)
            logger.info("Backfilling conv %d into %s", conv_idx, conv_root)
            rc, stdout, stderr, wall_ms = await asyncio.to_thread(
                memex_backfill, conv_root, staged_roots[conv_idx],
                backend=args.backend, model=args.model,
                worker_count=per_daemon_workers)
            logger.info(
                "Backfill conv %d done in %dms (rc=%d): %s",
                conv_idx, wall_ms, rc,
                stdout.strip().splitlines()[-1] if stdout.strip() else "",
            )
            if rc != 0:
                logger.error("Backfill failed for conv %d (rc=%d): %s",
                             conv_idx, rc, stderr.strip()[:500])
                backfill_failures.append(conv_idx)
                return

            # Stop the daemon so its codex worker subprocesses (which saw
            # source content during EXTRACT/MERGE) exit. The next call into
            # `memex_query` auto-starts a fresh daemon with fresh worker
            # threads. Without this, EXPAND turns can inherit prior
            # EXTRACT/MERGE context from the worker's persistent thread —
            # measured at ~+6 pts of inflated score on conv 0 — making
            # benchmark numbers reflect cross-job context leakage rather
            # than retrieval quality.
            await asyncio.to_thread(memex_daemon_stop, conv_root)

        # Queries start as soon as this conv's backfill finishes — other convs
        # may still be backfilling, which overlaps their agents with these
        # queries' OpenAI calls.
        try:
            await query_one_conv(conv_idx)
        except Exception:
            logger.exception("[conv %d] query failed", conv_idx)

    await asyncio.gather(*[process_conv_pipeline(ci) for ci in valid_indices])

    if backfill_failures:
        if not args.keep_daemon:
            for ci in valid_indices:
                memex_daemon_stop(memex_root_for_conv(ci))
        return

    if not args.keep_daemon:
        for ci in valid_indices:
            memex_daemon_stop(memex_root_for_conv(ci))

    if all_evaluations:
        metrics = compute_locomo_metrics(all_evaluations, cutoffs)
        display_results(metrics, cutoffs)

        timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
        unified_path = os.path.join(args.output_dir, f"locomo_results_{timestamp}.json")
        save_result_json(unified_path, {
            "metadata": {
                "benchmark": "locomo",
                "project_name": args.project_name,
                "run_id": args.run_id,
                "timestamp": timestamp,
                "judge_model": args.judge_model,
                "judge_provider": args.judge_provider,
                "answerer_model": answerer_model if args.raw_query else args.model,
                "answerer_provider": answerer_provider if args.raw_query else args.backend,
                "total_questions": len(all_evaluations),
                "top_k": args.top_k,
                "top_k_cutoffs": [cutoff_label(c) for c in cutoffs],
                "categories": categories,
                "questions": None,
                "with_evidence": args.with_evidence,
                "judge_only": False,
                "previous_results_file": None,
                "merged_from_questions": None,
            },
            "metrics_by_cutoff": metrics,
            "evaluations": all_evaluations,
        })
        print(f"\nResults saved to: {unified_path}")

    print(f"\nTotal questions: {len(all_evaluations)}")


def main():
    asyncio.run(async_main())


if __name__ == "__main__":
    main()
