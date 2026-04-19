//! Daemon main event loop.

use crate::daemon::config::Config;
use crate::daemon::handler::{HandlerState, handle};
use crate::daemon::lock::{TryAcquire, try_acquire};
use crate::daemon::pidfile;
use crate::daemon::protocol::{Event, Request};
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
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

/// Run the daemon in the foreground. Blocks until SIGTERM or idle timeout.
pub async fn run_foreground(paths: DaemonPaths, cfg: Config) -> Result<StartOutcome> {
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
                anyhow::bail!(
                    "refusing to unlink {:?}: path is a symlink",
                    paths.socket
                );
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
    info!(socket = %paths.socket.display(), pid, "daemon listening");

    let retrieval_tx = crate::daemon::retrieval::spawn();
    let pool = crate::daemon::worker::WorkerPool::new(cfg.daemon.worker.clone());
    let state = Arc::new(HandlerState {
        pid,
        started_at: Utc::now(),
        retrieval: retrieval_tx,
        jobs: Arc::new(pool),
    });

    // 5. Accept loop with idle timeout + SIGTERM.
    let shutdown = Arc::new(Notify::new());
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
        shutdown_signal.notify_waiters();
    });

    let idle_timeout = Duration::from_secs(cfg.daemon.idle_timeout_min * 60);
    // Shared across all connection tasks so long-running queries keep the
    // daemon alive: we only time out when in_flight == 0 AND last_activity
    // has been stale for the full idle_timeout.
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let in_flight = Arc::new(AtomicUsize::new(0));

    loop {
        let elapsed = last_activity.lock().unwrap().elapsed();
        if in_flight.load(Ordering::Acquire) == 0 && elapsed >= idle_timeout {
            info!(idle_for_secs = elapsed.as_secs(), "idle timeout reached");
            break;
        }
        let sleep_for = idle_timeout.saturating_sub(elapsed);
        tokio::select! {
            _ = shutdown.notified() => {
                info!("shutting down from signal");
                break;
            }
            _ = tokio::time::sleep(sleep_for) => {
                // Next iteration's idle check will break if we're over.
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

    // Graceful drain: stop accepting new connections, then wait up to
    // `DRAIN_TIMEOUT` for in-flight work to finish. Each worker's agent
    // subprocess is killed on drop via `kill_on_drop`, so exceeding the
    // deadline still exits cleanly; the user's in-flight query just fails
    // with a dropped reply channel rather than undefined half-written state.
    drop(listener);
    const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
    let drain_start = Instant::now();
    while in_flight.load(Ordering::Acquire) > 0 {
        if drain_start.elapsed() >= DRAIN_TIMEOUT {
            warn!(
                in_flight = in_flight.load(Ordering::Acquire),
                "drain timeout — exiting with in-flight jobs"
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
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    // One request per connection. Read the first line, parse, dispatch.
    let n = timeout(Duration::from_secs(5), reader.read_line(&mut line))
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
