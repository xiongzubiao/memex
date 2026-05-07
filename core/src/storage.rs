use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const RETRY_SCHEDULE: &[Duration] = &[
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(500),
    Duration::from_millis(500),
    Duration::from_millis(500),
];
// Total: ~1.9s across 9 attempts (1 immediate + 8 backoffs).

/// Classify an I/O error as transient (retry-worthy) or permanent (fail-fast).
/// Biased toward transient on unknowns — wait 1.9s on a weird new error code
/// rather than falsely fail-fast on a genuinely transient one.
pub(crate) fn is_transient_io_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;

    // Definitely permanent — fail fast.
    if matches!(
        err.kind(),
        NotFound | InvalidInput | InvalidData | UnexpectedEof
    ) {
        return false;
    }

    // Definitely transient by kind.
    if matches!(
        err.kind(),
        WouldBlock | Interrupted | ResourceBusy | TimedOut
    ) {
        return true;
    }

    // Check raw OS error codes.
    if let Some(code) = err.raw_os_error() {
        #[cfg(unix)]
        {
            let transient_posix = [
                libc::EBUSY,
                libc::EAGAIN,
                libc::EINTR,
                libc::ETXTBSY,
                libc::ESTALE,
            ];
            if transient_posix.contains(&code) {
                return true;
            }
        }
        #[cfg(windows)]
        {
            const ERROR_SHARING_VIOLATION: i32 = 32;
            const ERROR_LOCK_VIOLATION: i32 = 33;
            const ERROR_ACCESS_DENIED: i32 = 5;
            if [
                ERROR_SHARING_VIOLATION,
                ERROR_LOCK_VIOLATION,
                ERROR_ACCESS_DENIED,
            ]
            .contains(&code)
            {
                return true;
            }
        }
    }

    // POSIX PermissionDenied: permanent (RO FS, RO parent dir, SELinux).
    // Transient EACCES cases hit the raw_os_error check above.
    #[cfg(unix)]
    if matches!(err.kind(), std::io::ErrorKind::PermissionDenied) {
        return false;
    }

    // Unknown — bias toward transient (wait 1.9s, then surface).
    true
}

// Test-only injection hook for `retry_io`: when set, `retry_io` returns
// the injected error instead of calling `f`. Used by unit tests to
// exercise the retry loop deterministically without filesystem tricks.
#[cfg(test)]
thread_local! {
    static TEST_FAILURE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn inject_atomic_write_failures(n: usize) {
    TEST_FAILURE_COUNT.with(|c| c.set(n));
}

#[cfg(test)]
fn test_injected_failure() -> Option<std::io::Error> {
    TEST_FAILURE_COUNT.with(|c| {
        let remaining = c.get();
        if remaining == 0 {
            return None;
        }
        c.set(remaining - 1);
        Some(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "test-injected",
        ))
    })
}

