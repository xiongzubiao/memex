//! Daemon main event loop.

use crate::daemon::config::Config;
use crate::daemon::handler::{HandlerState, handle};
use crate::daemon::lock::{TryAcquire, try_acquire};
use crate::daemon::pidfile;
use crate::daemon::protocol::{Event, Request};
use crate::memex_root;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;
use tokio::time::{Instant, timeout};
use tracing::{error, info, warn};

pub struct DaemonPaths {
    pub lock: PathBuf,
    pub socket: PathBuf,
    pub pid: PathBuf,
}

impl DaemonPaths {
    pub fn default_under(memex_home: &Path) -> Self {
        Self {
            lock: memex_home.join("daemon.lock"),
            socket: memex_home.join("daemon.sock"),
            pid: memex_home.join("daemon.pid"),
        }
    }
}

pub enum StartOutcome {
    /// We were THE daemon; ran to completion (idle timeout or SIGTERM).
    RanAsDaemon,
    /// Another daemon was already running; we exited cleanly.
    AlreadyRunning,
}

/// Files-per-chunk for chunked reconcile. Embed-model lock is acquired
/// once per chunk and released between chunks so concurrent ingest
/// embeds can interleave during long sweeps. 32 balances lock churn
/// (each acquisition is microseconds) against ingest tail latency.
const RECONCILE_CHUNK: usize = 32;

/// Run a reconcile pass with chunked embed-model locking: walks once
/// (no embedder needed), then per-file indexes in `RECONCILE_CHUNK`
/// batches with the lock held only across each batch. Releases between
/// batches, deletes stale doc rows at the end (no lock needed).
pub(crate) async fn reconcile_chunked(
    memex: &memex_core::Memex,
    embed_model: &crate::daemon::handler::SharedEmbedder,
    opts: memex_core::reconcile::ReconcileOptions,
) -> memex_core::error::Result<memex_core::reconcile::ReconcileReport> {
    let plan = memex_core::reconcile::reconcile_walk(memex, opts)?;
    let mut report = memex_core::reconcile::ReconcileReport {
        skipped_symlinks: plan.skipped_symlinks,
        ..Default::default()
    };
    for chunk in plan.wiki_paths.chunks(RECONCILE_CHUNK) {
        let mut guard = embed_model.lock().await;
        for path in chunk {
            match memex_core::index_wiki::index_wiki_file(memex, path, Some(guard.as_mut())) {
                Ok(memex_core::index_wiki::IndexOutcome::Skipped) => report.skipped += 1,
                Ok(_) => report.indexed += 1,
                Err(e) => {
                    tracing::warn!(path=%path.display(), %e, "reconcile: skip wiki file");
                    report.skipped += 1;
                }
            }
        }
    }
    for chunk in plan.raw_paths.chunks(RECONCILE_CHUNK) {
        let mut guard = embed_model.lock().await;
        for path in chunk {
            match memex_core::index_raw::index_raw_file(memex, path, Some(guard.as_mut())) {
                Ok(memex_core::index_raw::IndexOutcome::Skipped) => report.skipped += 1,
                Ok(memex_core::index_raw::IndexOutcome::HashMismatch) => {
                    report.hash_mismatches += 1;
                }
                Ok(_) => report.indexed += 1,
                Err(e) => {
                    tracing::warn!(path=%path.display(), %e, "reconcile: skip raw file");
                    report.skipped += 1;
                }
            }
        }
    }
    for path in &plan.to_delete {
        if let Err(e) = memex.search().delete_document_with_cleanup(path) {
            tracing::warn!(?e, %path, "reconcile: delete failed");
        } else {
            report.deleted += 1;
        }
    }
    Ok(report)
}

