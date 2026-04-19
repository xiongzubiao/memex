//! Lock 2 — daemon lifetime ownership flock.
//!
//! The daemon calls `try_acquire` on startup. `Acquired` means this process
//! is THE daemon and must hold the lock until exit. Dropping the guard
//! releases the lock; a crash releases it via OS cleanup.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub enum TryAcquire {
    Acquired(FlockGuard),
    Busy,
}

pub struct FlockGuard {
    #[allow(dead_code)] // held for lifetime; OS releases on drop/crash
    file: File,
    path: PathBuf,
}

impl FlockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn try_acquire(path: &Path) -> Result<TryAcquire> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {path:?}"))?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(TryAcquire::Acquired(FlockGuard {
            file,
            path: path.to_path_buf(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(TryAcquire::Busy),
        Err(e) => Err(e).with_context(|| format!("flock on {path:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquires_when_unlocked() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.lock");
        match try_acquire(&path).unwrap() {
            TryAcquire::Acquired(guard) => assert_eq!(guard.path(), &path),
            TryAcquire::Busy => panic!("expected Acquired, got Busy"),
        }
    }

    #[test]
    fn second_attempt_is_busy_while_first_held() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.lock");
        let _first = match try_acquire(&path).unwrap() {
            TryAcquire::Acquired(g) => g,
            TryAcquire::Busy => panic!("first acquire should succeed"),
        };
        // second attempt in the same process on the same path: busy.
        match try_acquire(&path).unwrap() {
            TryAcquire::Busy => {}
            TryAcquire::Acquired(_) => panic!("expected Busy"),
        }
    }

    #[test]
    fn drop_releases_lock() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.lock");
        {
            let _g = match try_acquire(&path).unwrap() {
                TryAcquire::Acquired(g) => g,
                TryAcquire::Busy => panic!(),
            };
        } // drop
        // can re-acquire.
        match try_acquire(&path).unwrap() {
            TryAcquire::Acquired(_) => {}
            TryAcquire::Busy => panic!("expected re-acquire after drop"),
        }
    }
}
