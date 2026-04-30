//! Filesystem watcher: emits change events from wiki/raw trees.
//!
//! Native backend (notify::recommended_watcher) on Linux/macOS/Windows.
//! Polling fallback for network drives or hosts where the native backend
//! is unavailable. Both backends produce `WatcherEvent` values on the
//! caller-provided channel; the consumer reindexes synchronously.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum WatcherEvent {
    Touch(PathBuf),
    Remove(PathBuf),
    Rescan,
}

pub struct WatcherConfig {
    pub wiki_dir: PathBuf,
    pub raw_dir: PathBuf,
    pub poll_interval: Duration,
    pub force_polling: bool,
}

pub struct WatcherHandle {
    _watcher: Option<RecommendedWatcher>,
    _poll: Option<tokio::task::JoinHandle<()>>,
}

pub fn spawn_watcher(cfg: WatcherConfig, tx: mpsc::Sender<WatcherEvent>) -> Result<WatcherHandle> {
    let tx = Arc::new(tx);
    let mut watcher_handle: Option<RecommendedWatcher> = None;
    if !cfg.force_polling {
        match start_native(&cfg, tx.clone()) {
            Ok(w) => watcher_handle = Some(w),
            Err(e) => tracing::warn!(?e, "native watcher unavailable; falling back to polling"),
        }
    }
    let needs_poll = watcher_handle.is_none() || cfg.force_polling;
    let poll = if needs_poll {
        Some(start_polling(cfg.poll_interval, tx.clone()))
    } else {
        None
    };
    Ok(WatcherHandle {
        _watcher: watcher_handle,
        _poll: poll,
    })
}

fn start_native(
    cfg: &WatcherConfig,
    tx: Arc<mpsc::Sender<WatcherEvent>>,
) -> Result<RecommendedWatcher> {
    let mut w = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        let evt = match res {
            Ok(e) => e,
            Err(e) => {
                // notify can drop events on inotify queue overflow
                // (Linux) or backend-specific signal loss (FSEvents
                // coalescing under load). The error path doesn't tell
                // us which events were lost, so treat any watcher
                // error as "state may have desynced" and queue a
                // Rescan. Rescan triggers reconcile which is
                // idempotent — over-eager beats silently missed
                // deletes that survive until the next poll cycle.
                tracing::warn!(error = %e, "watcher error; queuing rescan");
                let _ = tx.blocking_send(WatcherEvent::Rescan);
                return;
            }
        };
        // `Modify(Name(Both))` arrives as a single event whose paths are
        // [from, to] — emit Remove for the source and Touch for the
        // destination so the old documents row is cleaned up. macOS
        // FSEvents and some Windows backends coalesce renames this way;
        // Linux inotify usually splits into From + To, handled in the
        // per-path loop below.
        if matches!(
            evt.kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::Both))
        ) && evt.paths.len() == 2
        {
            let _ = tx.blocking_send(WatcherEvent::Remove(evt.paths[0].clone()));
            let _ = tx.blocking_send(WatcherEvent::Touch(evt.paths[1].clone()));
            return;
        }
        for path in evt.paths {
            let outbound = match evt.kind {
                EventKind::Remove(_) => Some(WatcherEvent::Remove(path)),
                // A `mv old.md new.md` fires Modify(Name(From)) for the
                // old path and Modify(Name(To)) for the new — without
                // this branch the From event got dispatched as Touch,
                // which tried to re-index a path that no longer exists,
                // leaving the old documents row in place forever. Treat
                // From as a delete; the matching To event indexes the
                // new path through the normal Create/Modify branch.
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    Some(WatcherEvent::Remove(path))
                }
                EventKind::Create(_) | EventKind::Modify(_) => Some(WatcherEvent::Touch(path)),
                _ => None,
            };
            if let Some(e) = outbound {
                // notify's callback runs on its own thread (not a tokio
                // task), so blocking_send is safe and applies backpressure
                // to the watcher backend rather than dropping events. If
                // the consumer is wedged, inotify's kernel queue is the
                // ultimate buffer; an overflow surfaces as a notify error
                // event in the next callback (handled above).
                let _ = tx.blocking_send(e);
            }
        }
    })?;
    if cfg.wiki_dir.exists() {
        w.watch(&cfg.wiki_dir, RecursiveMode::Recursive)?;
    }
    if cfg.raw_dir.exists() {
        w.watch(&cfg.raw_dir, RecursiveMode::Recursive)?;
    }
    Ok(w)
}

