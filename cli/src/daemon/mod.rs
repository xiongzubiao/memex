//! Memex daemon subsystem.
//!
//! IPC plumbing (socket + flock + config + ping), retrieval, agent workers,
//! query expansion, and session ingestion.

pub mod client;
pub mod config;
pub mod context;
pub mod error;
pub mod handler;
pub mod lock;
pub mod logging;
pub mod pidfile;
pub mod protocol;
pub mod queue;
pub mod retrieval;
pub mod server;
pub mod worker;

use crate::memex_root;
use anyhow::{Context, Result};
use server::DaemonPaths;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::Instant;

pub fn default_config_path() -> PathBuf {
    if let Ok(p) = std::env::var("MEMEX_CONFIG") {
        return PathBuf::from(p);
    }
    memex_root().join("config.toml")
}

/// `memex daemon start` entrypoint (foreground mode).
pub fn start_foreground() -> Result<i32> {
    let paths = DaemonPaths::default_under(&memex_root());
    let cfg = config::Config::load(&default_config_path()).context("loading config")?;
    let _guard = logging::init(&cfg.daemon.log_file)?;
    tracing::info!("memex daemon starting (foreground)");

    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
    let outcome = rt.block_on(server::run_foreground(paths, cfg))?;
    match outcome {
        server::StartOutcome::RanAsDaemon => Ok(0),
        server::StartOutcome::AlreadyRunning => Ok(0),
    }
}

