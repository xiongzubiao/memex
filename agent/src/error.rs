use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("AGENT_E001: Orchestrator model unreachable: {details}")]
    OrchestratorUnreachable { details: String },

    #[error("AGENT_E002: Only {responded}/{total} panelists responded (need at least 2)")]
    InsufficientPanelists { responded: usize, total: usize },

    #[error("AGENT_E003: Session approaching ${budget:.2} limit. {spent:.2} spent.")]
    CostBudgetExceeded { budget: f64, spent: f64 },

    #[error("AGENT_E004: Wiki locked by another process (30s timeout at {lock_path})")]
    LockTimeout { lock_path: PathBuf },

    #[error("AGENT_E005: Wiki page rejected: {details}")]
    StoreValidationFailure { details: String },

    #[error("AGENT_E006: Template not found: {path}. Using embedded fallback.")]
    MissingTemplate { path: String },

    #[error("AGENT_E007: Research sub-agent failed: {reason}")]
    DelegateFailure { reason: String },

    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, AgentError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_includes_code() {
        let err = AgentError::OrchestratorUnreachable {
            details: "timeout".to_string(),
        };
        assert!(format!("{err}").contains("AGENT_E001"));
    }

    #[test]
    fn cost_budget_error_formats_dollars() {
        let err = AgentError::CostBudgetExceeded {
            budget: 5.0,
            spent: 4.87,
        };
        let msg = format!("{err}");
        assert!(msg.contains("$5.00"), "got: {msg}");
        assert!(msg.contains("4.87"), "got: {msg}");
    }
}
