//! CLI-side socket client.

use crate::daemon::protocol::{Event, Request};
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{Instant, sleep};

/// Try to connect to the daemon socket. Does not spawn. Returns Err if the
/// daemon isn't running.
pub async fn connect(socket: &Path) -> Result<UnixStream> {
    UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {socket:?}"))
}

/// Try to connect with retry, up to `deadline`. Useful when another process
/// is known to be spawning the daemon.
pub async fn connect_with_retry(socket: &Path, deadline: Instant) -> Result<UnixStream> {
    let mut attempt = 0;
    loop {
        match UnixStream::connect(socket).await {
            Ok(s) => return Ok(s),
            Err(_) if Instant::now() < deadline => {
                attempt += 1;
                sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                bail!(
                    "connect to {:?} failed after {} attempts: {e}",
                    socket,
                    attempt
                );
            }
        }
    }
}

/// Connect to the daemon socket; if the first attempt fails, try to acquire
/// `lock` — success means no daemon is running, so we spawn one; failure
/// means another client or daemon already holds it, so we skip the spawn
/// and just retry. This dedupes the spawn-storm when N parallel clients
/// all see a cold socket at once.
pub async fn connect_or_spawn(socket: &Path, lock: &Path, deadline: Instant) -> Result<UnixStream> {
    // Fast path.
    if let Ok(s) = UnixStream::connect(socket).await {
        return Ok(s);
    }
    // Try to claim the spawn lock. If busy, a daemon is either up (just
    // not listening yet) or spawning in another client; retry connect.
    match crate::daemon::lock::try_acquire(lock)? {
        crate::daemon::lock::TryAcquire::Busy => {
            return connect_with_retry(socket, deadline).await;
        }
        crate::daemon::lock::TryAcquire::Acquired(guard) => {
            // Release the flock before spawning so the child can acquire
            // it itself. There's still a narrow window where a sibling
            // client could squeeze in and spawn a second daemon, but the
            // extra daemon just exits cleanly via its own flock check.
            drop(guard);
            let exe = std::env::current_exe().context("getting current exe path")?;
            let mut child = std::process::Command::new(&exe)
                .args(["daemon", "start"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .with_context(|| format!("spawning {exe:?} daemon start"))?;
            // Wait for the daemon to fork and exit, so we don't leave zombies.
            // The daemon will detach itself via setsid() in start_background.
            let _ = child.wait();
        }
    }
    connect_with_retry(socket, deadline).await
}

/// Send a single request and read all response events until `done`.
pub async fn request(stream: UnixStream, req: &Request) -> Result<Vec<Event>> {
    let (read_half, mut write_half) = stream.into_split();
    let mut s = serde_json::to_string(req).context("serializing request")?;
    s.push('\n');
    write_half
        .write_all(s.as_bytes())
        .await
        .context("writing request")?;
    write_half.shutdown().await.ok();

    let mut reader = BufReader::new(read_half);
    let mut events = Vec::new();
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .context("reading response")?;
        if n == 0 {
            break;
        }
        let event: Event = serde_json::from_str(buf.trim_end())
            .with_context(|| format!("parsing event {:?}", buf.trim_end()))?;
        let done = matches!(event, Event::Done { .. });
        events.push(event);
        if done {
            break;
        }
    }
    Ok(events)
}