/// Execute `f` with bounded retry on transient I/O errors (`RETRY_SCHEDULE`,
/// ~1.9s across 9 attempts). Permanent errors fail fast as `FileOpFailed`;
/// exhausted retries surface as `FileOpExhausted`. See `is_transient_io_error`
/// for the permanent/transient split.
pub fn retry_io<F>(path: &Path, operation: &'static str, mut f: F) -> crate::error::Result<()>
where
    F: FnMut() -> std::io::Result<()>,
{
    let mut last_err: Option<std::io::Error> = None;
    for (attempt, delay) in std::iter::once(&Duration::ZERO)
        .chain(RETRY_SCHEDULE)
        .enumerate()
    {
        if attempt > 0 {
            std::thread::sleep(*delay);
        }
        #[cfg(test)]
        if let Some(e) = test_injected_failure() {
            last_err = Some(e);
            continue;
        }
        match f() {
            Ok(()) => return Ok(()),
            Err(e) => {
                if !is_transient_io_error(&e) {
                    return Err(crate::error::MemexError::FileOpFailed {
                        path: path.to_path_buf(),
                        operation,
                        source: e,
                    });
                }
                last_err = Some(e);
            }
        }
    }
    Err(crate::error::MemexError::FileOpExhausted {
        path: path.to_path_buf(),
        operation,
        source: last_err.expect("loop ran at least once"),
    })
}

/// 4 bytes of hex random — plenty to disambiguate concurrent writes by the
/// same PID across process lifetimes. `rand::random::<u32>()` uses the OS RNG.
fn random_nonce_hex() -> String {
    let n: u32 = rand::random();
    format!("{n:08x}")
}

/// Atomic write with retry + fsync durability.
///
/// Sequence: write tmp, fsync(tmp), close, rename, fsync(parent_dir).
/// Step 3's tmp fsync guarantees content durability after a crash; step
/// 6's parent fsync guarantees the rename's directory entry survives.
/// Skipping either leaves a window where the file's content or its
/// visibility is undefined after power loss.
///
/// Writes to `.{filename}.{nonce}.tmp` (e.g., `.rest-patterns.md.a1b2c3d4.tmp`)
/// then renames over the target path. Write and rename are retried with
/// a bounded schedule (`retry_io`) when the I/O error is transient.
/// Permanent errors (`NotFound`, `InvalidInput`, etc.) fail fast through
/// `MemexError::FileOpFailed`.
pub fn atomic_write(path: &Path, content: &[u8]) -> crate::error::Result<()> {
    let nonce = random_nonce_hex();
    // Format: `.{filename}.{nonce}.tmp`. The nonce is 8 hex chars = 32
    // bits — birthday collision is statistically irrelevant given the
    // writer-lock serializes all atomic_write calls per memex root.
    // Earlier versions also included {pid}, but that added up to 11
    // bytes to the temp name (eating into NAME_MAX) without buying
    // anything the lock didn't already give us.
    let temp_name = format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        nonce,
    );
    let temp = path.with_file_name(temp_name);

    // Write + fsync the tmp file: contents durable on disk.
    if let Err(e) = retry_io(&temp, "write temp", || {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp)?;
        std::io::Write::write_all(&mut f, content)?;
        f.sync_all()?;
        Ok(())
    }) {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }

    if let Err(e) = retry_io(path, "rename", || fs::rename(&temp, path)) {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }

    // fsync the parent dir: rename durable in the directory entry.
    // Required, not best-effort. A failure here means the rename's
    // directory entry isn't guaranteed to survive a crash.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let dir = fs::File::open(parent).map_err(|e| crate::error::MemexError::FileOpFailed {
            path: parent.to_path_buf(),
            operation: "atomic_write: open parent dir for fsync",
            source: e,
        })?;
        dir.sync_all().map_err(|e| crate::error::MemexError::FileOpFailed {
            path: parent.to_path_buf(),
            operation: "atomic_write: fsync parent dir",
            source: e,
        })?;
    }

    Ok(())
}

/// Forward-slash-normalized lossy path string. Memex stores all paths in
/// the DB with `/` separators regardless of host OS so wiki/raw lookups
/// work the same on Windows and Unix.
pub fn rel_path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Split a `---\n<yaml>\n---\n<body>` frontmatter block into its YAML and
/// body slices. Schema-agnostic. Returns `None` when the input lacks a
/// well-formed leading frontmatter fence pair — callers that want body-only
/// use `split_frontmatter(c).map_or(c, |(_, b)| b)`.
///
/// Single source of truth for body bytes: hash, FTS body, snippet positions,
/// and lint's stale-index check all flow from this function so they agree
/// byte-for-byte.
pub fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim_start_matches('\u{feff}');
    let rest = trimmed.strip_prefix("---\n")?;
    let close = rest.find("\n---")?;
    let yaml = &rest[..close];
    let after = &rest[close + 4..];
    let body = after
        .strip_prefix("\n\n")
        .or_else(|| after.strip_prefix('\n'))
        .unwrap_or(after);
    Some((yaml, body))
}

/// Convert a `SystemTime` to nanoseconds since the Unix epoch for SQLite storage.
pub fn systime_to_nanos(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as i64
}

/// Reconstruct a `SystemTime` from nanoseconds since the Unix epoch (SQLite read).
pub fn nanos_to_systime(nanos: i64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_nanos(nanos as u64)
}

/// Try to acquire exclusive flock on `lock_path`, polling every 10ms until
/// `timeout` elapses. Contention errors (WouldBlock) cause continued polling;
/// any other I/O error returns immediately so the caller can distinguish
/// `LockTimeout` (retry-worthy) from `LockAcquireIo` (configuration problem).
pub(crate) fn try_acquire_lock(
    lock_path: &Path,
    timeout: std::time::Duration,
) -> std::io::Result<fs::File> {
    use fs2::FileExt;
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;

    let is_contention = |e: &std::io::Error| -> bool {
        if matches!(e.kind(), std::io::ErrorKind::WouldBlock) {
            return true;
        }
        #[cfg(windows)]
        if e.raw_os_error() == Some(33) {
            return true;
        } // ERROR_LOCK_VIOLATION
        false
    };

    // First attempt — uncontended fast path.
    match file.try_lock_exclusive() {
        Ok(()) => return Ok(file),
        Err(e) if !is_contention(&e) => return Err(e),
        Err(_) => {}
    }

    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(std::time::Duration::from_millis(10));
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if !is_contention(&e) => return Err(e),
            Err(_) => continue,
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("could not acquire lock within {:?}", timeout),
    ))
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

