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
    SubprocessCrashed,
    AuthFailed,
    AgentUnavailable(String),
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
            DaemonError::SubprocessCrashed => "subprocess_crashed",
            DaemonError::AuthFailed => "auth_failed",
            DaemonError::AgentUnavailable(_) => "agent_unavailable",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            DaemonError::SubprocessTimeout | DaemonError::SubprocessCrashed => 4,
            DaemonError::AuthFailed => 5,
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
            DaemonError::SubprocessTimeout => "agent subprocess timed out".to_string(),
            DaemonError::SubprocessCrashed => "agent subprocess crashed".to_string(),
            DaemonError::AuthFailed => "auth failed (run provider's login command)".to_string(),
            DaemonError::AgentUnavailable(reason) => {
                format!("configured agent provider not available: {reason}")
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
        assert_eq!(DaemonError::SubprocessCrashed.exit_code(), 4);
        assert_eq!(DaemonError::AuthFailed.exit_code(), 5);

        assert_eq!(
            DaemonError::VersionMismatch { supported: vec![1] }.code_str(),
            "version_mismatch"
        );
        assert_eq!(
            DaemonError::AgentUnavailable("x".into()).code_str(),
            "agent_unavailable"
        );
        assert_eq!(DaemonError::AgentUnavailable("x".into()).exit_code(), 1);
    }
}
