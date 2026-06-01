# Concurrency, Locking & Crash Safety

**Status:** Living design doc — describes the system as implemented.
**Last updated:** 2026-05-29

Part of the consolidated memex design set ([`README.md`](README.md)). Companion to [`architecture.md`](architecture.md) and [`daemon.md`](daemon.md).

## Model

The daemon is the single writer. CLI mutation commands (`write`, `delete`, `lint --fix`, `ingest`) are daemon clients; the daemon serializes all index and filesystem mutations. Readers (`search`, `read`) take no lock and rely on SQLite WAL snapshot isolation.

## Locks

| Lock | Where | Protects |
|---|---|---|
| Writer flock `~/.memex/.lock` | `core/src/storage.rs::try_acquire_lock` (fs2/flock), RAII `WriterLock` (`core/src/lib.rs`) | Exclusive write access to a memex root. Acquired by `Memex::open_writer`. |
| Daemon lifetime `~/.memex/daemon.lock` | `cli/src/daemon/lock.rs` | Single daemon instance per root. |
| Per-slug async lock (`TokioMutex`) | `cli/src/daemon/handler/mod.rs::acquire_slug_lock(s)` | Serializes concurrent writes to the *same* slug; different slugs proceed in parallel. Held across the per-slug MERGE LLM call. |
| Embed-model mutex (`Arc<TokioMutex<Box<dyn Embedder>>>`) | `cli/src/daemon/handler/mod.rs` | Serializes use of the single warm embedding model. |

Because the daemon is the writer, `Memex::open_writer` (which takes the `.lock` flock) is now reached on only two paths: the `rechunk` CLI command (still a direct in-process writer), and `apply_fix_locked` when `lint --fix` repairs an issue. Everything else mutates through the daemon's per-slug + embed-model locks.

## Lock scope per operation

- **`write` / `delete`** — daemon-routed; the per-slug lock is held only during the mutation, not for the client round-trip.
- **`ingest`** — daemon-routed; per-slug fan-out, each slug locked from re-read through DB commit + embed (`store_extracted_pages`). The dedup search runs lock-free before locking the union of affected slugs.
- **`lint --fix`** — reader pre-scan with no lock, then per-issue `apply_fix_locked`: opens a *fresh* DB connection under the writer lock, re-verifies the issue is still present (`FixOutcome::Stale` if already fixed), applies, releases.
- **`rechunk`** — direct `Memex::open_writer`; holds the writer flock for the operation.
- **`search` / `read`** — no lock.

## Atomic write protocol

`core/src/storage.rs::atomic_write`:

1. Write to a temp file `.{name}.{nonce}.tmp` (8-char hex nonce, no PID).
2. `fsync` the temp file.
3. `rename` over the target (atomic on POSIX).
4. `fsync` the parent directory (POSIX).

Transient failures (`EBUSY`, `EAGAIN`, `EINTR`, `ETXTBSY`, `ESTALE`; Windows sharing/lock-violation) retry on a fixed schedule (9 attempts, ~1.9 s total). Permanent failures (e.g. `PermissionDenied` on POSIX) fail fast. Stale temp files (`.{name}.{nonce}.tmp` older than 1 hour) are cleaned up under the writer lock on `open_writer`.

## Configuration

`~/.memex/config.toml`:

```toml
[locking]
timeout_seconds = 120   # range [1, 3600]
```

Override via `MEMEX_LOCK_TIMEOUT_SECONDS`. Precedence: env > file > default (120 s). Loader + validation in `core/src/config.rs`.

## Error model & exit codes

Variants (`core/src/error.rs`): `LockTimeout`, `LockAcquireIo`, `FileOpExhausted`, `FileOpFailed`, `MalformedConfig`, `InvalidEnvVar`. CLI exit-code mapping (`cli/src/main.rs`): `LockTimeout → 2`, `FileOpExhausted → 3`, everything else `→ 1`, success `0`.

## Known gap

Backlink rewrites during cross-linking currently skip failures silently (`core/src/crosslink.rs` — a failed `atomic_write` is not recorded), and `Event::Written.backlinked` lists successes only. A per-item backlink-failure report was designed but not implemented; until then a failed backlink rewrite is invisible except via a later `lint` pass.
