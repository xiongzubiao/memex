use crate::types::CopilotAction;

/// Parse user input into a CopilotAction.
pub fn parse_copilot_input(input: &str) -> CopilotAction {
    let trimmed = input.trim();
    if trimmed == "a" || trimmed.starts_with("accept") {
        CopilotAction::Accept
    } else if trimmed.starts_with("r ") || trimmed.starts_with("reject ") {
        let feedback = trimmed
            .trim_start_matches("reject ")
            .trim_start_matches("r ")
            .to_string();
        CopilotAction::Reject { feedback }
    } else if trimmed.starts_with("e ") || trimmed.starts_with("edit ") {
        let content = trimmed
            .trim_start_matches("edit ")
            .trim_start_matches("e ")
            .to_string();
        CopilotAction::Edit { content }
    } else {
        CopilotAction::Reject {
            feedback: trimmed.to_string(),
        }
    }
}

/// Format a CopilotAction into structured feedback for the next Agent turn.
pub fn format_copilot_feedback(action: &CopilotAction, stage: &str, model_id: &str) -> String {
    match action {
        CopilotAction::Accept => {
            format!("[ACCEPTED] User accepted {} output from {}", stage, model_id)
        }
        CopilotAction::Reject { feedback } => {
            format!(
                "[REJECTED] User rejected {} output from {}.\nFeedback: {}",
                stage, model_id, feedback
            )
        }
        CopilotAction::Edit { content } => {
            format!(
                "[EDITED] User edited {} output from {}.\nNew content: {}",
                stage, model_id, content
            )
        }
    }
}
