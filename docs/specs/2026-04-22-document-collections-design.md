# Memex Document Collections Design

Date: 2026-04-22
Status: Proposed (brainstormed, pending implementation plan)

## 1. Scope and Decisions

This design adds document collections for organization across both source and wiki documents.

Locked decisions:

- One document can belong to multiple collections.
- `memex ingest` supports repeatable `--collection`.
- `memex backfill` supports repeatable `--collection`.
- `memex query` supports repeatable `--collection`.
- Multiple `--collection` values in query use OR semantics.
- Collections are for organization, not isolation boundaries.
- No-flag behavior is consistent across commands (Model 2):
  - ingest/backfill with no `--collection` assigns `default`.
  - query with no `--collection` searches `default` only.
- No in-place migration is required. Collection support applies to newly initialized/reset databases.
- Wiki frontmatter gains `collections` so wiki docs can rebuild collection mappings.

Out of scope:

- Collection-level ACL or isolation.
- Collection metadata such as `include_by_default`.
- Backward-compatible DB migration for existing installations.

## 2. Data Model

Add normalized many-to-many storage.

### 2.1 New Tables

`collections`

- `id INTEGER PRIMARY KEY AUTOINCREMENT`
- `name TEXT NOT NULL UNIQUE`

`document_collections`

- `document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE`
- `collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE`
- `PRIMARY KEY (document_id, collection_id)`

### 2.2 Name Normalization

Collection names are normalized at CLI ingress:

- Trim whitespace.
- Lowercase.
- Reject empty values after normalization.
- Deduplicate within a single command invocation.

### 2.3 Identity and Docids

Collections are metadata, not document identity:

- Same `(doc_type, path)` keeps one row in `documents`.
- Existing `docid` remains unchanged on collection updates.
- No new `documents` row is created for collection-only changes.

## 3. CLI and Protocol Changes

### 3.1 CLI

- `memex ingest --agent <agent> [--collection <name> ...]`
- `memex backfill <agent> [--path <dir>] [--collection <name> ...]`
- `memex query <question> [--raw] [--top-k <n>] [--collection <name> ...]`

If `--collection` is omitted, effective collection set is `default`.

### 3.2 Daemon Protocol

Extend daemon request payloads:

- `Request::Ingest` gains `collections: Vec<String>`.
- `Request::Query` gains `collections: Vec<String>`.

`backfill` remains a client-side fan-out to `ingest`; it forwards the same collection set to each dispatched ingest call.

## 4. Write Path Semantics

## 4.1 Ingest / Backfill

For each ingest job:

1. Resolve effective collections:
   - user-provided normalized list; or `default` if none.
2. Persist/lookup collection ids.
3. Apply memberships to:
   - the source transcript document.
   - every wiki page created or updated by this ingest.

When ingest merges into an existing wiki page, collection memberships are unioned:

- existing page collections U incoming collections.

This is intentional because collections are organizational, not isolation boundaries.

## 4.2 Wiki Frontmatter

Wiki pages written/updated by ingest include:

```yaml
collections: [default, project-a]
```

Rules:

- Frontmatter value is normalized names.
- Persist deterministic order (sorted) to keep diffs stable.
- If no collections are present in parsed frontmatter during rebuild, assign `default`.

## 5. Query Semantics

Effective query collections:

- If user passes `--collection`, use that normalized set.
- If not, use `[`default`]`.

Filtering:

- Apply OR semantics across selected collections.
- A document matches if it has membership in any selected collection.

Retrieval behavior:

- Collection filter applies before fusion/synthesis so non-matching documents do not enter ranking.
- This applies to both raw context and synthesized query paths.

Unknown collections:

- Not an error.
- They simply produce zero matches if no documents belong to them.

## 6. Rebuild Behavior

Goal: allow rebuilding collection mappings for wiki documents from markdown files.

On wiki reindex/lint-fix style rebuild:

- Parse frontmatter `collections`.
- Normalize and write memberships for that wiki document.
- If absent/empty, assign `default`.

Note:

- Source documents are SQLite-only and cannot be fully reconstructed from wiki files alone.
- Rebuild guarantee in this design is specifically for wiki-document collection mappings.

## 7. Error Handling and Guardrails

Validation errors:

- Empty/invalid `--collection` value returns a clear CLI error.

Safety invariants:

- Collection-only updates must not change `docid`.
- No duplicate membership rows due to PK on `(document_id, collection_id)`.
- `ON DELETE CASCADE` prevents orphaned membership rows.

Content/hash orphan cleanup:

- Existing content cleanup behavior remains required when page/source hash changes.
- In ingest batch overwrite paths, ensure old content/chunks are orphan-cleaned when superseded.

## 8. Testing Plan

### 8.1 Core Data Tests

- Create/lookup collection rows idempotently.
- Membership insert is idempotent and unique.
- Deleting a document cascades membership cleanup.

### 8.2 Ingest/Backfill Tests

- Ingest with explicit collections writes memberships to source + wiki docs.
- Ingest with no flag assigns `default`.
- Backfill forwards collection set to each ingest request.

### 8.3 Query Tests

- No-flag query returns only docs in `default`.
- Query with repeated collections uses OR semantics.
- Unknown collection yields no results without errors.

### 8.4 Frontmatter/Rebuild Tests

- Ingest writes `collections` field in wiki frontmatter.
- Rebuild rehydrates membership from frontmatter.
- Missing frontmatter collections falls back to `default`.

### 8.5 Identity/Orphan Tests

- Updating collections does not change `docid` for same path.
- No extra `documents` row created for collection-only changes.
- Overwrite paths cleanup superseded hashes/chunks.

## 9. Alternatives Considered

### 9.1 Collections as tags (`collection:*`)

Rejected because:

- mixes two different semantics (content tags vs organization scope).
- easy accidental loss via tag rewrites.
- fragile exact-match filtering and normalization.
- unwanted effects on FTS/ranking through tag weighting.

### 9.2 Comma-separated list on `documents`

Rejected because:

- brittle matching and update semantics.
- weak invariants and duplicate handling.
- hard to evolve compared to normalized joins.

## 10. Implementation Readiness

This spec is ready for implementation planning with the following constraints carried forward:

- No DB migration support for existing installs.
- No collection isolation guarantees.
- Default collection is required for no-flag ingest/backfill/query behavior.
