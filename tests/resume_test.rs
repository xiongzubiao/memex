use brainstormer::resume::*;
use brainstormer::types::*;

#[test]
fn parse_session_state_from_entries() {
    let config_json = serde_json::to_string(&SessionConfig {
        task_type: "software".into(),
        mode: Mode::Autopilot,
        do_loop: true,
        brainstorm_models: vec![ModelRef { provider: "openai".into(), model: "gpt-5.4".into() }],
        review_models: vec![ModelRef { provider: "openai".into(), model: "gpt-5.4-mini".into() }],
        max_rounds: 5,
        merge_llm: ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
    })
    .unwrap();

    let state = SessionState::from_entries(
        "test-session-id",
        Some(&config_json),
        None,
        Some("# Draft content from round 1"),
        1,
    );

    assert_eq!(state.session_id, "test-session-id");
    assert_eq!(state.last_round, 1);
    assert!(state.config.is_some());
    assert!(state.last_draft.is_some());
    assert_eq!(state.last_draft.unwrap(), "# Draft content from round 1");
}

#[test]
fn session_state_without_config_is_not_resumable() {
    let state = SessionState::from_entries("test", None, None, Some("draft"), 1);
    assert!(state.config.is_none());
    assert!(!state.is_resumable());
}

#[test]
fn session_state_without_draft_is_not_resumable() {
    let config_json = serde_json::to_string(&SessionConfig {
        task_type: "software".into(),
        mode: Mode::Autopilot,
        do_loop: true,
        brainstorm_models: vec![],
        review_models: vec![],
        max_rounds: 5,
        merge_llm: ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
    })
    .unwrap();
    let state = SessionState::from_entries("test", Some(&config_json), None, None, 0);
    assert!(!state.is_resumable());
}