fn start_polling(
    interval: Duration,
    tx: Arc<mpsc::Sender<WatcherEvent>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if tx.send(WatcherEvent::Rescan).await.is_err() {
                break;
            }
        }
    })
}

/// Apply a single watcher event to the index. Called serially by the consumer task.
///
/// `embed_model` is the shared embedder — locked per event so the touched
/// doc gets indexed and embedded under the same critical section that
/// `handle_write` uses, keeping vector chunks in sync with `documents`.
pub async fn handle_watch_event(
    cache: &crate::daemon::memex_handle::MemexHandle,
    embed_model: &crate::daemon::handler::SharedEmbedder,
    evt: WatcherEvent,
) -> anyhow::Result<()> {
    let m = cache.get_or_open(&crate::memex_root())?;
    match evt {
        WatcherEvent::Touch(path) => {
            // notify emits Create events for parent directories (raw/<hh>/)
            // and for atomic-write .tmp files that have already been renamed
            // away. Both must no-op rather than surface as warnings.
            // Use `symlink_metadata` so a symlink doesn't masquerade as a
            // regular file: reconcile's WalkDir already skips symlinks
            // for the security boundary (a symlink in wiki/ pointing at
            // /etc/passwd would otherwise expose its target's contents
            // through `memex query`); the watcher must match.
            let md = std::fs::symlink_metadata(&path).ok();
            let is_regular_file = md.as_ref().map(|m| m.is_file()).unwrap_or(false);
            if !is_regular_file {
                // The path is missing. Two cases land here:
                // 1. atomic-write tmp file already renamed away — no
                //    documents row exists, the cleanup below is a no-op
                // 2. a rename whose Old side reached us as Touch
                //    (RenameMode::Any/Other or malformed Both) — the
                //    documents row at this path is stale. Without the
                //    cleanup it survives until the next poll-driven
                //    reconcile; on a long-running daemon that may
                //    never happen.
                // Drop events whose paths aren't under the memex root —
                // documents.path stores relative paths, so an absolute
                // outside-root string would never match a row anyway,
                // and forwarding it just leaks a stray absolute path
                // into logs if the cleanup errors.
                let Ok(rel) = path.strip_prefix(m.root()) else {
                    return Ok(());
                };
                let rel_str = memex_core::storage::rel_path_string(rel);
                m.search().delete_document_with_cleanup(&rel_str)?;
                return Ok(());
            }
            // Wiki has a flat namespace: <wiki_dir>/<slug>.md (depth 1).
            // Raw is 2 levels: <raw_dir>/<hh>/<rest>. Reject deeper paths
            // so subdir-dropped files don't get indexed under a name lint
            // can't see (lint walks top-level only).
            if path.starts_with(m.wiki_dir())
                && path.extension().and_then(|e| e.to_str()) == Some("md")
                && path.parent() == Some(m.wiki_dir().as_path())
            {
                let mut guard = embed_model.lock().await;
                memex_core::index_wiki::index_wiki_file(&m, &path, Some(guard.as_mut()))?;
            } else if path.starts_with(m.raw_dir())
                && path
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|p| p == m.raw_dir().as_path())
                    .unwrap_or(false)
            {
                let mut guard = embed_model.lock().await;
                memex_core::index_raw::index_raw_file(&m, &path, Some(guard.as_mut()))?;
            }
        }
        WatcherEvent::Remove(path) => {
            let rel = path.strip_prefix(m.root()).unwrap_or(&path);
            let rel_str = memex_core::storage::rel_path_string(rel);
            m.search().delete_document_with_cleanup(&rel_str)?;
        }
        WatcherEvent::Rescan => {
            let mut guard = embed_model.lock().await;
            memex_core::reconcile::reconcile_with_embed(
                &m,
                Default::default(),
                guard.as_mut(),
            )?;
        }
    }
    Ok(())
}
