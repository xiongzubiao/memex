use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemexError {
    #[error("MEMEX_E007: Wiki changes failed validation: {details}")]
    ValidationFailure { details: String },

    #[error(
        "MEMEX_E009: Schema version {found} not supported (expected {expected}). Upgrade memex."
    )]
    SchemaVersionMismatch { found: u32, expected: u32 },

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("ONNX Runtime error: {0}")]
    Ort(#[from] ort::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),

    #[error("timed out waiting for writer lock after {timeout_secs}s (lock held by another process at {})", lock_path.display())]
    LockTimeout {
        timeout_secs: u64,
        lock_path: std::path::PathBuf,
    },

    #[error("cannot acquire writer lock at {}: {source}", lock_path.display())]
    LockAcquireIo {
        lock_path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("file operation failed after retries: {operation} on {}", path.display())]
    FileOpExhausted {
        path: std::path::PathBuf,
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("file operation failed: {operation} on {}", path.display())]
    FileOpFailed {
        path: std::path::PathBuf,
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed config file at {}: {reason}", path.display())]
    MalformedConfig {
        path: std::path::PathBuf,
        reason: String,
    },

    #[error("invalid value for {var} = {value:?}: {reason}")]
    InvalidEnvVar {
        var: &'static str,
        value: String,
        reason: String,
    },

    #[error("internal invariant violated: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, MemexError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_error() {
        let err = MemexError::SchemaVersionMismatch {
            found: 2,
            expected: 1,
        };
        let msg = format!("{err}");
        assert!(msg.contains("MEMEX_E009"));
        assert!(msg.contains("version 2"));
    }

    #[test]
    fn io_error_converts() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let memex_err: MemexError = io_err.into();
        assert!(format!("{memex_err}").contains("gone"));
    }
}

#[cfg(test)]
mod parallel_access_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn lock_timeout_display() {
        let err = MemexError::LockTimeout {
            timeout_secs: 120,
            lock_path: PathBuf::from("/tmp/.lock"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("120s"),
            "expected timeout in message, got: {msg}"
        );
        assert!(
            msg.contains("/tmp/.lock"),
            "expected path in message, got: {msg}"
        );
    }

    #[test]
    fn lock_acquire_io_display() {
        let err = MemexError::LockAcquireIo {
            lock_path: PathBuf::from("/tmp/.lock"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "perm"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("/tmp/.lock"));
    }

    #[test]
    fn file_op_exhausted_display() {
        let err = MemexError::FileOpExhausted {
            path: PathBuf::from("/tmp/foo"),
            operation: "rename",
            source: std::io::Error::new(std::io::ErrorKind::WouldBlock, "busy"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("rename"));
        assert!(msg.contains("/tmp/foo"));
    }

    #[test]
    fn file_op_failed_display() {
        let err = MemexError::FileOpFailed {
            path: PathBuf::from("/tmp/foo"),
            operation: "write temp",
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "gone"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("write temp"));
    }

    #[test]
    fn malformed_config_display() {
        let err = MemexError::MalformedConfig {
            path: PathBuf::from("/tmp/config.toml"),
            reason: "expected `=` at line 2".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("/tmp/config.toml"));
        assert!(msg.contains("expected `=`"));
    }

    #[test]
    fn invalid_env_var_display() {
        let err = MemexError::InvalidEnvVar {
            var: "MEMEX_LOCK_TIMEOUT_SECONDS",
            value: "abc".into(),
            reason: "not a non-negative integer".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("MEMEX_LOCK_TIMEOUT_SECONDS"));
        assert!(msg.contains("abc"));
        assert!(msg.contains("not a non-negative"));
    }
}
