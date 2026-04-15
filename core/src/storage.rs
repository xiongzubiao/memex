use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Atomic write: write to temp file, then rename to final path.
pub fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, content)?;
    fs::rename(&temp, path)?;
    Ok(())
}

/// Acquire exclusive file lock for write operations.
pub fn acquire_lock(lock_path: &Path) -> std::io::Result<fs::File> {
    use fs2::FileExt;
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    file.lock_exclusive()?;
    Ok(file)
}

/// Try to acquire lock with a timeout (in seconds).
pub fn try_acquire_lock(lock_path: &Path, timeout_secs: u64) -> std::io::Result<fs::File> {
    use fs2::FileExt;
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    if let Ok(()) = file.try_lock_exclusive() {
        return Ok(file);
    }
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    // TODO: this is a sync function called from async contexts; `std::thread::sleep`
    // blocks the tokio runtime thread. A proper fix would make this async and use
    // `tokio::time::sleep`, but that requires changing all callers. Using a short
    // sleep (10ms) reduces the blocking window in the meantime.
    while start.elapsed() < timeout {
        std::thread::sleep(std::time::Duration::from_millis(10));
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(_) => continue,
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("Could not acquire lock within {}s", timeout_secs),
    ))
}

/// Async-safe wrapper: acquires the lock in a blocking thread pool to avoid
/// stalling the tokio runtime during the polling sleep loop.
pub async fn try_acquire_lock_async(
    lock_path: &Path,
    timeout_secs: u64,
) -> std::io::Result<fs::File> {
    let path = lock_path.to_path_buf();
    tokio::task::spawn_blocking(move || try_acquire_lock(&path, timeout_secs))
        .await
        .map_err(std::io::Error::other)?
}

/// Release file lock.
pub fn release_lock(file: fs::File) {
    use fs2::FileExt;
    FileExt::unlock(&file).ok();
    drop(file);
}

/// Normalize a path by joining `base` with `relative_key` and resolving `..` and `.`
/// components lexically — no filesystem I/O, no directory creation.
///
/// Use this for path-traversal checks: after normalizing, verify the result still
/// starts with the expected base directory.
pub fn normalize_path(base: &Path, relative_key: &str) -> PathBuf {
    let mut result = base.to_path_buf();
    for component in Path::new(relative_key).components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::Normal(c) => {
                result.push(c);
            }
            _ => {}
        }
    }
    result
}

/// Compute SHA-256 hash of byte content. Returns hex string.
pub fn content_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// Compute SHA-256 hash of a file. Returns hex string.
pub fn file_hash(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(content_hash(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn atomic_write_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn atomic_write_no_temp_leftover() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        atomic_write(&path, b"hello").unwrap();
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn lock_acquire_and_release() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".lock");
        let file = acquire_lock(&lock_path).unwrap();
        assert!(lock_path.exists());
        release_lock(file);
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash(b"hello world");
        let h2 = content_hash(b"hello world");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn content_hash_different_inputs() {
        assert_ne!(content_hash(b"hello"), content_hash(b"world"));
    }

    #[test]
    fn file_hash_matches_content_hash() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, "hello").unwrap();
        assert_eq!(file_hash(&path).unwrap(), content_hash(b"hello"));
    }

    /// Verify that `try_acquire_lock_async` does not block the tokio runtime.
    ///
    /// If the blocking sleep ran on the tokio thread pool directly it would
    /// starve other tasks.  `spawn_blocking` moves the work to a dedicated
    /// thread, so the concurrent `async` task below must complete while the
    /// lock is being polled.
    #[tokio::test]
    async fn try_acquire_lock_async_does_not_block_runtime() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".async_lock");

        // Acquire the lock synchronously to force the async call to poll.
        use fs2::FileExt;
        let blocker = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        blocker.lock_exclusive().unwrap();

        // Spawn a task that completes immediately — proves the runtime is live.
        let side_task = tokio::spawn(async { 42u32 });

        // Start the async lock acquisition (will poll in a blocking thread).
        let lock_path_clone = lock_path.clone();
        let lock_future =
            tokio::spawn(async move { try_acquire_lock_async(&lock_path_clone, 5).await });

        // The side task must resolve without waiting for the lock.
        let side_result = side_task.await.unwrap();
        assert_eq!(side_result, 42, "runtime was blocked during lock polling");

        // Release the blocker so the lock future can succeed.
        blocker.unlock().unwrap();
        drop(blocker);

        let lock_result = lock_future.await.unwrap();
        assert!(lock_result.is_ok(), "lock should succeed after release");
        release_lock(lock_result.unwrap());
    }
}
