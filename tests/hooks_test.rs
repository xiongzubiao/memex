use brainstormer::hooks::sequence_enforcement::*;
use brainstormer::hooks::copilot::*;
use brainstormer::types::*;

// T22: Valid sequence allowed
#[test]
fn t22_valid_tool_sequence() {
    let mut state = PipelineState::new();
    assert!(state.validate_tool_call("brainstorm_swarm").is_ok());
    state.advance(Stage::Brainstorm);
    assert!(state.validate_tool_call("merge").is_ok());
    state.advance(Stage::Merge1);
    assert!(state.validate_tool_call("brainstorm_swarm").is_ok()); // review stage
}

// T23: Invalid sequence cancelled
#[test]
fn t23_invalid_sequence_cancelled() {
    let state = PipelineState::new();
    assert!(state.validate_tool_call("merge").is_err());
}

// T24: Convergence blocked until quality pass
#[test]
fn t24_convergence_blocked_until_quality() {
    let mut state = PipelineState::new();
    state.advance(Stage::Brainstorm);
    state.advance(Stage::Merge1);
    state.advance(Stage::Review);
    state.advance(Stage::Merge2);
    assert!(state.validate_tool_call("convergence").is_err());
    state.advance(Stage::QualityCheck);
    assert!(state.validate_tool_call("convergence").is_ok());
}

// T29: Parse accept/reject/edit
#[test]
fn t29_parse_copilot_commands() {
    assert_eq!(parse_copilot_input("a"), CopilotAction::Accept);
    assert_eq!(parse_copilot_input("accept"), CopilotAction::Accept);
    assert_eq!(
        parse_copilot_input("r needs more error handling"),
        CopilotAction::Reject { feedback: "needs more error handling".to_string() }
    );
    assert_eq!(
        parse_copilot_input("reject too verbose"),
        CopilotAction::Reject { feedback: "too verbose".to_string() }
    );
}

// T30: Structured feedback for next turn
#[test]
fn t30_format_copilot_feedback() {
    let action = CopilotAction::Reject { feedback: "Missing pagination".to_string() };
    let formatted = format_copilot_feedback(&action, "merge", "claude-opus-4-6");
    assert!(formatted.contains("REJECTED"));
    assert!(formatted.contains("Missing pagination"));
    assert!(formatted.contains("merge"));
}
