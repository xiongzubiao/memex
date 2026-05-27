<!-- /autoplan restore point: /root/.gstack/projects/xiongzubiao-memex/strip-tags-injection-fix-autoplan-restore-20260522-050848.md -->
# Model-aware chunked EXTRACT for transcripts and documents

## Problem

The EXTRACT ingest path has two related sizing problems, one acute and one chronic.

**Acute (transcripts).** A claude-code session at `~/.claude/projects/-data-MemVerge-memex--claude-worktrees-locomo-benchmark-origin/e57b697d-6aad-4ad7-abe8-de52e9e926f2.jsonl` (30 MB, ~205,000 tokens of text after parsing) cannot be ingested at all. `handle_ingest_transcript_content` sends the entire transcript to the EXTRACT worker in one call; the prompt exceeds the worker model's context window; the worker returns a degraded response (prose status report or tool-use markup instead of the required `{"pages":[]}` JSON); the daemon's parser rejects it; the ingest fails. There is no transcript-side chunking today.

**Chronic (documents).** `handle_ingest_document` does chunk via `chunk_markdown`, but with two static, model-blind size knobs: `chunk_target_tokens = 30k` and `chunk_hard_cap_tokens = 50k`. A worker pointed at sonnet-200k (~180k usable context) or sonnet-1M tier (~900k usable) is artificially capped at 30k chunks. Documents pay extra EXTRACT calls, extra cross-chunk MERGE calls, and extra system-prompt re-tokenization for no quality reason — the model has plenty of room to take whole sections in one pass.

Both symptoms share one root cause: the chunker doesn't know what the worker can actually handle.

## Goal

Make EXTRACT chunking model-aware, and apply it consistently to both transcripts and documents:

- Transcripts gain a chunked path so arbitrarily-large sessions ingest.
- Documents get model-derived chunk sizing so workers with big context windows stop paying for unnecessary chunk splits.
- One shared chunked-extract pipeline (parallel EXTRACT + cross-chunk MERGE) feeds both paths.
- One model-derived size knob replaces the two static config fields.

Non-goals (deferred for now):
- Improving EXTRACT prompt quality (covered in separate INSTRUCTION BOUNDARY and quote-escape changes on this same branch).
- Per-chunk overlap marking, semantic chunk boundaries, or other extraction-quality optimizations. YAGNI until evidence demands them.
- Backfill of previously-ingested content at the new chunk sizes. Existing raw files remain valid; only new ingests use the new sizing.

## Architecture

```
                                  ┌────────────────────────────────────────────────┐
transcript ingest ──▶  parse ─▶   │  chunk_transcript_segments(turns)              │
                                  │   pack consecutive turns under chunk_max_tokens│
                                  │   (model-derived fresh-worker capacity)        │
                                  │   chunks 1..N prepend last 3 turns of i−1      │
                                  │   (overlap_turns = 3, anchor context for       │
                                  │   mid-topic boundaries; cross-chunk MERGE      │
                                  │   deduplicates Timeline entries)               │
                                  │   → Vec<Vec<ExtractSegment>>                   │
                                  └────────────────┬───────────────────────────────┘
                                                   │
                                                   │  Vec<Vec<ExtractSegment>>
                                                   ▼
                                  ┌────────────────────────────────────────────────┐
                                  │  extract_pages_from_chunked_jobs               │
                                  │   (chunks, source, state)                      │
                                  │   - build one IngestJob per chunk (with        │
                                  │     ChunkPosition and a oneshot reply channel) │
                                  │   - submit jobs in parallel to the worker queue│
                                  │   - collect ExtractedPages from all chunks     │
                                  │   - cross-chunk MERGE consolidation by slug    │
                                  │   → Vec<ExtractedPage>                         │
                                  └────────────────▲───────────────────────────────┘
                                                   │  Vec<Vec<ExtractSegment>>
                                                   │  (each inner Vec holds one
                                                   │  segment with role/timestamp/
                                                   │  index = None — preserves the
                                                   │  EXTRACT prompt's Mode B
                                                   │  document detection)
                                                   │
                                  ┌────────────────┴───────────────────────────────┐
                                  │  wrap each markdown chunk in a one-element     │
                                  │  Vec<ExtractSegment>{text: chunk, ...None}     │
                                  └────────────────▲───────────────────────────────┘
                                                   │  Vec<String>
                                                   │
document ingest ──▶  chunk_markdown(body, chunk_max_tokens, max_chunks) ────────────┘
```

Both paths produce the same shared shape (`Vec<Vec<ExtractSegment>>`) before reaching `extract_pages_from_chunked_jobs`. The IngestJob construction — including allocating the `tokio::sync::oneshot` reply channel — lives inside the helper, so neither caller deals with it.

### The document-side wrapping step in detail

`chunk_markdown` returns `Vec<String>` — one string per chunk, no internal structure (the markdown was already split at H1/H2 boundaries inside the function and the caller just gets the resulting text blobs). The shared helper expects `Vec<Vec<ExtractSegment>>` — one inner Vec per chunk, each inner Vec containing the segments that make up that chunk.