/// `memex daemon stop` entrypoint.
///
/// Reads the PID file, sends SIGTERM, waits up to 10s for the process to exit.
pub fn stop() -> Result<i32> {
    let paths = DaemonPaths::default_under(&memex_root());
    let pid = match pidfile::read(&paths.pid)? {
        Some(p) => p,
        None => {
            println!("daemon: not running (no pid file at {:?})", paths.pid);
            return Ok(0);
        }
    };
    if !pidfile::is_alive(pid) {
        println!("daemon: stale pid file ({pid} not running); cleaning up");
        pidfile::remove(&paths.pid)?;
        let _ = std::fs::remove_file(&paths.socket);
        return Ok(0);
    }
    // Confirm the PID really is our daemon before sending SIGTERM — between
    // the read of the pidfile and the kill, the real daemon could have
    // exited and its PID been reused by an unrelated process. If the
    // socket is alive, the PID is ours.
    let socket_alive = {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(async {
            tokio::time::timeout(
                Duration::from_millis(500),
                tokio::net::UnixStream::connect(&paths.socket),
            )
            .await
            .is_ok_and(|r| r.is_ok())
        })
    };
    if !socket_alive {
        println!("daemon: pid {pid} exists but socket is not responsive; refusing to signal (possible PID reuse)");
        pidfile::remove(&paths.pid)?;
        let _ = std::fs::remove_file(&paths.socket);
        return Ok(0);
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    // Wait up to 10s.
    for _ in 0..100 {
        if !pidfile::is_alive(pid) {
            println!("daemon: stopped (pid {pid})");
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("daemon: still running after 10s SIGTERM; giving up");
    Ok(1)
}

/// `memex daemon status` entrypoint.
///
/// Checks the PID file + flock + ping, reports daemon state.
pub fn status() -> Result<i32> {
    let paths = DaemonPaths::default_under(&memex_root());
    let pid = pidfile::read(&paths.pid)?;
    match pid {
        None => {
            println!("daemon: not running");
            Ok(0)
        }
        Some(p) if !pidfile::is_alive(p) => {
            println!("daemon: stale pid file ({p} not running)");
            Ok(0)
        }
        Some(p) => {
            // Try a ping.
            let rt = tokio::runtime::Runtime::new()?;
            let result = rt.block_on(async {
                let stream = client::connect_with_retry(
                    &paths.socket,
                    Instant::now() + Duration::from_secs(2),
                )
                .await?;
                let events = client::request(stream, &protocol::Request::Ping { v: 1 }).await?;
                anyhow::Ok(events)
            });
            match result {
                Ok(events) => {
                    for ev in &events {
                        if let protocol::Event::Pong { pid, started_at } = ev {
                            println!("daemon: running (pid {pid}, started {started_at})");
                            return Ok(0);
                        }
                    }
                    println!("daemon: pid {p} responded but did not send pong");
                    Ok(1)
                }
                Err(e) => {
                    println!("daemon: pid {p} alive but ping failed ({e})");
                    Ok(1)
                }
            }
        }
    }
}

/// `memex ingest` entrypoint. Sends Request::Ingest to the daemon (auto-spawns if needed).
/// Returns exit code (0 = queued, 1 = error/skipped).
pub fn ingest(transcript_path: &str, agent: &str, root: &str) -> Result<i32> {
    let paths = DaemonPaths::default_under(&memex_root());
    let rt = tokio::runtime::Runtime::new()?;
    let result: anyhow::Result<Vec<protocol::Event>> = rt.block_on(async {
        let stream = client::connect_or_spawn(
            &paths.socket,
            &paths.lock,
            Instant::now() + Duration::from_secs(5),
        )
        .await?;
        let events = client::request(
            stream,
            &protocol::Request::Ingest {
                v: 1,
                transcript_path: transcript_path.to_string(),
                agent: agent.to_string(),
                memex_root: root.to_string(),
            },
        )
        .await?;
        Ok(events)
    });

    let events = match result {
        Ok(e) => e,
        Err(e) => {
            eprintln!("memex ingest: {e}");
            return Ok(1);
        }
    };

    let mut status_code = 1;
    for ev in &events {
        match ev {
            protocol::Event::Queued { job_id, .. } => {
                if !job_id.is_empty() {
                    eprintln!("memex: queued {job_id}");
                }
            }
            protocol::Event::Error { code, message, .. } => {
                eprintln!("memex ingest error ({code}): {message}");
            }
            protocol::Event::Done { status } => {
                status_code = *status;
            }
            _ => {}
        }
    }
    Ok(status_code)
}

/// `memex query <question> --raw` entrypoint. Connects to the running daemon
/// (does NOT auto-spawn — Task 9 adds that). Prints the context pages to stdout
/// in a reader-friendly format. Returns the exit code.
pub fn query_raw(
    question: &str,
    top_k: usize,
    memex_root_override: Option<&std::path::Path>,
) -> Result<i32> {
    let paths = DaemonPaths::default_under(&memex_root());
    let root = memex_root_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(memex_root);

    let rt = tokio::runtime::Runtime::new()?;
    let result: anyhow::Result<Vec<protocol::Event>> = rt.block_on(async {
        let stream = client::connect_or_spawn(
            &paths.socket,
            &paths.lock,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await?;
        let events = client::request(
            stream,
            &protocol::Request::Query {
                v: 1,
                question: question.to_string(),
                raw: true,
                top_k,
                memex_root: root.to_string_lossy().to_string(),
            },
        )
        .await?;
        Ok(events)
    });

    let events = match result {
        Ok(e) => e,
        Err(e) => {
            eprintln!("memex query: {e}");
            return Ok(1);
        }
    };

    let mut status_code = 1;
    for ev in &events {
        match ev {
            protocol::Event::Context { pages } => {
                for p in pages {
                    let rank = p.get("rank").and_then(|v| v.as_u64()).unwrap_or(0);
                    let collection = p.get("collection").and_then(|v| v.as_str()).unwrap_or("");
                    let signal = p.get("signal").and_then(|v| v.as_str()).unwrap_or("");
                    let stem = p.get("stem").and_then(|v| v.as_str()).unwrap_or("");
                    let body = p.get("body").and_then(|v| v.as_str()).unwrap_or("");
                    println!("## [rank {rank}, {collection}, signal {signal}] {stem}");
                    println!("{body}");
                    println!();
                }
            }
            protocol::Event::Error {
                code,
                message,
                status,
            } => {
                eprintln!("memex query error ({code}): {message}");
                status_code = *status;
            }
            protocol::Event::Done { status } => {
                if *status == 0 {
                    status_code = 0;
                }
            }
            _ => {}
        }
    }
    Ok(status_code)
}

/// `memex query <question>` (no `--raw`) entrypoint. Auto-spawns daemon,
/// sends Query{raw:false}, prints the synthesized answer + citations.
pub fn query_synth(
    question: &str,
    top_k: usize,
    memex_root_override: Option<&std::path::Path>,
) -> Result<i32> {
    let paths = server::DaemonPaths::default_under(&memex_root());
    let root = memex_root_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(memex_root);

    let rt = tokio::runtime::Runtime::new()?;
    let result: anyhow::Result<Vec<protocol::Event>> = rt.block_on(async {
        let stream = client::connect_or_spawn(
            &paths.socket,
            &paths.lock,
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await?;
        let events = client::request(
            stream,
            &protocol::Request::Query {
                v: 1,
                question: question.to_string(),
                raw: false,
                top_k,
                memex_root: root.to_string_lossy().to_string(),
            },
        )
        .await?;
        Ok(events)
    });

    let events = match result {
        Ok(e) => e,
        Err(e) => {
            eprintln!("memex query: {e}");
            return Ok(1);
        }
    };

    let mut status_code = 1;
    for ev in &events {
        match ev {
            protocol::Event::Expansion { lex, vec, hyde } => {
                println!("Expansion:");
                if !lex.is_empty() {
                    println!("  lex: {lex}");
                }
                if !vec.is_empty() {
                    println!("  vec: {vec}");
                }
                if !hyde.is_empty() {
                    println!("  hyde: {hyde}");
                }
                println!();
            }
            protocol::Event::Answer { text, citations } => {
                println!("{text}");
                if !citations.is_empty() {
                    println!();
                    println!("Citations:");
                    for c in citations {
                        println!("  [[{c}]]");
                    }
                }
            }
            protocol::Event::Error {
                code,
                message,
                status,
            } => {
                eprintln!("memex query error ({code}): {message}");
                status_code = *status;
            }
            protocol::Event::Done { status } => {
                if *status == 0 {
                    status_code = 0;
                }
            }
            _ => {}
        }
    }
    Ok(status_code)
}
