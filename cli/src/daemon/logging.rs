//! Tracing subscriber setup for the daemon. File-rotating JSON logs.

use anyhow::Result;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Initialize the global tracing subscriber. The returned guard must stay
/// alive for the process's lifetime; dropping it flushes pending writes.
pub fn init(log_file: &Path) -> Result<WorkerGuard> {
    let parent = log_file.parent().unwrap_or(Path::new("."));
    let filename = log_file
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("daemon.log");
    std::fs::create_dir_all(parent)?;

    let appender = rolling::daily(parent, filename);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let env_filter =
        EnvFilter::try_from_env("MEMEX_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().json().with_writer(non_blocking))
        .init();

    // Route panics through tracing before unwinding. `daemonize_child`
    // dups stderr to /dev/null, so the default panic-to-stderr is lost.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            panic = %info,
            backtrace = %bt,
            "daemon panicked",
        );
        prev(info);
    }));

    Ok(guard)
}
