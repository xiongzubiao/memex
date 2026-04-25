//! PID file helpers. Used for observability (`memex daemon status`) and
//! `memex daemon stop` to find the daemon process. Not authoritative for
//! liveness — the flock at `${MEMEX_ROOT}/daemon.lock` is.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub fn write(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
    }
    fs::write(path, format!("{pid}\n")).with_context(|| format!("writing {path:?}"))?;
    Ok(())
}

pub fn read(path: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let trimmed = s.trim();
            trimmed
                .parse::<u32>()
                .map(Some)
                .with_context(|| format!("parsing pid in {path:?}: {trimmed:?}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {path:?}")),
    }
}

pub fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {path:?}")),
    }
}

/// Returns true iff a process with this pid is alive. On Unix this uses
/// `kill(pid, 0)` which returns 0 iff the pid exists and we can signal it,
/// ESRCH if it doesn't.
#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_alive(_pid: u32) -> bool {
    false // Unix-only; Windows is out of scope.
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_and_read_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.pid");
        write(&path, 12345).unwrap();
        assert_eq!(read(&path).unwrap(), Some(12345));
    }

    #[test]
    fn read_missing_returns_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.pid");
        assert_eq!(read(&path).unwrap(), None);
    }

    #[test]
    fn remove_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.pid");
        remove(&path).unwrap();
    }

    #[test]
    fn is_alive_returns_true_for_self_pid() {
        let pid = std::process::id();
        assert!(is_alive(pid));
    }

    #[test]
    fn is_alive_returns_false_for_impossible_pid() {
        // Use a PID that cannot exist: 2000000 is well beyond any reasonable
        // process limit on any modern Unix system.
        assert!(!is_alive(2000000));
    }
}
