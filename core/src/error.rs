use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemexError {
    #[error("MEMEX_E001: No config found at {path}. Run `memex init`.")]
    ConfigMissing { path: PathBuf },

    #[error("MEMEX_E002: No provider configured. Set an API key env var or run `memex init`.")]
    NoProviderConfigured,

    #[error("MEMEX_E003: Provider auth failed: {details}")]
    AuthFailure { details: String },

    #[error("MEMEX_E004: Memex locked by another process. If stale, remove {lock_path}")]
    StaleLock { lock_path: PathBuf },

    #[error("MEMEX_E005: LLM call timed out. Source stored, wiki unchanged.")]
    LlmTimeout,

    #[error("MEMEX_E012: LLM call failed: {details}")]
    LlmCallFailed { details: String },

    #[error("MEMEX_E006: LLM output couldn't be parsed into wiki pages. Retrying with guidance.")]
    LlmParseFailure,

    #[error("MEMEX_E007: Wiki changes failed validation: {details}")]
    ValidationFailure { details: String },

    #[error("MEMEX_E008: Could not detect format for {path}. Use --format to specify.")]
    FormatDetection { path: PathBuf },

    #[error(
        "MEMEX_E009: Schema version {found} not supported (expected {expected}). Upgrade memex."
    )]
    SchemaVersionMismatch { found: u32, expected: u32 },

    #[error("MEMEX_E010: index.md exceeds {tokens} tokens. Performance may degrade.")]
    IndexBudgetExceeded { tokens: usize },

    #[error("MEMEX_E011: Provider can't process {format}. Falling back to text extraction.")]
    MultimodalUnsupported { format: String },

    #[error("Could not load prior knowledge. Proceeding without context.")]
    ContextForFailure,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, MemexError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_includes_code() {
        let err = MemexError::ConfigMissing {
            path: PathBuf::from("/home/.memex/config.toml"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("MEMEX_E001"), "got: {msg}");
        assert!(msg.contains("memex init"), "got: {msg}");
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