const STALE_TMP_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Strict match for memex-generated tmp names.
///
/// Format: `.{filename}.{nonce}.tmp` where filename = `{stem}.md` and
/// nonce is 8 ASCII hex chars.
/// Example: `.rest-patterns.md.a1b2c3d4.tmp`.
pub(crate) fn is_memex_tmp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.').and_then(|s| s.strip_suffix(".tmp")) else {
        return false;
    };
    let mut parts = rest.rsplitn(2, '.');
    let (Some(nonce), Some(basename)) = (parts.next(), parts.next()) else {
        return false;
    };

    if nonce.len() != 8 || !nonce.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    let Some(stem) = basename.strip_suffix(".md") else {
        return false;
    };
    if stem.is_empty() || stem.contains('.') {
        return false;
    }
    true
}

/// Remove memex tmp files older than STALE_TMP_AGE in `wiki_dir` (recursive).
/// Safe to call unconditionally — mismatches (non-memex names) are ignored
/// by `is_memex_tmp_name`. Must be called under the writer flock so fresh
/// tmp files from a concurrent writer aren't misidentified.
pub(crate) fn cleanup_stale_tmp_files(wiki_dir: &Path) {
    let now = std::time::SystemTime::now();
    for entry in walkdir::WalkDir::new(wiki_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !is_memex_tmp_name(&name) {
            continue;
        }

        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        let Ok(age) = now.duration_since(mtime) else {
            continue;
        };
        if age >= STALE_TMP_AGE {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// `validate::parse_frontmatter`'s body must be byte-identical to
    /// `split_frontmatter`'s. If they ever diverge, lint will compute a
    /// different hash than `commit_doc` did at write time and report a
    /// false `stale-index`. Pinning the invariant here keeps a future
    /// edit to either function from silently regressing.
    #[test]
    fn validate_and_split_extract_identical_body() {
        const FM: &str =
            "title: T
sources: []\ncreated_at: 2026-04-06T00:00:00Z\nupdated_at: 2026-04-06T00:00:00Z";
        let bodies = [
            "body content\n",                   // canonical
            "body content",                     // no trailing newline
            "body content\n\n\n",               // trailing blank lines
            "  body with leading spaces\n",     // body whitespace preserved
            "body",                             // minimal
        ];
        for body in bodies {
            let content = format!("---\n{FM}\n---\n\n{body}");
            let split_body = split_frontmatter(&content).map(|(_, b)| b).unwrap();
            let (_, validate_body) = crate::validate::parse_frontmatter(&content).unwrap();
            assert_eq!(
                split_body, validate_body,
                "extractor mismatch for body {body:?}: split={split_body:?} validate={validate_body:?}"
            );
            assert_eq!(split_body, body, "split should return the exact body bytes");
        }
    }

    #[test]
    fn atomic_write_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
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
    fn atomic_write_leaves_no_tmp_files_on_success() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("hello.md");
        atomic_write(&target, b"x").unwrap();
        let entries: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            entries,
            vec!["hello.md".to_string()],
            "no .tmp files should remain after a successful write"
        );
    }

    #[test]
    fn atomic_write_in_nested_dir() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let target = nested.join("page.md");
        atomic_write(&target, b"deep").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"deep");
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

    #[test]
    fn try_acquire_lock_returns_timeout_on_contention() {
        use fs2::FileExt;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(".lock");
        let blocker = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        blocker.lock_exclusive().unwrap();

        let result = try_acquire_lock(&lock_path, std::time::Duration::from_millis(100));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        FileExt::unlock(&blocker).unwrap();
    }

    #[test]
    fn try_acquire_lock_returns_non_timeout_io_immediately() {
        // Point at a path whose parent doesn't exist — OpenOptions.open() fails
        // with NotFound, which should be returned immediately (not after timeout).
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("nonexistent_subdir").join(".lock");

        let start = std::time::Instant::now();
        let result = try_acquire_lock(&lock_path, std::time::Duration::from_secs(5));
        let elapsed = start.elapsed();

        let err = result.unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "expected NotFound from missing parent dir, got {:?}",
            err.kind()
        );
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "non-timeout error should return immediately, took {:?}",
            elapsed
        );
    }

    #[test]
    fn classifier_not_found_is_permanent() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        assert!(!is_transient_io_error(&e));
    }

    #[test]
    fn classifier_invalid_input_is_permanent() {
        let e = std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad");
        assert!(!is_transient_io_error(&e));
    }

    #[test]
    fn classifier_would_block_is_transient() {
        let e = std::io::Error::new(std::io::ErrorKind::WouldBlock, "busy");
        assert!(is_transient_io_error(&e));
    }

    #[cfg(unix)]
    #[test]
    fn classifier_posix_permission_denied_is_permanent() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "perm");
        assert!(!is_transient_io_error(&e));
    }

    #[cfg(unix)]
    #[test]
    fn classifier_posix_ebusy_is_transient() {
        let e = std::io::Error::from_raw_os_error(libc::EBUSY);
        assert!(is_transient_io_error(&e));
    }

    #[test]
    fn retry_io_succeeds_first_try() {
        let temp = std::path::PathBuf::from("/tmp/unused");
        let result = retry_io(&temp, "noop", || Ok(()));
        assert!(result.is_ok());
    }

    #[test]
    fn retry_io_fast_fails_on_permanent_error() {
        let temp = std::path::PathBuf::from("/tmp/unused");
        let start = std::time::Instant::now();
        let result = retry_io(&temp, "noop", || {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))
        });
        assert!(matches!(
            result,
            Err(crate::error::MemexError::FileOpFailed { .. })
        ));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "fast-fail should not wait the retry budget"
        );
    }

    #[test]
    fn retry_io_exhausts_on_persistent_transient() {
        let temp = std::path::PathBuf::from("/tmp/unused");
        let result = retry_io(&temp, "noop", || {
            Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "busy"))
        });
        assert!(matches!(
            result,
            Err(crate::error::MemexError::FileOpExhausted { .. })
        ));
    }

    #[test]
    fn atomic_write_retries_on_injected_transient() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foo.md");
        inject_atomic_write_failures(3);
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn atomic_write_tmp_filename_uses_nonce_format() {
        // Write a file, verify the happy path produces the final file correctly
        // and no stale tmp file is left behind.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("foo.md");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");

        let stale: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(stale.is_empty(), "tmp file leaked: {:?}", stale);
    }

    #[test]
    fn is_memex_tmp_name_matches_valid() {
        assert!(is_memex_tmp_name(".rest-patterns.md.a1b2c3d4.tmp"));
        assert!(is_memex_tmp_name(".foo.md.00000000.tmp"));
    }

    #[test]
    fn is_memex_tmp_name_rejects_invalid() {
        assert!(!is_memex_tmp_name(".DS_Store"));
        assert!(!is_memex_tmp_name(".swp"));
        assert!(!is_memex_tmp_name("rest-patterns.md"));
        assert!(
            !is_memex_tmp_name(".rest-patterns.md.tmp"),
            "missing nonce"
        );
        assert!(
            !is_memex_tmp_name(".rest.patterns.md.a1b2c3d4.tmp"),
            "stem must not contain dots"
        );
        assert!(
            !is_memex_tmp_name(".rest-patterns.txt.a1b2c3d4.tmp"),
            "must end in .md"
        );
        assert!(
            !is_memex_tmp_name(".rest-patterns.md.xxxxxxxx.tmp"),
            "nonce must be hex"
        );
        assert!(
            !is_memex_tmp_name(".rest-patterns.md.a1b2c3d.tmp"),
            "nonce must be 8 chars"
        );
    }

    #[test]
    fn cleanup_removes_stale_tmp_files() {
        use std::fs::OpenOptions;
        let dir = TempDir::new().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();

        let stale_path = wiki.join(".foo.md.deadbeef.tmp");
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&stale_path)
            .unwrap();

        // Backdate mtime to 2 hours ago.
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        filetime::set_file_mtime(
            &stale_path,
            filetime::FileTime::from_system_time(two_hours_ago),
        )
        .unwrap();

        cleanup_stale_tmp_files(&wiki);

        assert!(!stale_path.exists(), "stale tmp should have been removed");
    }

    #[test]
    fn cleanup_preserves_fresh_tmp_files() {
        use std::fs::OpenOptions;
        let dir = TempDir::new().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();

        let fresh_path = wiki.join(".foo.md.deadbeef.tmp");
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&fresh_path)
            .unwrap();
        // mtime = now (default)

        cleanup_stale_tmp_files(&wiki);
        assert!(
            fresh_path.exists(),
            "fresh tmp should NOT have been removed"
        );
    }

    #[test]
    fn cleanup_ignores_non_memex_dotfiles() {
        let dir = TempDir::new().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();

        let ds_store = wiki.join(".DS_Store");
        let swp = wiki.join(".foo.swp");
        std::fs::write(&ds_store, "").unwrap();
        std::fs::write(&swp, "").unwrap();

        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        filetime::set_file_mtime(
            &ds_store,
            filetime::FileTime::from_system_time(two_hours_ago),
        )
        .unwrap();
        filetime::set_file_mtime(&swp, filetime::FileTime::from_system_time(two_hours_ago))
            .unwrap();

        cleanup_stale_tmp_files(&wiki);
        assert!(ds_store.exists());
        assert!(swp.exists());
    }
}