The wrapping turns each markdown chunk string into a one-element `Vec<ExtractSegment>`. Why one element, not many? Because a markdown chunk doesn't have any internal segment structure to honor — there are no turns, no speakers, no timestamps. It's a single text blob that the EXTRACT worker treats as one continuous document chunk. Splitting it further into multiple `ExtractSegment` entries would either fabricate fake structure or split mid-sentence; one segment per chunk is the honest representation.

The wrapping looks like this (inside `extract_pages_from_content`, the document caller):

```rust
let chunks: Vec<Vec<ExtractSegment>> = markdown_chunks
    .into_iter()
    .map(|chunk_text| vec![ExtractSegment {
        index: None,
        role: None,
        timestamp: None,
        text: chunk_text,
    }])
    .collect();
```

Each of the three `None` fields is load-bearing for a different reason:

- **`role: None` — Mode A vs Mode B detection.** The EXTRACT system prompt in `cli/src/daemon/worker/prompt.txt` says: *"Two extraction modes, identified by whether segments carry `role`."* Mode A (TRANSCRIPT) applies the one-page-per-speaker rule, requires a `## Timeline` H2 on subject pages with dated action-events, resolves relative dates from timestamps, etc. Mode B (DOCUMENT) uses H1/H2 headings as page boundaries, doesn't force Timeline sections, and doesn't apply the speaker-page rule. If a document chunk arrived with `role: Some("user")` set, the worker would incorrectly switch to Mode A and produce one-page-per-speaker output for a document that has no speakers. Documents MUST wrap with `role: None`.

- **`timestamp: None`** — documents don't have a per-segment timestamp. The EXTRACT prompt only consults segment timestamps to resolve relative dates inside transcript turns; with `None`, the worker simply doesn't try, which is correct for documents.

- **`index: None`** — the prompt says *"`index` (integer, optional) — ordering hint; array position is canonical when absent."* For a one-element Vec, array position alone is sufficient; an explicit index would be redundant.

What the worker sees in the EXTRACT prompt for a document chunk:

```json
{"segments":[{"text":"## Architecture\n\nThe system has three layers...\n\n## API\n\n`GET /v0/items`..."}],
 "source":"file:///path/to/doc.md",
 "chunk_index":0,
 "total_chunks":3}
```

No `role`, no `timestamp`, no `index` keys inside the segment — they're omitted entirely thanks to `#[serde(skip_serializing_if = "Option::is_none")]` on the `ExtractSegment` struct. Mode B detection fires cleanly.

What the transcript chunker emits, for comparison:

```json
{"segments":[
   {"index":1,"role":"user","timestamp":"2026-03-18T15:14:00Z","text":"Review the docs..."},
   {"index":2,"role":"assistant","timestamp":"2026-03-18T15:14:30Z","text":"Reading them now."}
 ],
 "source":"opencode://session/ses_300db7000...",
 "chunk_index":0,
 "total_chunks":2}
```

Same envelope shape, but each segment carries `role`, so the worker switches to Mode A.

A separate worker-side change adds a third restart trigger (`fit_miss`) so the worker auto-resets its subprocess before running a job whose prompt would not fit alongside accumulated conversation context. This is required because a worker may already have prior conversation context when a chunk arrives; the chunker assumes a fresh worker is available.

## Components

### `core::chunk::chunk_transcript_segments` (new)

```rust
pub fn chunk_transcript_segments(
    segments: &[ExtractSegment],
    chunk_max_tokens: usize,
    max_chunks: usize,
    overlap_turns: usize,
) -> Result<Vec<Vec<ExtractSegment>>, ChunkError>
```

