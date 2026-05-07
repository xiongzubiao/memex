//! Daemon error type + exit codes.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    BadRequest(String),
    VersionMismatch { supported: Vec<u32> },
    /// Retrieval returned no results. `collections` carries the user's
    /// filter (empty = no filter); `Display` formats it as either
    /// "no indexed content for MEMEX_ROOT" or "no documents matched in
    /// collection(s): X, Y".
    RetrievalEmpty { collections: Vec<String> },
    Internal(String),
    SubprocessTimeout {
        timeout_sec: u64,
    },
    /// Subprocess crashed. Carries the diagnostic (combined context +
    /// drained stderr) so the user sees why, not just a generic message.
    SubprocessCrashed(String),
    BackendUnavailable(String),
    /// Operation blocked by a dependency (e.g. backlinks referencing the page
    /// being deleted). The message explains what is blocking and how to override.
    Conflict(String),
    /// Storage/database error. Wraps the string representation of the underlying
    /// MemexError so that DaemonError keeps its Clone + PartialEq + Eq derives.
    Storage(String),
}

impl DaemonError {
    pub fn code_str(&self) -> &'static str {
        match self {
            DaemonError::BadRequest(_) => "bad_request",
            DaemonError::VersionMismatch { .. } => "version_mismatch",
            DaemonError::RetrievalEmpty { .. } => "retrieval_empty",
            DaemonError::Internal(_) => "internal",
            DaemonError::SubprocessTimeout { .. } => "subprocess_timeout",
            DaemonError::SubprocessCrashed(_) => "subprocess_crashed",
            DaemonError::BackendUnavailable(_) => "backend_unavailable",
            DaemonError::Conflict(_) => "conflict",
            DaemonError::Storage(_) => "storage_error",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            DaemonError::SubprocessTimeout { .. } | DaemonError::SubprocessCrashed(_) => 4,
            _ => 1,
        }
    }

    /// Inner reason carried by the variant — the data, no English prefix
    /// that duplicates `code_str()`. Variants with no payload supply a
    /// short description that adds context beyond the code (e.g. which
    /// config knob is involved). `Display` drops the trailing colon when
    /// this returns an empty string.
    pub fn message(&self) -> String {
        match self {
            DaemonError::BadRequest(reason) => reason.clone(),
            DaemonError::VersionMismatch { supported } => format!("{supported:?}"),
            DaemonError::RetrievalEmpty { collections } => {
                if collections.is_empty() {
                    "no indexed content for MEMEX_ROOT".to_string()
                } else {
                    format!(
                        "no documents matched in collection(s): {}",
                        collections.join(", ")
                    )
                }
            }
            DaemonError::Internal(reason) => reason.clone(),
            DaemonError::SubprocessTimeout { timeout_sec } => {
                format!("no reply within {timeout_sec}s (daemon.worker.timeout_sec)")
            }
            DaemonError::SubprocessCrashed(reason) => reason.clone(),
            DaemonError::BackendUnavailable(reason) => reason.clone(),
            DaemonError::Conflict(msg) => msg.clone(),
            DaemonError::Storage(msg) => msg.clone(),
        }
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = self.message();
        if reason.is_empty() {
            write!(f, "{}", self.code_str())
        } else {
            write!(f, "{}: {}", self.code_str(), reason)
        }
    }
}

impl std::error::Error for DaemonError {}

impl From<crate::daemon::queue::WorkerError> for DaemonError {
    fn from(e: crate::daemon::queue::WorkerError) -> Self {
        use crate::daemon::queue::WorkerError;
        match e {
            WorkerError::Crash(reason) => DaemonError::SubprocessCrashed(reason),
            WorkerError::Timeout { secs } => DaemonError::SubprocessTimeout { timeout_sec: secs },
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

        assert_eq!(DaemonError::SubprocessTimeout { timeout_sec: 300 }.exit_code(), 4);
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
    fn display_drops_colon_when_message_is_empty() {
        // Variants whose payload is empty render as just the code.
        assert_eq!(
            DaemonError::SubprocessCrashed("".into()).to_string(),
            "subprocess_crashed"
        );
        assert_eq!(
            DaemonError::BackendUnavailable("".into()).to_string(),
            "backend_unavailable"
        );
        // Variants with a populated payload render as `code: payload`.
        assert_eq!(
            DaemonError::SubprocessCrashed("EOF".into()).to_string(),
            "subprocess_crashed: EOF"
        );
        assert_eq!(
            DaemonError::BackendUnavailable("[404] missing".into()).to_string(),
            "backend_unavailable: [404] missing"
        );
        // Field-less variants supply a short description that adds info
        // beyond the code (which config knob, what's missing, etc.).
        assert_eq!(
            DaemonError::SubprocessTimeout { timeout_sec: 300 }.to_string(),
            "subprocess_timeout: no reply within 300s (daemon.worker.timeout_sec)"
        );
        assert_eq!(
            DaemonError::RetrievalEmpty { collections: vec![] }.to_string(),
            "retrieval_empty: no indexed content for MEMEX_ROOT"
        );
        assert_eq!(
            DaemonError::RetrievalEmpty {
                collections: vec!["notes".into()]
            }
            .to_string(),
            "retrieval_empty: no documents matched in collection(s): notes"
        );
        assert_eq!(
            DaemonError::RetrievalEmpty {
                collections: vec!["a".into(), "b".into()]
            }
            .to_string(),
            "retrieval_empty: no documents matched in collection(s): a, b"
        );
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
