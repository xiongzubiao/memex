use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemexError {
    #[error("MEMEX_E004: Memex locked by another process. If stale, remove {lock_path}")]
    StaleLock { lock_path: PathBuf },

    #[error("MEMEX_E007: Wiki changes failed validation: {details}")]
    ValidationFailure { details: String },

    #[error(
        "MEMEX_E009: Schema version {found} not supported (expected {expected}). Upgrade memex."
    )]
    SchemaVersionMismatch { found: u32, expected: u32 },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("ONNX Runtime error: {0}")]
    Ort(#[from] ort::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, MemexError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_includes_code() {
        let err = MemexError::StaleLock {
            lock_path: PathBuf::from("/home/.memex/.lock"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("MEMEX_E004"), "got: {msg}");
    }

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