Behavior:
- Greedy: pack consecutive segments into a chunk until adding the next segment would exceed `chunk_max_tokens`; emit the chunk, start a new one. Never split mid-segment.
- A single segment exceeding `chunk_max_tokens` → `ChunkError::TooLargeSegment(idx, tokens)`. The segment index points at the offending turn for diagnostics. (Transcripts can't be split mid-turn.)
- Total would exceed `max_chunks` → `ChunkError::TooManyChunks(max_chunks)` (sanity bound on cost).
- After packing, walk `chunks[1..]`: prepend the last `overlap_turns` segments of `chunks[i−1]` to `chunks[i]`. If `chunks[i−1]` has fewer than `overlap_turns` segments, prepend what's available (best-effort).
- Token estimation uses `BYTES_PER_TOKEN` from `core::model` (chars/4).
- Empty `segments` → empty `Vec` (no chunks).

`chunk_max_tokens` is supplied by the caller and is the maximum size a single chunk should be. The handler derives this from the worker model's context window (see `worker_chunk_max_tokens` below). The chunker stays model-agnostic — same function works for any cap value.

Lives next to `chunk_markdown` in `core/src/chunk.rs`. New `ChunkError` variants extend the existing enum.

### `extract_pages_from_chunked_jobs` (refactored from existing)

The body of the existing `extract_pages_from_content` (in `cli/src/daemon/handler/ingest.rs`) is split:

```rust
async fn extract_pages_from_chunked_jobs(
    chunks: Vec<Vec<ExtractSegment>>,
    source: &str,
    state: &HandlerState,
) -> Result<Vec<ExtractedPage>, DaemonError>
```

Body is the existing fan-out-EXTRACT-via-worker-queue + collect replies + cross-chunk fragment-merge-by-slug logic, untouched in behavior. The helper creates one `tokio::sync::oneshot` pair per chunk internally, constructs each `IngestJob` with `source: source.to_string()` and `chunk: Some(ChunkPosition{i, total})` when `total > 1` (else `None`), submits via `state.jobs.submit(...)`, collects replies, then runs the by-slug cross-chunk MERGE consolidation.

`extract_pages_from_content` becomes a thin wrapper that calls `chunk_markdown(content, worker_chunk_max_tokens(cfg), max_chunks)`, wraps each markdown chunk in a one-element `Vec<ExtractSegment>` (with role/timestamp/index = None — preserving document Mode B detection), and delegates to the new helper.

`chunk_markdown` itself collapses from two size parameters (`target_tokens`, `hard_cap_tokens`) into one (`chunk_max_tokens`): it packs sections under the cap and force-splits any single section that exceeds it. With model-derived sizing, the previous distinction (typical chunk size vs absolute max) no longer earns its complexity — the cap is the model's fresh-worker capacity, and there's no quality reason to artificially keep chunks smaller than that.

### `handle_ingest_transcript_content` (modified)

After parsing turns and computing `content_hash` (existing), and after dedup-raw-file-present check (existing):

```rust
let segments: Vec<ExtractSegment> = transcript.turns.into_iter()
    .enumerate()
    .map(|(i, t)| ExtractSegment {
        index: Some(i + 1),
        role: Some(t.role),
        timestamp: t.timestamp,
        text: t.text,
    })
    .collect();

let chunk_max = worker_chunk_max_tokens(&state.config);
let chunks = match chunk_transcript_segments(
    &segments,
    chunk_max,
    state.config.ingest.max_chunks,
    3, // overlap_turns
) {
    Ok(c) => c,
    Err(e) => return error_events(DaemonError::BadRequest(e.to_string())),
};

let extracted = extract_pages_from_chunked_jobs(chunks, &source_label, state).await?;
```

Rest of the function (`store_extracted_pages`, ingest_jobs status updates) unchanged.

### `worker_chunk_max_tokens` (new helper)

```rust
// in cli/src/daemon/worker/mod.rs
const EXTRACT_PROMPT_OVERHEAD_TOKENS: usize = 8_000;

pub(crate) fn worker_chunk_max_tokens(cfg: &crate::daemon::config::Config) -> usize {
    let w = &cfg.daemon.worker;
    let model = w.model
        .as_deref()
        .unwrap_or_else(|| w.backend.default_model());
    memex_core::model::compute_batch_budget(model, EXTRACT_PROMPT_OVERHEAD_TOKENS, 0)
}
```

Resolves the model name the same way the worker loop already does (`cfg.model` with a backend-default fallback). Relies on `core::model::lookup_model` (vendored litellm catalog with provider/prefix resolution) and `compute_batch_budget` which encodes `max_input − overhead − max_output − 10% safety`. Returns the maximum chunk size that fits on a fresh worker — this is BOTH the typical chunk size and the absolute force-split / TooLargeSegment boundary.

Both `IngestConfig.chunk_target_tokens` and `IngestConfig.chunk_hard_cap_tokens` config fields are removed; the catalog-derived value is the sole source. Per the project's no-back-compat-scaffolding policy, removed-field warnings are not emitted; serde will silently ignore both fields in existing configs.

### Worker `fit_miss` restart trigger (new)

`run_worker_loop` in `cli/src/daemon/worker/mod.rs` gains one new piece of state and one new pre-job check.

State added: `last_response_tokens: u64` (set after each completed job from the response payload size; consistent with the existing `last_turn_input_tokens: u64`).

Pre-job check, added to the existing `count_hit || context_hit` clause:

```rust
let new_prompt = build_prompt_for(&job);  // already computed for dispatch
let new_prompt_tokens: u64 = (new_prompt.len() / memex_core::model::BYTES_PER_TOKEN) as u64;
let projected_input = last_turn_input_tokens
    .saturating_add(last_response_tokens)
    .saturating_add(new_prompt_tokens);
let safety = max_input / 10; // 10% margin
let fit_miss = projected_input > max_input.saturating_sub(safety);
if count_hit || context_hit || fit_miss {
    let trigger = if fit_miss { "fit_miss" }
        else if context_hit { "context" }
        else { "count" };
    tracing::info!(projected_input, max_input, trigger, "restart triggered");
    if !sp.soft_reset(&cfg).await {
        subprocess = None;
    }
    jobs_done = 0;
}
```

(`max_input` is already in scope at this point in `run_worker_loop`; computed once at loop entry from `lookup_model(model_name).max_input_tokens`.)

After a successful job:

```rust
last_turn_input_tokens = input_tokens;
last_response_tokens = response_tokens;  // NEW: extracted from outcome
```

Effect:
- Small chunk on a near-empty worker: no reset, cache preserved.
- Big chunk on a busy worker: reset triggered with `trigger="fit_miss"`, then runs against a fresh subprocess.
- Single segment too big even on a fresh worker: chunker catches it before submit (`TooLargeSegment`).

## Data flow

For a transcript ingest:

```
Client
  │ IngestSource::TranscriptInline { content, agent, source_label }
  ▼
handle_ingest_transcript_content
  │ 1. parse content → CleanedTranscript
  │ 2. filter (NonSubstantive / InternalSession → silent skip)
  │ 3. render canonical_transcript
  │ 4. content_hash + job_id
  │ 5. raw-file-present dedup → early Done{0}
  │ 6. insert_ingest_job (ON CONFLICT resets failed→pending)
  │ 7. turns → Vec<ExtractSegment>
  │ 8. chunk_max = worker_chunk_max_tokens(cfg)                       ← NEW
  │ 9. chunks = chunk_transcript_segments(segments, chunk_max, ...)   ← NEW
  │       returns Vec<Vec<ExtractSegment>>
  │10. extract_pages_from_chunked_jobs(chunks, source_label, state)   ← shared with docs
  │       ├─ build one IngestJob per chunk (with ChunkPosition)
  │       ├─ submit all jobs to worker queue
  │       ├─ worker may fit_miss-reset per chunk if needed
  │       ├─ collect Vec<ExtractedPage>
  │       └─ cross-chunk MERGE by slug
  │11. empty pages → completed, Done{0}
  │12. build RawFrontmatter + title
  │13. store_extracted_pages (raw + pages + indexes)
  │14. ingest_job=completed → Done{0}
```

Invariants preserved:
- `content_hash` is computed BEFORE chunking; dedup still works against pre-chunking-era raw files.
- Cross-chunk merge by slug is the topic-continuity mechanism. Overlap turns at boundaries give per-chunk context; MERGE deduplicates any Timeline entries that appear in both chunks.
- Single-chunk fast path is automatic: small transcripts produce one chunk, the fan-out trivially completes, cross-chunk merge is a no-op.

## Error handling

Inherited (no change):
- A chunk's EXTRACT fails: first failure short-circuits, in-flight chunks complete and discard, ingest fails atomically, `ingest_jobs.status=failed`.
- Cross-chunk MERGE fails: atomic fail.
- Empty extracted pages across all chunks: `ingest_jobs.status=completed`, `Done{0}`, no raw file written.
- Daemon crash mid-flight: `recover_stuck_ingest_jobs` on next startup flips row to `failed: interrupted by daemon restart`.

New errors:
- `ChunkError::TooLargeSegment(idx, tokens)` → `DaemonError::BadRequest("transcript segment {idx} is {tokens} tokens, exceeds chunk_max {cap}")`. Updates `ingest_jobs.status=failed`. Fires before any worker calls; no LLM credits spent.
- `ChunkError::TooManyChunks(max_chunks)` → `DaemonError::BadRequest("transcript would split into more than {max} chunks; raise daemon.ingest.max_chunks")`.

Logging additions:
- INFO `chunked transcript: {n_chunks} chunks, {total_segments} segments` at dispatch.
- INFO `restart triggered` with `trigger="fit_miss"` (new field) when the new pre-job check fires.

Per-chunk daemon worker timeout (300s default) applies independently to each chunk. A 7-chunk transcript could take up to ~35 min worst case. Matches existing document behavior.

## Testing

### Unit — `chunk_transcript_segments`

- One small segment → one chunk, no overlap.
- Many small segments → packed under `chunk_max_tokens`, never split mid-segment.
- Chunks 1..N start with last 3 segments of chunk i−1 (overlap).
- Single segment > `chunk_max_tokens` → `TooLargeSegment(idx, tokens)`.
- Total > `max_chunks` → `TooManyChunks(max)`.
- Empty input → empty `Vec`.
- Single segment exactly at `chunk_max_tokens` → own chunk (boundary).
- Token estimation uses `BYTES_PER_TOKEN` from `core::model`.

### Unit — `worker_chunk_max_tokens`

- Known model (e.g., `claude-sonnet-4-6`) → value derived from `lookup_model`, not the `DEFAULT_MODEL_INFO` fallback.
- Unknown model → falls back to `DEFAULT_MODEL_INFO` (128k input) → derived cap.
- Model in 1M-context tier returns a derived cap > 800k; baseline 200k-context model returns > 150k. (Verifies model-aware sizing actually scales.)

### Unit — worker `fit_miss` restart trigger

- `last_turn_input_tokens=0`, small new prompt → no reset.
- `last_turn_input_tokens=high` (under `context_threshold`), huge new prompt overflowing projected input → reset triggered with `trigger="fit_miss"`.
- `last_turn_input_tokens=0`, new prompt > `max_input − safety` → reset triggered (defense-in-depth even though chunker should have caught it).
- Existing `count_hit` and `context_hit` triggers still fire (regression).
- `last_response_tokens` is updated after a job completes and used on the next iteration.

### Integration — transcript handler

- Opencode SQLite fixture with one session whose total turns sum to 5× the derived `chunk_max_tokens` for the test model → produces ~5 chunk EXTRACT calls, fragment-merges by slug, stores pages.
- Small opencode fixture (total turns well under `chunk_max_tokens`) → produces 1 chunk, single-chunk fast path, no cross-chunk merge.
- Fixture with one session containing a single huge segment > `chunk_max_tokens` → `DaemonError::BadRequest("transcript segment ...")`, `ingest_jobs.status=failed`.

### Integration — document handler

- Document with total body > derived `chunk_max_tokens` → produces N>1 chunks via the refactored `extract_pages_from_content`, fragment-merges by slug, stores pages.
- Small document (body well under `chunk_max_tokens`) → produces 1 chunk, single-chunk fast path, no cross-chunk merge.
- Document with a single markdown section > `chunk_max_tokens` → `chunk_markdown` force-splits it at paragraph/line boundaries (existing fallback path), ingest completes.

### Regression

- All existing `core::transcript::tests::*` pass.
- All existing `cli::daemon::worker::tests::*` pass.
- Document-ingest tests pass after the `extract_pages_from_content` refactor (behavior preserved).

### Manual end-to-end

- Re-ingest the 30 MB claude-code session at `~/.claude/projects/-data-MemVerge-memex--claude-worktrees-locomo-benchmark-origin/e57b697d-6aad-4ad7-abe8-de52e9e926f2.jsonl` (the originally-failing session — ~205k tokens). On sonnet-200k worker: expect ~1–2 chunks at model-derived `chunk_max_tokens` (~180k), with at most one `trigger="fit_miss"` reset between chunks. On sonnet-1M worker: expect 1 chunk, no reset. Verify successful page extraction, no degraded outputs, ingest_jobs status flips to `completed`.
- Re-ingest a small opencode transcript (single chunk on any model) — confirm chunk count = 1, no `fit_miss` triggers, fast path preserved.
- Re-ingest a document with body > 50k but ≤ derived `chunk_max_tokens` (e.g., 100k markdown). Today this produces ~3 chunks (50k hard cap force-splits); after the change, expect 1 chunk. Verify the wiki pages produced cover the same subjects as the old 3-chunk extraction (spot-check 2–3 page slugs).
- Re-ingest a document that previously fit comfortably under 30k (e.g., a 20k README). Confirm output is unchanged — single-chunk path was already optimal and remains so.

## Migration notes

- Both `IngestConfig.chunk_target_tokens` and `IngestConfig.chunk_hard_cap_tokens` are removed. Per the no-back-compat-scaffolding policy, existing configs containing these fields will deserialize with the fields silently ignored (serde default behavior). No migration code; no warning emission.
- **Document-ingest behavior change** — significant. Today, document chunks pack to ~30k target with a 50k hard cap, producing many small chunks. After the change, chunks pack to the model-derived `chunk_max_tokens` (≥128k for any model in the catalog, up to ~900k on 1M-context models). Visible effects:
  - Document with 200k of body content on a sonnet-200k worker: today ~7 chunks of ~30k each; after change, 1 chunk of ~180k.
  - Net effect: fewer EXTRACT calls per document, fewer cross-chunk MERGE consolidations, lower total LLM token cost.
  - Extraction quality tradeoff: full context per call may give better holistic understanding; smaller chunks may give better per-section attention. Neither has been empirically validated for this codebase. Manual end-to-end checks listed in the testing section verify the small + large cases both work; broader quality regressions, if any, will surface as user-visible page-content changes and can be tuned post-ship by re-introducing a config knob.
- No `content_hash` format change; existing raw files remain valid for dedup.

---

## /autoplan Review — CEO Phase

Run: 2026-05-22 (sonnet-4-6 subagent + codex-cli 0.130.0). User challenge surfaced and **rejected** — original direction held; both config knobs remain removed as designed.

### Concerns to acknowledge (not blocking, but recorded)

| # | Severity | Source | Concern | Disposition |
|---|---|---|---|---|
| C1 | HIGH | both voices | Bigger chunks may degrade extraction quality (lost-in-the-middle); not empirically validated | Held. Manual end-to-end tests verify the small + large cases; if regressions surface post-ship, re-introduce knob then. |
| C2 | HIGH | both voices | `chars/4` token estimation under-counts for code/JSON-heavy transcripts by 30–50%; `fit_miss` may miscompute | Add Failure Modes Registry entry. Consider widening the safety margin from 10% to ~20% if real-world undercounting shows up. |
| C3 | HIGH | both voices | Cross-chunk MERGE-by-slug doesn't actually handle topic continuity when the same topic gets slightly-different slugs across chunks | Acknowledged as accepted defect; revisit if eval evidence shows cross-chunk subject splitting in practice. |
| C4 | MED | Claude subagent | 30 MB locomo-benchmark session is the only forcing example, and it's from an eval workflow not a real user transcript | Held. Future re-evaluation: if no other transcript-too-large cases surface within ~3 months, transcript chunking can be downgraded to a `--truncate-tokens N` flag. |
| C5 | MED | Claude subagent | `overlap_turns = 3` is arbitrary; no measurement of how many cross-boundary topics it actually recovers | Held. Cheap to revisit later as a tuning param. |
| C6 | MED | Codex | Migration "silently ignore old config fields" is operationally fragile for users with existing configs | Held per the project's no-back-compat-scaffolding policy. Documented in Migration notes. |
| C7 | LOW | Claude subagent | Wiki contains pages extracted at old (30k) and new (>=180k) chunk sizes; user search sees inconsistent page granularity across eras | Acknowledged silent technical debt; not blocking. |

### Out of scope per CEO challenge (raised, deferred)

- Replacing chunking strategy with a semantic / topic-boundary chunker (competitive parity with Mem0/MemMachine/Letta) — separate work, larger scope.
- Adaptive overlap (boundary confidence, speaker switches) — premature optimization until C5 is measured.
- A pre-summarize-with-cheap-model pass before EXTRACT — interesting alternative, deferred.
- Schema-first decoding + parser-repair stage in the worker (Codex #3) — separate work; the INSTRUCTION BOUNDARY and quote-escape prompt fixes on this branch already address a subset.
- Outcome-based release gates (% correct facts, time-to-first-useful-page, cost per useful fact) — adopt for future eval cycles; out of scope for this PR.

### Premise gate status

- Premises **CONFIRMED** by user at D2.
- User Challenge on config knob removal: **REJECTED by user** (Hold scope, no knobs).
- Phase 1 closed.

---

## /autoplan Review — Eng Phase

Run: 2026-05-22 (sonnet-4-6 subagent + codex-cli 0.130.0). Both voices read the actual repo source (queue.rs, config.rs, model.rs, worker/mod.rs, handler/ingest.rs) and verified claims at file/line level. Strong convergence on six critical or high findings. Four require spec changes (folded in below); two require call-out in the spec text but not new sections.

### Spec changes folded in (must implement)

1. **Reserve overlap budget when packing (E-CRIT-1).** `chunk_transcript_segments` packs to `chunk_max_tokens − reserved_overlap_tokens`, not to `chunk_max_tokens`. `reserved_overlap_tokens` is estimated from the running average turn size × `overlap_turns`, recomputed per-chunk. After overlap is prepended, a debug-assert verifies total chunk tokens ≤ `chunk_max_tokens`. Without this reservation, a chunk packed to exactly `chunk_max_tokens` and then prefixed with 3 overlap turns can exceed the model's context — recreating the failure the design exists to prevent.

2. **`MIN_CHUNK_TOKENS` floor in `worker_chunk_max_tokens` (E-CRIT-2).** `compute_batch_budget` saturates to 0 when overhead exceeds `max_input` (e.g., unknown model falls back to `DEFAULT_MODEL_INFO` 128k input − 16k max_output − 8k overhead − 12.8k safety = ~91k, OK; but a hypothetical model with bizarre catalog values could saturate to 0). Add `const MIN_CHUNK_TOKENS: usize = 8_000;` and return `cmp::max(MIN_CHUNK_TOKENS, compute_batch_budget(...))`. At daemon startup, log the resolved model name and the computed `chunk_max` so operators can see what's in effect.

3. **Mode B serde contract test (E-CRIT-3).** Add in `cli/src/daemon/queue.rs` tests:
   ```rust
   #[test]
   fn extract_segment_doc_mode_omits_optional_fields() {
       let seg = ExtractSegment { index: None, role: None, timestamp: None, text: "x".into() };
       let json = serde_json::to_string(&seg).unwrap();
       assert_eq!(json, r#"{"text":"x"}"#);
   }
   ```
   This is the load-bearing test for the entire shared-helper design: it pins the contract that the EXTRACT prompt's Mode A vs Mode B detection depends on.

4. **Delete the orphaned `IngestConfig::validate` clauses (E-CRIT-4).** When removing `chunk_target_tokens` and `chunk_hard_cap_tokens` from `IngestConfig`, also remove the validator clauses at `cli/src/daemon/config.rs:238-248`. The migration as written says "serde silently ignores the field" but if the validator clauses aren't deleted alongside, the code won't compile (the validators reference the removed fields).

5. **`fit_miss` state reset (E-HIGH-6).** When `soft_reset` returns false (claude-code respawn path) OR when `subprocess = None` is set, also zero `last_turn_input_tokens = 0; last_response_tokens = 0;` (not just `jobs_done = 0`). Without this, the next iteration's `fit_miss` check uses stale tracking from the now-dead subprocess, false-positives a reset that just happened.

### Spec text changes (no implementation impact)

6. **Token estimation: "chars/4" → "bytes/4" (E-HIGH-5).** Spec text in the "Token estimation" notes claims `chars/4`. Verified in `core/src/model.rs:19`: `BYTES_PER_TOKEN = 4`, and `prompt.len() / BYTES_PER_TOKEN` uses `String::len()` which is bytes. Fix the spec wording. Add a content-class margin: when the chunk text is > 30% non-alphabetic by byte count (code, JSON, base64), scale the estimate by 1.4× before comparison. Widen the `fit_miss` safety margin from 10% to 20%.

### Eng phase findings acknowledged but deferred

| ID | Issue | Disposition |
|---|---|---|
| E-MED-7 | Sequential per-slug MERGE truncates to 20k chars on the LLM-fail concat-fallback path (Codex #6) — silent data loss when 4+ fragments share a slug | Acknowledged. Out of scope for this spec; track as a separate follow-up if real data shows >4 fragments per slug is common. |
| E-MED-8 | Per-ingest concurrency is unbounded; a 20-chunk ingest can monopolize the worker queue and starve other ingests/queries (Codex #7) | Acknowledged. The default `max_chunks=20` and typical `worker.max_count` (CPU count) make this a real concern at peak load. Out of scope here; consider a per-ingest semaphore in a follow-up. |
| E-MED-9 | Cross-chunk merge waits for ALL chunk replies before merging — slowest chunk pins total latency at the 300s worker timeout (subagent F8) | Acknowledged. Stream-merge by slug as chunks complete is a clean optimization; defer until peak-load profile shows it matters. |
| E-MED-10 | Slug drift across chunks (`acme-auth` vs `acme-auth-service`) escapes by-slug consolidation (both voices) | Already captured as C3 in CEO phase. No new disposition. |
| E-LOW-11 | Manual e2e fixture references `~/.claude/projects/.../e57b697d-...jsonl` — a developer-local path, not reproducible in CI (subagent F20) | Acknowledged. The eng phase recommendation is to check in a sanitized fixture under `tests/fixtures/`. Out of scope for this design doc; track as test-infrastructure work. |
| E-LOW-12 | Memory cost: ~60 MB peak during a 30 MB transcript chunking pass (original + cloned segments) (subagent F19) | Noted. Not worth speculative optimization; revisit if profiling shows it. |

### Phase 3 status

- Eng dual voices: ran, both completed in foreground, consensus table produced.
- 4 critical findings folded into the spec as required changes (E-CRIT-1 through E-CRIT-4).
- 2 high findings folded as spec-text updates (E-HIGH-5, E-HIGH-6).
- 6 medium/low findings acknowledged in the table above; out of scope.
- Phase 3 closed.

---

## Post-implementation manual e2e — initial failure, root-cause investigation, resolution (2026-05-22 to 2026-05-23)

The 30 MB `e57b697d-...jsonl` claude-code session was re-ingested after the implementation landed (commit `fe9b7a2`). First attempt failed with a prose response that bypassed the JSON parser. Investigation across the following day diagnosed **three layered issues**, each masking the others. After all three are addressed the session ingests end-to-end.

### What the investigation actually found

Initial framing in this section claimed the failure was prompt-injection from dense imperative content and was "outside the chunked-ingest design's scope." That framing was wrong. The real diagnosis took several rounds of experimentation:

**Issue 1 — Extended thinking exhausts the per-turn timeout.** Default reasoning effort on `claude-sonnet-4-6` (via `claude -p`) enables extended thinking. On dense pipeline-meta content the model spends the entire per-turn timeout in internal deliberation and emits no output text:

| Effort   | Elapsed | Thinking block | Output text |
|----------|---------|----------------|-------------|
| default  | 1200s   | 48,616 bytes   | 0 bytes     |
| low      | 207s    | 69 bytes       | 29,942 bytes |

Same input, same model, same system prompt. The worker discards `thinking` content blocks (`block.ty == "text"` is the only kept type), so the cached cause of failure was invisible until we captured raw stream-json. Fixed in commit `424f75f` by adding `--effort low` (claude-code), `-c model_reasoning_effort=low` (codex), and `request.reasoning_effort(Low)` (openai-api). Gemini CLI has no equivalent knob; left at default.

**Issue 2 — The catalog's 1M context for `claude-sonnet-4-6` isn't accessible by default.** The vendored LiteLLM catalog lists `claude-sonnet-4-6` at `max_input_tokens=1_000_000` (the 1M-context tier). That tier requires "usage credits" enabled at `claude.ai/settings/usage`. Without them, requests above 200k tokens trigger an auto-compaction attempt that fails with `API Error: Usage credits required for 1M context...`, surfaced to the caller as `[invalid_request] Prompt is too long`. `claude -p --verbose` confirms via `modelUsage.{model}.contextWindow=200000` for the default OAuth path.

Adding `--effort low` exposes this because the compaction path triggers at lower input sizes than default-effort runs (which seem to compact silently, often producing prose summaries as the eventual EXTRACT output — which is itself one of the failure modes we were observing).

Fixed in commit `e400777` via a new `apply_known_overrides()` layer inside `core::model::lookup_model`. The override caps `claude-sonnet-4-6` to `max_input_tokens=200_000`, matching what claude-code actually delivers. The vendored LiteLLM catalog stays as a faithful copy of upstream; the memex-specific derate lives in code where re-vendoring can't lose it. With the cap in place, `compute_batch_budget` returns `108_000` (= 200k − 10% safety − 64k max_output − 8k overhead) and the 480k-token transcript chunks into 4 pieces that fit cleanly under the 200k API limit.

**Issue 3 — The LLM cache stores prose responses, poisoning future retries.** `worker::run_job_with_retry` was inserting the raw worker text into `llm_cache` on `TurnOutcome::Ok` *before* the schema-parse step. When EXTRACT or MERGE emitted prose, the prose got cached. Every subsequent retry of the same prompt hit the cache and returned `backend_unavailable` in milliseconds. On this machine: 312 of 630 cache entries (~50%) were unparseable prose, making the original failure look reproducible even after the prompt fixes landed. Fixed in commit `886d29a` by moving the cache insert to after the parse outcome is built, gated on `parse_ok`.

### End-to-end verification (2026-05-23, after all three fixes)

```
chunk_max_tokens=108000   ← derived from 200k-derate via apply_known_overrides
n_chunks=4
total_segments=4449

EXTRACT phase  : 20:58:12 → 21:07:39 (~9 min, 4 chunks × ~135-165s each on --effort low)
fit_miss reset : 3 inter-chunk worker resets (last_turn_input_tokens=155100, projected_input=162659)
dedup search   : 4 existing wiki pages to merge with + 2 new pages
MERGE phase    : 21:08:42 → 21:15:31 (~7 min, 4 sequential merges: 62s, 225s, 297s, 471s)
indexing tail  : 21:15:31 → 21:49:01 (~34 min, see below)
ingest_jobs    : status=completed, stored=6 pages
total elapsed  : 51 min
```

### Indexing tail (34 minutes, unobservable from logs)

After the last MERGE completes, the handler runs through `store_extracted_pages` lines 526–593: write the raw file under content-hash-named path, commit the source row, embed the raw source, run `maintain_backlinks_batch` across the existing wiki. **No progress logs fire during any of this.** The only events in the 34-minute window are one WARN at the start (`raw file body hash does not match path expected=fc... found=None`, expected since we cleared the prior raw file to force re-ingest) and the final `ingest job completed`.

The work hidden inside the tail:
- **Embed the raw source.** 1.4 MB / ~480k tokens of canonical transcript text, semantic-split into ~250 chunks of ≤ 2044 tokens each, embedded sequentially under the single shared `embed_model` lock. With a remote embedder this is many individual API calls.
- **`maintain_backlinks_batch`** across 173 existing wiki pages × 2 new pages to add backlinks where applicable. Each backlink-update touches a wiki file under per-page locks.
- Both phases run under one acquisition of `embed_model.lock().await` (lines 568-593), so they serialize relative to concurrent reconcile and watcher embed-work but don't show progress to the operator.

**Not a bug in the chunked-ingest design**; it's a pre-existing operational visibility gap. Follow-up worth tracking: periodic progress logging (`embedding raw source: chunk N of M`, `maintaining backlinks for new slug X`) during these phases. A 50+ minute ingest with 34 minutes of silent indexing looks like a hang from outside the daemon.

### What this verifies about the spec

The chunked-ingest design **does work end-to-end** on the originally-failing 30 MB session, with the right config:

- **Chunker correctness**: 4 chunks of ~108k each fit the 200k API context; overlap budget reservation prevents post-overlap overflow (E-CRIT-1); `fit_miss` worker resets fire correctly between chunks (E-HIGH-5/6 worked as designed); cross-chunk MERGE-by-slug consolidated multi-chunk subjects.
- **6 review fold-ins** (E-CRIT-1 through E-CRIT-4, E-HIGH-5, E-HIGH-6) all behaved as specified in production conditions.
- **Three orthogonal fixes layered onto the chunked-ingest design** to handle real-world claude-code OAuth realities: reasoning-effort capped, catalog override for sonnet-4-6's accessible context, parse-then-cache ordering. None of these were in the spec; all are tracked in their own commits (`424f75f`, `e400777`, `886d29a`).
- **Full workspace test suite passes**: `cargo test --workspace` → 530 tests green.

### Items still flagged for follow-up

| Concern | Where it lives |
|---|---|
| **Per-account context-window probe** — `apply_known_overrides` caps `claude-sonnet-4-6` to 200k unconditionally. Users with usage credits enabled get suboptimal chunk sizes (smaller than necessary, not failures). A startup probe of `claude --verbose` to read `modelUsage.contextWindow` would derive the per-account real cap. Deferred. | `core::model::apply_known_overrides` docstring |
| **Indexing-tail progress logging** — `embed_and_mark` for the raw source + `maintain_backlinks_batch` together can take 30+ minutes silently on a 1.4 MB transcript + 170-page wiki. Add periodic INFO logs so 50-min ingests are observable. | Out of scope for this PR; future ops-visibility work. |
| **Anthropic's parallel-call throttling** — claude-code workers run in parallel via the worker pool. Practical throughput is limited by per-org concurrency that the test machine hit during early /8 experiments. Not currently addressed; usually invisible because individual chunk count stays low. | Already captured in /autoplan Eng Phase as E-MED-8. |

Phase 6 (manual e2e) closes with successful end-to-end ingest. The "outside the chunked-ingest design's scope" framing in the first revision of this section was a misdiagnosis; the actual issues were three layered config/library bugs and have been fixed.
