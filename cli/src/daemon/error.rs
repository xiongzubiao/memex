//! Daemon error type + exit codes.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    BadRequest(String),
    VersionMismatch { supported: Vec<u32> },
    NotImplemented(String),
    RetrievalEmpty,
    Internal(String),
    SubprocessTimeout,
    /// Subprocess crashed. Carries the diagnostic (combined context +
    /// drained stderr) so the user sees why, not just a generic message.
    SubprocessCrashed(String),
    BackendUnavailable(String),
}

impl DaemonError {
    pub fn code_str(&self) -> &'static str {
        match self {
            DaemonError::BadRequest(_) => "bad_request",
            DaemonError::VersionMismatch { .. } => "version_mismatch",
            DaemonError::NotImplemented(_) => "not_implemented",
            DaemonError::RetrievalEmpty => "retrieval_empty",
            DaemonError::Internal(_) => "internal",
            DaemonError::SubprocessTimeout => "subprocess_timeout",
            DaemonError::SubprocessCrashed(_) => "subprocess_crashed",
            DaemonError::BackendUnavailable(_) => "backend_unavailable",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            DaemonError::SubprocessTimeout | DaemonError::SubprocessCrashed(_) => 4,
            _ => 1,
        }
    }

    pub fn message(&self) -> String {
        match self {
            DaemonError::BadRequest(msg) => msg.clone(),
            DaemonError::VersionMismatch { supported } => {
                format!("protocol version not supported (supported: {supported:?})")
            }
            DaemonError::NotImplemented(op) => format!("op '{op}' not implemented in this build"),
            DaemonError::RetrievalEmpty => "no indexed content for MEMEX_ROOT".to_string(),
            DaemonError::Internal(msg) => msg.clone(),
            DaemonError::SubprocessTimeout => "backend subprocess timed out".to_string(),
            DaemonError::SubprocessCrashed(reason) if reason.is_empty() => {
                "backend subprocess crashed".to_string()
            }
            DaemonError::SubprocessCrashed(reason) => {
                format!("backend subprocess crashed: {reason}")
            }
            DaemonError::BackendUnavailable(reason) => {
                format!("configured backend provider not available: {reason}")
            }
        }
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code_str(), self.message())
    }
}

impl std::error::Error for DaemonError {}

impl From<crate::daemon::queue::WorkerError> for DaemonError {
    fn from(e: crate::daemon::queue::WorkerError) -> Self {
        use crate::daemon::queue::WorkerError;
        match e {
            WorkerError::Crash(reason) => DaemonError::SubprocessCrashed(reason),
            WorkerError::Timeout => DaemonError::SubprocessTimeout,
            WorkerError::Backend { message, code } => {
                let formatted = match code {
                    Some(c) => format!("[{c}] {message}"),
                    None => message,
                };
                DaemonError::BackendUnavailable(formatted)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_str_and_exit_code_mapping_matches_spec() {
        assert_eq!(
            DaemonError::BadRequest("x".into()).code_str(),
            "bad_request"
        );
        assert_eq!(DaemonError::BadRequest("x".into()).exit_code(), 1);

        assert_eq!(DaemonError::SubprocessTimeout.exit_code(), 4);
        assert_eq!(DaemonError::SubprocessCrashed("".into()).exit_code(), 4);
        assert_eq!(DaemonError::SubprocessCrashed("x".into()).exit_code(), 4);

        assert_eq!(
            DaemonError::VersionMismatch { supported: vec![1] }.code_str(),
            "version_mismatch"
        );
        assert_eq!(
            DaemonError::BackendUnavailable("x".into()).code_str(),
            "backend_unavailable"
        );
        assert_eq!(DaemonError::BackendUnavailable("x".into()).exit_code(), 1);
    }

    #[test]
    fn worker_agent_error_formats_with_code_prefix() {
        use crate::daemon::queue::WorkerError;
        let e: DaemonError = WorkerError::Backend {
            message: "The model gpt-5 does not exist".into(),
            code: Some("404".into()),
        }
        .into();
        match e {
            DaemonError::BackendUnavailable(msg) => {
                assert_eq!(msg, "[404] The model gpt-5 does not exist");
            }
            other => panic!("expected BackendUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn worker_agent_error_without_code_passes_message_through() {
        use crate::daemon::queue::WorkerError;
        let e: DaemonError = WorkerError::Backend {
            message: "turn/completed malformed".into(),
            code: None,
        }
        .into();
        assert_eq!(
            e,
            DaemonError::BackendUnavailable("turn/completed malformed".into())
        );
    }
}
