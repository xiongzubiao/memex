//! Memex daemon subsystem.
//!
//! IPC plumbing (socket + flock + config + ping), retrieval, agent workers,
//! query expansion, and session ingestion.

pub mod client;
pub mod config;
pub mod context;
pub mod error;
pub mod fs_kind;
pub mod handler;
pub mod lock;
pub mod logging;
pub mod memex_handle;
pub mod pidfile;
pub mod plan;
pub mod protocol;
pub mod queue;
pub mod retrieval;
pub mod server;
pub mod watcher;
pub mod worker;

use crate::memex_root;

/// Daemonize the child process.
///
/// Standard daemon practice: close inherited file descriptors, then redirect
/// stdin/stdout/stderr to /dev/null.
#[cfg(unix)]
fn daemonize_child() -> Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    if rc != 0 {
        anyhow::bail!(
            "getrlimit(RLIMIT_NOFILE) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    let max_fd = limit.rlim_cur.min(i32::MAX as libc::rlim_t) as i32;
    for fd in 0..max_fd {
        unsafe {
            libc::close(fd);
        }
    }

    let devnull_fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
    if devnull_fd < 0 {
        anyhow::bail!(
            "open(/dev/null) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    for fd in 0..=2 {
        let rc = unsafe { libc::dup2(devnull_fd, fd) };
        if rc < 0 {
            anyhow::bail!(
                "dup2(/dev/null, {fd}) failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    if devnull_fd > 2 {
        unsafe {
            libc::close(devnull_fd);
        }
    }

    Ok(())
}
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

/// `memex daemon start` entrypoint.
///
/// Self-daemonizes via `fork() → parent exits → child setsid()` at the very
/// top. After this, the daemon is in its own session, detached from the
/// spawning CLI's process group and controlling terminal: it survives
/// Ctrl+C on the CLI, terminal close (SIGHUP), and is adopted by init
/// rather than held as a child of the CLI that spawned it.
///
/// If `foreground` is true, skip forking and run in terminal (for debugging).
pub fn start_background(foreground: bool) -> Result<i32> {
    if !foreground {
        // Pre-fork check: if a daemon is already responsive, the spawned
        // child would otherwise fail to acquire the daemon flock and
        // exit cleanly without telling the user anything. Print here so
        // `memex daemon start` always confirms what happened.
        let paths = DaemonPaths::default_under(&memex_root());
        if let Some(pid) = pidfile::read(&paths.pid)?
            && pidfile::is_alive(pid)
            && socket_responsive(&paths.socket)
        {
            println!("daemon: already running (pid {pid})");
            return Ok(0);
        }
        // SAFETY: must happen before any tokio runtime or thread setup — forking
        // a multithreaded process with async runtimes/mutexes is undefined.
        unsafe {
            let pid = libc::fork();
            if pid < 0 {
                anyhow::bail!("fork failed: {}", std::io::Error::last_os_error());
            }
            if pid > 0 {
                // Parent: return to caller.
                // The detached child continues below.
                return Ok(0);
            }
            // Child: daemonize (close FDs, redirect stdin/stdout/stderr).
            daemonize_child()?;
            // Become session leader, detaching from controlling tty
            // and the CLI's process group.
            if libc::setsid() < 0 {
                anyhow::bail!("setsid failed: {}", std::io::Error::last_os_error());
            }
        }
    }

    let paths = DaemonPaths::default_under(&memex_root());
    let cfg = config::Config::load(&default_config_path()).context("loading config")?;
    let _guard = logging::init(&cfg.daemon.log_file)?;
    tracing::info!("memex daemon starting (detached)");

    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime")?;
    let outcome = rt.block_on(server::run_daemon(paths, cfg))?;
    match outcome {
        server::StartOutcome::RanAsDaemon => Ok(0),
        server::StartOutcome::AlreadyRunning => Ok(0),
    }
}

/// Ensure the daemon is running before issuing a batch of concurrent
/// requests. Idempotent. Single call means only one spawn attempt, which
/// avoids the N-racing-spawns pattern that produces many brief zombies
/// under the CLI.
pub async fn warm_up() -> Result<()> {
    let paths = DaemonPaths::default_under(&memex_root());
    let _ = client::connect_or_spawn(
        &paths.socket,
        &paths.lock,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    Ok(())
}

/// Probe whether a Unix socket is accepting connections within 500ms.
/// Used by both `start_background` (to detect a live daemon before
/// forking) and `stop` (to confirm a pid file's daemon is actually
/// ours before sending SIGTERM).
fn socket_responsive(socket_path: &std::path::Path) -> bool {
    let Ok(rt) = tokio::runtime::Runtime::new() else {
        return false;
    };
    rt.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::UnixStream::connect(socket_path),
        )
        .await
        .is_ok_and(|r| r.is_ok())
    })
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
    if !socket_responsive(&paths.socket) {
        println!(
            "daemon: pid {pid} exists but socket is not responsive; refusing to signal (possible PID reuse)"
        );
        pidfile::remove(&paths.pid)?;
        let _ = std::fs::remove_file(&paths.socket);
        return Ok(0);
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    // Wait up to ~6 s — covers the daemon's 3 s drain plus tokio
    // runtime teardown overhead.
    for _ in 0..60 {
        if !pidfile::is_alive(pid) {
            println!("daemon: stopped (pid {pid})");
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("daemon: still running after 6s SIGTERM; giving up");
    Ok(1)
}

/// Quiet daemon health probe for `memex doctor`. Returns `Ok(true)` if a
/// daemon process is alive AND responds to ping within `timeout`. Returns
/// `Ok(false)` if no daemon is running. Returns `Err` if the pid file is
/// stale or the daemon is alive but unresponsive.
pub fn ping_quiet(timeout: Duration) -> Result<bool> {
    let paths = DaemonPaths::default_under(&memex_root());
    let pid = pidfile::read(&paths.pid)?;
    match pid {
        None => Ok(false),
        Some(p) if !pidfile::is_alive(p) => {
            anyhow::bail!("stale pid file (pid {p} not running)")
        }
        Some(p) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let stream =
                    client::connect_with_retry(&paths.socket, Instant::now() + timeout).await?;
                let events = client::request(stream, &protocol::Request::Ping {}).await?;
                if events
                    .iter()
                    .any(|ev| matches!(ev, protocol::Event::Pong { .. }))
                {
                    Ok(true)
                } else {
                    anyhow::bail!("daemon pid {p} responded but did not pong")
                }
            })
        }
    }
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
                let events = client::request(stream, &protocol::Request::Ping {}).await?;
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

/// Async ingest for OpenCode sessions. Reads the session from the user's
/// opencode SQLite DB on the CLI side, ships the canonical envelope to the
/// daemon via TranscriptInline. Mirrors `ingest_async` but session-id-keyed.
pub async fn ingest_opencode_async(
    session_id: &str,
    db_path: &std::path::Path,
    collections: Vec<String>,
) -> Result<i32> {
    let envelope = memex_core::transcript::extract_opencode_session(db_path, session_id)
        .map_err(|e| anyhow::anyhow!("read OpenCode session {session_id}: {e}"))?;
    let paths = DaemonPaths::default_under(&memex_root());
    let stream = client::connect_or_spawn(
        &paths.socket,
        &paths.lock,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let events = client::request(
        stream,
        &protocol::Request::Ingest {
            source: protocol::IngestSource::TranscriptInline {
                content: envelope,
                agent: protocol::TranscriptAgent::OpenCode,
                source_label: format!("opencode://session/{session_id}"),
            },
            collections,
        },
    )
    .await?;

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

/// Async core of the ingest client. Callable concurrently from one runtime.
/// Returns exit code (0 = queued, 1 = error/skipped).
pub async fn ingest_async(
    transcript_path: &str,
    agent: &str,
    collections: Vec<String>,
) -> Result<i32> {
    let paths = DaemonPaths::default_under(&memex_root());
    let stream = client::connect_or_spawn(
        &paths.socket,
        &paths.lock,
        Instant::now() + Duration::from_secs(5),
    )
    .await?;
    let agent_enum = match agent {
        "claude-code" => protocol::TranscriptAgent::ClaudeCode,
        "codex" => protocol::TranscriptAgent::Codex,
        "gemini-cli" => protocol::TranscriptAgent::GeminiCli,
        "openclaw" => protocol::TranscriptAgent::OpenClaw,
        "hermes" => protocol::TranscriptAgent::Hermes,
        "opencode" => protocol::TranscriptAgent::OpenCode,
        other => anyhow::bail!("unknown agent: {other}"),
    };
    let events = client::request(
        stream,
        &protocol::Request::Ingest {
            source: protocol::IngestSource::Transcript {
                path: transcript_path.to_string(),
                agent: agent_enum,
            },
            collections,
        },
    )
    .await?;

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

/// `memex ingest` entrypoint. Sync wrapper spinning up a fresh runtime for
/// single-call hook use. Returns exit code (0 = queued, 1 = error/skipped).
pub fn ingest(
    transcript_path: &str,
    agent: &str,
    collections: Vec<String>,
) -> Result<i32> {
    let rt = tokio::runtime::Runtime::new()?;
    match rt.block_on(ingest_async(transcript_path, agent, collections)) {
        Ok(code) => Ok(code),
        Err(e) => {
            eprintln!("memex ingest: {e}");
            Ok(1)
        }
    }
}

/// Print an actionable hint for `retrieval_empty` instead of the raw error
/// message. Both `query_raw` and `query_synth` route through this so users
/// see the same guidance whether they pass `--raw` or not.
fn print_retrieval_empty_hint(message: &str) {
    eprintln!("memex query: {message}");
    eprintln!();
    eprintln!("To populate your wiki, try one of:");
    eprintln!("  • `memex backfill claude-code`  — import existing Claude Code sessions");
    eprintln!("  • `memex backfill codex`        — import existing Codex sessions");
    eprintln!(
        "  • Open a session with the marketplace plugin installed; SessionEnd ingests automatically"
    );
    eprintln!("  • `memex write <slug>` then paste content via stdin to add a page manually");
}

/// `memex query <question> --raw` entrypoint. Connects to the running daemon
/// (does NOT auto-spawn — Task 9 adds that). Prints the context entries to stdout
/// in a reader-friendly format. Returns the exit code.
pub fn query_raw(
    question: &str,
    top_k: usize,
    collections: Vec<String>,
    intent: Option<String>,
    memex_root_override: Option<&std::path::Path>,
) -> Result<i32> {
    let paths = DaemonPaths::default_under(&memex_root());
    let _root = memex_root_override
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
                question: question.to_string(),
                raw: true,
                top_k,
                collections,
                intent,
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
            protocol::Event::Context { entries } => {
                // Emit the same JSON-array shape that the synthesis path
                // feeds to the LLM (`cli/src/daemon/context.rs`). Unifies
                // --raw stdout with the synth context block so scripts and
                // prompts can parse the output as JSON without ad-hoc
                // splitting on markdown headers.
                match serde_json::to_string_pretty(entries) {
                    Ok(s) => println!("{s}"),
                    Err(e) => {
                        eprintln!("memex query: failed to serialize context: {e}");
                        status_code = 1;
                    }
                }
            }
            protocol::Event::Error {
                code,
                message,
                status,
            } => {
                if code == "retrieval_empty" {
                    print_retrieval_empty_hint(message);
                } else {
                    eprintln!("memex query error ({code}): {message}");
                }
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
    collections: Vec<String>,
    intent: Option<String>,
    memex_root_override: Option<&std::path::Path>,
) -> Result<i32> {
    let paths = server::DaemonPaths::default_under(&memex_root());
    let _root = memex_root_override
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
                question: question.to_string(),
                raw: false,
                top_k,
                collections,
                intent,
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
                if code == "retrieval_empty" {
                    print_retrieval_empty_hint(message);
                } else {
                    eprintln!("memex query error ({code}): {message}");
                }
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