/// Run the daemon main loop. Blocks until SIGTERM or idle timeout.
pub async fn run_daemon(paths: DaemonPaths, cfg: Config) -> Result<StartOutcome> {
    // 1. Lock 2.
    let _guard = match try_acquire(&paths.lock).context("acquiring lock 2")? {
        TryAcquire::Acquired(g) => g,
        TryAcquire::Busy => {
            info!(lock = %paths.lock.display(), "daemon already running, exiting cleanly");
            return Ok(StartOutcome::AlreadyRunning);
        }
    };

    // 2. PID file.
    let pid = std::process::id();
    pidfile::write(&paths.pid, pid).context("writing pid file")?;
    let pid_path = paths.pid.clone();

    // 3. Stale socket cleanup (we hold the flock — safe to unlink). Use
    // `symlink_metadata` so we don't follow a symlink. Refuse to unlink
    // if the path is a symlink: a malicious or broken symlink at
    // `paths.socket` pointing at user data must not cause us to destroy
    // the target. Unlinking a stale regular file or a real socket is OK
    // (both are plausible after a crash).
    match std::fs::symlink_metadata(&paths.socket) {
        Ok(md) => {
            if md.file_type().is_symlink() {
                anyhow::bail!("refusing to unlink {:?}: path is a symlink", paths.socket);
            }
            std::fs::remove_file(&paths.socket)
                .with_context(|| format!("removing stale socket {:?}", paths.socket))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("stat {:?}", paths.socket));
        }
    }

    // 4. Bind.
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("binding socket {:?}", paths.socket))?;
    // Restrict the socket to the owning user. Without this it inherits
    // umask (typically 0755), letting any local user connect and submit
    // ingest/write requests against the daemon's bound root.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(&paths.socket, perms) {
            tracing::warn!(?e, socket = %paths.socket.display(), "set socket mode 0600 failed");
        }
    }
    info!(socket = %paths.socket.display(), pid, "daemon listening");

    // Install signal handlers BEFORE the slow startup work (model load,
    // reconcile, watcher). Otherwise SIGTERM during startup hits the
    // default handler and kills the process without graceful logging,
    // and a user who runs `daemon stop` right after `daemon start` sees
    // a half-initialized daemon vanish silently. The Notify is created
    // here and consumed by the main accept loop later — `notify_one`
    // stores a permit if the loop hasn't reached `notified()` yet, so
    // an early signal isn't lost.
    let shutdown = Arc::new(Notify::new());
    {
        let shutdown_signal = shutdown.clone();
        tokio::spawn(async move {
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    error!(?e, "failed to install SIGTERM handler");
                    return;
                }
            };
            let mut sigint = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    error!(?e, "failed to install SIGINT handler");
                    return;
                }
            };
            tokio::select! {
                _ = sigterm.recv() => info!("received SIGTERM"),
                _ = sigint.recv() => info!("received SIGINT"),
            }
            shutdown_signal.notify_one();
        });
    }

    let memex_handle = crate::daemon::memex_handle::MemexHandle::new();

    // Create local settings to disable plugins for benchmarking.
    let root = memex_root();
    let settings_path = root.join(".claude").join("settings.json");
    if !settings_path.exists() {
        let _ = std::fs::create_dir_all(root.join(".claude"));
        let _ = std::fs::write(&settings_path, r#"{"enabledPlugins":{}}"#);
    }

    // Load the embedding model once at startup — fatal if it can't load.
    // The daemon is the single owner of the warm model; the handler
    // (search/dedup/embed_document) and the retrieval actor (query
    // embeds) share this single instance via `Arc<TokioMutex<...>>`.
    // Silently running without one used to return wrong slugs from
    // BM25-only fallbacks, which corrupted ingest dedup.
    memex_core::embed::init_runtime().map_err(|e| {
        anyhow::anyhow!(
            "ONNX runtime init failed: {e}. Install libonnxruntime via \
             `brew install onnxruntime` (macOS), your distro's package \
             manager (Linux), or download from \
             https://github.com/microsoft/onnxruntime/releases and place \
             the dylib at ~/.memex/lib/."
        )
    })?;
    let model = memex_core::retrieval::load_default_model().map_err(|e| {
        anyhow::anyhow!(
            "embedding model load failed: {e}. Set MEMEX_EMBED_MODEL_PATH \
             or run `memex models install`."
        )
    })?;
    let embed_model = crate::daemon::handler::shared_embedder(model);

    let retrieval_tx =
        crate::daemon::retrieval::spawn(memex_handle.clone(), embed_model.clone());

    let pool = crate::daemon::worker::WorkerPool::new(cfg.daemon.worker.clone(), memex_handle.clone());
    let cfg = Arc::new(cfg);
    let reader_session = crate::daemon::handler::ReaderSession {
        bound_root: root.clone(),
        memex_handle: memex_handle.clone(),
        embed_model: embed_model.clone(),
    };
    let writer_session = crate::daemon::handler::WriterSession {
        reader: reader_session,
        slug_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        content_hash_locks: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };
    let state = Arc::new(HandlerState {
        pid,
        started_at: Utc::now(),
        retrieval: retrieval_tx,
        jobs: Arc::new(pool),
        config: cfg.clone(),
        writer: writer_session,
    });

    // 5. Startup reconcile: recover from a missing or corrupt index.db.
    // Pass the warm embedder so reconcile fills in vector chunks for
    // any newly-indexed files. Without this, files indexed at startup
    // land in `documents` + FTS but skip embedding, and the stat-based
    // skip in `index_wiki_file` then prevents any later write path
    // from filling them in — queries return `retrieval_empty`.
    match state.writer.memex_handle().get_or_open(&root) {
        Ok(memex) => {
            // Housekeeping: prune terminal `ingest_jobs` rows older than
            // 30 days, and `llm_cache` rows older than 90 days. Bounds
            // table growth on long-running daemons without losing
            // recent history. The cache TTL is longer because each
            // entry is more expensive to rebuild (one LLM call) and
            // because `cache_key` already includes the model name —
            // model upgrades produce new keys; old entries age out
            // naturally rather than serving stale results.
            if let Err(e) = memex.search().prune_terminal_ingest_jobs(30) {
                warn!(?e, "ingest_jobs prune failed; continuing");
            }
            match memex.search().prune_llm_cache(90) {
                Ok(n) if n > 0 => info!(
                    cache_pruned = n,
                    "pruned stale llm_cache entries (older than 90 days)"
                ),
                Ok(_) => {}
                Err(e) => warn!(?e, "llm_cache prune failed; continuing"),
            }

            // Stuck-job recovery: any rows still in `pending` or
            // `processing` were left there by a daemon that crashed
            // mid-job (or was killed before the worker handed back a
            // result). Mark them `failed` with a clear reason so the
            // table doesn't hold them as in-flight forever; the user
            // re-runs ingest if they still want the work done.
            match memex.search().recover_stuck_ingest_jobs() {
                Ok(n) if n > 0 => info!(stuck_jobs = n, "marked stuck ingest jobs as failed"),
                Ok(_) => {}
                Err(e) => warn!(?e, "stuck-job recovery failed; continuing"),
            }

            match reconcile_chunked(&memex, &embed_model, Default::default()).await {
                Ok(r) => info!(
                    indexed = r.indexed,
                    deleted = r.deleted,
                    hash_mismatches = r.hash_mismatches,
                    skipped_symlinks = r.skipped_symlinks,
                    "startup reconcile complete"
                ),
                Err(e) => warn!(?e, "startup reconcile failed; continuing with current DB"),
            }
        }
        Err(e) => {
            // If index.db exists and is non-empty but failed to open
            // (e.g. truncated, garbage bytes from a partial write, an
            // SQLite version mismatch), back it up and rebuild from
            // the filesystem. The wiki+raw trees are canonical.
            let db_path = root.join(memex_core::INDEX_DB_NAME);
            let db_size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
            if db_size > 0 {
                let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
                let backup = root.join(format!("{}.corrupt-{stamp}", memex_core::INDEX_DB_NAME));
                warn!(
                    ?e,
                    backup = %backup.display(),
                    "index.db open failed; backing up corrupt file and rebuilding from disk"
                );
                let mut recovered = false;
                if let Err(e2) = std::fs::rename(&db_path, &backup) {
                    warn!(?e2, "could not move corrupt index.db aside; deferring recovery");
                } else {
                    // Stale WAL/SHM from the corrupt DB are also unusable.
                    let _ = std::fs::remove_file(root.join(format!(
                        "{}-wal", memex_core::INDEX_DB_NAME
                    )));
                    let _ = std::fs::remove_file(root.join(format!(
                        "{}-shm", memex_core::INDEX_DB_NAME
                    )));
                    match state.writer.memex_handle().get_or_open(&root) {
                        Ok(memex) => {
                            info!("rebuilt fresh index.db after corruption; running reconcile");
                            match reconcile_chunked(
                                &memex,
                                &embed_model,
                                Default::default(),
                            )
                            .await
                            {
                                Ok(r) => {
                                    info!(
                                        indexed = r.indexed,
                                        deleted = r.deleted,
                                        hash_mismatches = r.hash_mismatches,
                                        skipped_symlinks = r.skipped_symlinks,
                                        "post-corruption reconcile complete"
                                    );
                                    recovered = true;
                                }
                                Err(e3) => {
                                    warn!(?e3, "post-corruption reconcile failed");
                                }
                            }
                        }
                        Err(e3) => {
                            warn!(?e3, "could not re-open index.db even after backing up corrupt file");
                        }
                    }
                }
                if !recovered {
                    warn!("DB recovery did not complete; daemon running in degraded state");
                }
            } else {
                warn!(?e, "could not open memex for startup reconcile; deferring");
            }
        }
    }

    // 6. Watcher: detect external edits and re-index. The reconcile
    // timer below (step 6b) is the safety net; `native_unreliable`
    // collected here decides whether the timer needs the short
    // cadence. Set to true on (a) network filesystems where native
    // backends drop events, (b) native init failure on local disks
    // (inotify exhaustion, missing backend), or (c) spawn_watcher
    // returning Err with no usable watcher at all.
    let watch_root = state.writer.bound_root().to_path_buf();
    let mut native_unreliable = false;
    match state.writer.memex_handle().get_or_open(&watch_root) {
        Ok(watch_memex) => {
            let (watch_tx, watch_rx) =
                tokio::sync::mpsc::channel::<crate::daemon::watcher::WatcherEvent>(64);
            let wiki_dir = watch_memex.wiki_dir();
            let raw_dir = watch_memex.raw_dir();
            native_unreliable = crate::daemon::fs_kind::is_network_fs(&wiki_dir)
                || crate::daemon::fs_kind::is_network_fs(&raw_dir);
            if native_unreliable {
                info!("watcher: network filesystem detected");
            }
            match crate::daemon::watcher::spawn_watcher(
                crate::daemon::watcher::WatcherConfig { wiki_dir, raw_dir },
                watch_tx,
            ) {
                Ok(watcher) => {
                    if !watcher.native_active {
                        info!("watcher: native backend unavailable");
                        native_unreliable = true;
                    }
                    // Keep watcher alive for daemon lifetime by storing in a task.
                    let memex_handle_for_watch = state.writer.memex_handle().clone();
                    let embed_model_for_watch = embed_model.clone();
                    tokio::spawn(async move {
                        let _keep_alive = watcher;
                        let mut watch_rx = watch_rx;
                        while let Some(evt) = watch_rx.recv().await {
                            if let Err(e) = crate::daemon::watcher::handle_watch_event(
                                &memex_handle_for_watch,
                                &embed_model_for_watch,
                                evt,
                            )
                            .await
                            {
                                tracing::warn!(?e, "watch event handling failed");
                            }
                        }
                    });
                }
                Err(e) => {
                    warn!(?e, "watcher startup failed; daemon will run without filesystem watching");
                    native_unreliable = true;
                }
            }
        }
        Err(e) => {
            warn!(?e, "could not open memex for watcher; skipping watcher startup");
        }
    }

    // 6b. Periodic reconcile timer. Single mechanism for catching drift
    // the native watcher misses, including:
    // - Phase 9 (DB commit) failures where raw+wiki landed but the
    //   `documents` row didn't (no file event fires on retry).
    // - Network-filesystem hosts where native watchers can't deliver
    //   events reliably — used to be a separate polling-watcher loop;
    //   now this timer covers both cases.
    // - General drift between filesystem and index.
    //
    // Interval scales with watcher health: when native is unreliable
    // (network FS or init failure) we tick on the shorter
    // `core.poll_interval_sec` so changes don't sit undetected for an
    // hour; otherwise we use `daemon.reconcile_interval_sec` (default
    // 1h) since drift on native watchers is rare and reconcile costs
    // CPU. Reconcile is idempotent and skips files whose mtime+size
    // match, so no-op passes are cheap.
    if cfg.daemon.reconcile_interval_sec > 0 {
        let memex_for_interval = state.writer.memex_handle().get_or_open(&root).ok();
        let interval = if native_unreliable {
            let poll_interval_sec = memex_for_interval
                .as_ref()
                .map(|m| m.config().poll_interval_sec)
                .unwrap_or(300);
            Duration::from_secs(poll_interval_sec.min(cfg.daemon.reconcile_interval_sec))
        } else {
            Duration::from_secs(cfg.daemon.reconcile_interval_sec)
        };
        let memex_handle_for_reconcile = state.writer.memex_handle().clone();
        let embed_model_for_reconcile = embed_model.clone();
        let root_for_reconcile = root.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Default `Burst` missed-tick behavior would fire every
            // skipped tick back-to-back if a reconcile pass overruns
            // the interval (large roots, short intervals, or contention
            // on the embed model lock can cause this). `Delay` waits a
            // full interval from the end of the previous pass, bounding
            // CPU/IO pressure to one reconcile-per-interval-window
            // regardless of how long any single pass takes.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately; skip it because startup
            // reconcile already ran. Wait one full interval before the
            // first periodic pass.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let memex = match memex_handle_for_reconcile.get_or_open(&root_for_reconcile) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(?e, "periodic reconcile: get_or_open failed; skipping pass");
                        continue;
                    }
                };
                match reconcile_chunked(
                    &memex,
                    &embed_model_for_reconcile,
                    Default::default(),
                )
                .await
                {
                    Ok(r) => {
                        if r.indexed > 0 || r.deleted > 0 || r.hash_mismatches > 0 {
                            info!(
                                indexed = r.indexed,
                                deleted = r.deleted,
                                hash_mismatches = r.hash_mismatches,
                                "periodic reconcile: drift detected and corrected"
                            );
                        }
                    }
                    Err(e) => warn!(?e, "periodic reconcile failed; will retry next interval"),
                }
            }
        });
        info!(
            interval_sec = interval.as_secs(),
            native_unreliable,
            "periodic reconcile timer armed"
        );
    }

    // 7. Accept loop with idle timeout + SIGTERM. Signal handlers were
    // installed early (just after socket bind) so SIGTERM during slow
    // startup doesn't kill the process before tracing flushes; the
    // shared Notify carries the signal across the gap. `notify_one`
    // stores a permit if the main loop hasn't reached `notified()` yet,
    // so an early signal isn't lost. notify_waiters would lose the
    // signal whenever the main loop is between `select!` calls (e.g.
    // mid-accept dispatch), causing the daemon to keep serving
    // requests after SIGTERM until the idle timeout.
    let idle_timeout = Duration::from_secs(cfg.daemon.idle_timeout_min * 60);
    // Shared across all connection tasks so long-running queries keep the
    // daemon alive: we only time out when in_flight == 0 AND last_activity
    // has been stale for the full idle_timeout.
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let in_flight = Arc::new(AtomicUsize::new(0));

    // Self-reap heartbeat: every `SOCKET_CHECK_INTERVAL` we stat the
    // socket file. If it's gone, our memex root has been deleted out
    // from under us (typical: a test's TempDir was dropped) — exit
    // immediately so we don't leak. Without this, the daemon keeps
    // running until the 15-minute idle timeout, and concurrent tests
    // each spawn fresh daemons that pile up.
    const SOCKET_CHECK_INTERVAL: Duration = Duration::from_secs(2);
    let socket_path = paths.socket.clone();

    loop {
        let elapsed = last_activity.lock().unwrap().elapsed();
        if in_flight.load(Ordering::Acquire) == 0 && elapsed >= idle_timeout {
            info!(idle_for_secs = elapsed.as_secs(), "idle timeout reached");
            break;
        }
        if !socket_path.exists() {
            info!(
                socket = %socket_path.display(),
                "socket file removed; bound root is gone, exiting"
            );
            break;
        }
        let sleep_for = idle_timeout
            .saturating_sub(elapsed)
            .min(SOCKET_CHECK_INTERVAL);
        tokio::select! {
            _ = shutdown.notified() => {
                info!("shutting down from signal");
                break;
            }
            _ = tokio::time::sleep(sleep_for) => {
                // Next iteration re-checks idle timeout AND socket presence.
            }
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        *last_activity.lock().unwrap() = Instant::now();
                        in_flight.fetch_add(1, Ordering::AcqRel);
                        let state = state.clone();
                        let last_activity = last_activity.clone();
                        let in_flight = in_flight.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, &state).await {
                                warn!(?e, "connection error");
                            }
                            *last_activity.lock().unwrap() = Instant::now();
                            in_flight.fetch_sub(1, Ordering::AcqRel);
                        });
                    }
                    Err(e) => {
                        error!(?e, "accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    // Graceful drain: stop accepting new connections, then give a brief
    // window for in-flight work to finish. Short tasks (writes, cached
    // queries) complete in well under a second; long ones (cold LLM
    // expand/synth) typically run 5–30 s and won't finish within any
    // reasonable shutdown budget anyway. Each worker's agent subprocess
    // is killed on drop via `kill_on_drop`, so exceeding the deadline
    // still exits cleanly; the user's in-flight query fails with a
    // dropped reply channel. Keep the drain short — `daemon stop`
    // means "stop now," not "let me finish that 30 s LLM call first."
    drop(listener);
    let drain_timeout = Duration::from_secs(state.config.daemon.drain_timeout_sec);
    let drain_start = Instant::now();
    while in_flight.load(Ordering::Acquire) > 0 {
        if drain_start.elapsed() >= drain_timeout {
            warn!(
                in_flight = in_flight.load(Ordering::Acquire),
                drain_timeout_sec = state.config.daemon.drain_timeout_sec,
                "drain timeout — exiting with in-flight jobs (raise [daemon] drain_timeout_sec for LLM-heavy workloads)"
            );
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Cleanup.
    let _ = std::fs::remove_file(&paths.socket);
    let _ = pidfile::remove(&pid_path);
    // Flock is released as `_guard` drops at end of function.
    Ok(StartOutcome::RanAsDaemon)
}

async fn serve_connection(stream: UnixStream, state: &HandlerState) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let (read_half, mut write_half) = stream.into_split();
    // Cap the request line at INGEST_MAX_BYTES + 64KB framing slop. Without
    // this, a misbehaving local client (e.g. a buggy gateway hook handler)
    // could send an unbounded TranscriptInline.content payload and OOM the
    // daemon during the line read.
    let req_cap = (crate::daemon::config::INGEST_MAX_BYTES + 64 * 1024) as u64;
    let mut reader = BufReader::new(read_half.take(req_cap));
    let mut line = String::new();

    // One request per connection. Read the first line, parse, dispatch.
    // 30 s ceiling covers a 5 MB content payload over a slow Unix socket with
    // comfortable margin; small requests aren't taxed (timeout fires only if
    // the read actually takes that long).
    let n = timeout(Duration::from_secs(30), reader.read_line(&mut line))
        .await
        .context("timed out waiting for request")??;
    if n == 0 {
        // Client disconnected without sending a line. Nothing to do.
        return Ok(());
    }

    let events = match serde_json::from_str::<Request>(line.trim_end()) {
        Ok(req) => handle(req, state).await,
        Err(e) => vec![
            Event::Error {
                code: "bad_request".into(),
                message: e.to_string(),
                status: 1,
            },
            Event::Done { status: 1 },
        ],
    };

    for ev in events {
        let mut s = serde_json::to_string(&ev).context("serializing event")?;
        s.push('\n');
        write_half
            .write_all(s.as_bytes())
            .await
            .context("writing event")?;
    }
    write_half.shutdown().await.ok();
    Ok(())
}
