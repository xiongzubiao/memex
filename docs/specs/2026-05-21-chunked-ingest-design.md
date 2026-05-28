# Model-aware chunked EXTRACT for transcripts and documents

## Problem

The EXTRACT ingest path has two related sizing problems, one acute and one chronic.

**Acute (transcripts).** A real claude-code session transcript (30 MB, ~205,000 tokens of text after parsing) cannot be ingested at all. `handle_ingest_transcript_content` sends the entire transcript to the EXTRACT worker in one call; the prompt exceeds the worker model's context window; the worker returns a degraded response (prose status report or tool-use markup instead of the required `{"pages":[]}` JSON); the daemon's parser rejects it; the ingest fails. There is no transcript-side chunking today.

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
- Greedy packing: pack consecutive segments into a chunk until adding the next would exceed the per-chunk budget; emit the chunk, start a new one. Never split mid-segment.
- Per-chunk budget: chunk 0 uses the full `chunk_max_tokens`; chunks 1.. reserve an overlap budget (`overlap_turns × average segment tokens`) to leave room for the prepended overlap.
- A single segment exceeding `chunk_max_tokens` → `ChunkError::TooLargeSegment(idx, tokens)` (transcripts can't be split mid-turn). `idx` is the 0-based slice position; the handler maps it to the 1-based turn index in the user-facing diagnostic.
- Total would exceed `max_chunks` → `ChunkError::TooManyChunks(max_chunks)` (sanity bound on cost).
- After packing, prepend the last `overlap_turns` segments of `chunks[i−1]` to `chunks[i]` (what's available if fewer), then trim overlap from the front until the chunk fits `chunk_max_tokens` exactly — overlap is anchor context, not load-bearing.
- Token estimation: `estimate_tokens` = `max(chars/3, bytes/4)` (conservative for both Latin and multibyte/CJK text), plus a per-segment JSON-envelope overhead so many-short-turn transcripts can't overflow on envelope alone.
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
// As implemented: estimate the new prompt from its content + per-item JSON
// envelope, then scale up if the content looks structured (code/JSON), which
// bytes/4 undercounts.
let new_prompt_tokens = job_prompt_tokens(&job);
let scaled_prompt_tokens = scale_for_structured_content(new_prompt_tokens, &job);
let projected_input = last_turn_input_tokens
    .saturating_add(last_response_tokens)
    .saturating_add(scaled_prompt_tokens);
let safety = max_input / 5; // 20% margin (compensates for bytes/4 undercount)
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
- `ChunkError::TooLargeSegment(idx, tokens)` → `DaemonError::BadRequest("segment {n} is {tokens} tokens, exceeds chunk_max_tokens={cap} ...")`, where `{n}` is the 1-based turn index. Updates `ingest_jobs.status=failed`. Fires before any worker calls; no LLM credits spent.
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
- Token estimation = `max(chars/3, bytes/4)` + per-segment envelope; multibyte/CJK content estimates via the byte path; many short turns split on envelope overhead.

### Unit — `worker_chunk_max_tokens`

- Known model (e.g., `claude-sonnet-4-6`) → value derived from `lookup_model`, not the `DEFAULT_MODEL_INFO` fallback.
- Unknown model → falls back to `DEFAULT_MODEL_INFO` (128k input) → derived cap.
- Model-aware sizing scales: a non-derated 1M-context model returns a derived cap > 800k; a 200k-context model returns > 150k. `claude-sonnet-4-6` is capped to 200k by `apply_known_overrides`, so its derived cap reflects the 200k tier (see Implementation notes).

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

- Re-ingest the originally-failing 30 MB claude-code session (~205k tokens). On a sonnet-200k worker: expect ~1–2 chunks at model-derived `chunk_max_tokens` (~180k), with at most one `trigger="fit_miss"` reset between chunks. On a sonnet-1M worker: expect 1 chunk, no reset. Verify successful page extraction, no degraded outputs, ingest_jobs status flips to `completed`.
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

## Implementation notes

The design shipped as specified, with the deviations and additional fixes below. (The full `/autoplan` CEO + Eng review and the day-by-day post-implementation debugging log live in the PR history; they're omitted here to keep this a design record rather than a process log.)

### Deviations from the design as written

- **Overlap budget** uses a single average over the whole input (not a per-chunk running average), and the per-chunk cap is enforced exactly by *trimming* prepended overlap from the front until each chunk fits — not by a debug-assert (which would only fire in debug builds).
- **No `MIN_CHUNK_TOKENS` clamp.** `worker_chunk_max_tokens` forwards `compute_batch_budget(...)` directly; the clamp would guard a scenario that can't occur (overhead is 8k; no catalog model has a context window small enough to saturate the budget to 0). The startup log of resolved model + `chunk_max` did ship.
- **Per-item JSON envelope** (`SEGMENT_ENVELOPE_TOKENS`) is counted per segment when packing and in the worker's `job_prompt_tokens`, so a transcript of many short turns can't overflow on envelope alone.
- **Token estimates ceil**, not floor (`bytes.div_ceil(BYTES_PER_TOKEN)`), so the `fit_miss` projection errs high near the cap. `estimate_tokens` takes `max(chars/3, bytes/4)`; the `fit_miss` safety margin is 20% and structured (code/JSON-dense) content is scaled 1.4×.

### Three orthogonal worker/model fixes (not in the original design)

Surfaced during end-to-end verification on a real 30 MB claude-code session; each was diagnosed from raw stream-json capture:

1. **`--effort low`** on claude-code / codex / openai-api. Default reasoning effort spent the entire per-turn timeout in extended thinking on dense content and emitted zero output text (≈48 KB thinking / 0 B output in 1200s); with `low`, the same input completes in ~207s.
2. **`apply_known_overrides` derates `claude-sonnet-4-6` to 200k.** The vendored LiteLLM catalog lists it at the 1M tier, but that tier needs usage credits enabled; without them, requests over 200k fail with `[invalid_request] Prompt is too long`. The derate lives in code so re-vendoring can't lose it.
3. **Parse-then-cache gate.** `run_job_with_retry` cached raw worker text *before* the schema-parse step, so prose responses poisoned the cache and every retry returned `backend_unavailable` instantly. The cache insert now happens after the parse outcome, gated on parse success.

### End-to-end result

The originally-failing 30 MB session (~205k tokens) ingests cleanly: 4 chunks of ~108k, 3 inter-chunk `fit_miss` resets, cross-chunk MERGE-by-slug, `status=completed`, 6 pages stored. The full workspace test suite is green.

### Follow-ups (tracked, out of scope here)

- **Per-account context probe** to replace the unconditional 200k derate — users with usage credits enabled would get larger chunks.
- **Indexing-tail progress logging** — `embed_and_mark` + `maintain_backlinks_batch` can run 30+ min silently on a large transcript; a 50-min ingest looks like a hang from outside the daemon.
- **Per-ingest concurrency cap** — a large chunk count can monopolize the worker pool and hit Anthropic per-org throttling.
- **Slug drift across chunks** — the same subject under slightly different slugs escapes by-slug consolidation.
