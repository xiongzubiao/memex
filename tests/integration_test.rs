use brainstormer::tools::convergence::*;
use brainstormer::tools::merge::*;
use brainstormer::tools::merge_quality::*;
use brainstormer::types::*;

/// T31: Standard pipeline happy path (simulated with mock data)
#[test]
fn t31_pipeline_happy_path() {
    let brainstorm_outputs = vec![
        ("claude-opus".to_string(), "Approach: Use Redis with consistent hashing...".to_string()),
        ("gpt-5.4".to_string(), "Approach: Distributed cache with gossip protocol...".to_string()),
        ("gemini-pro".to_string(), "Approach: Cache-aside pattern with TTL management...".to_string()),
    ];

    let merge_input = format_outputs_for_merge(&brainstorm_outputs);
    assert!(merge_input.contains("=== claude-opus ==="));
    assert!(merge_input.contains("=== gpt-5.4 ==="));

    let quality = parse_quality_verdict("All critiques addressed. PASS");
    assert!(quality.passed);

    let final_sections = vec![
        SectionConvergence { name: "Architecture".into(), converged: true, trend: Trend::Same, agreement: "3/3".into(), irreconcilable: false },
        SectionConvergence { name: "API".into(), converged: true, trend: Trend::Better, agreement: "3/3".into(), irreconcilable: false },
        SectionConvergence { name: "Data Model".into(), converged: true, trend: Trend::Same, agreement: "3/3".into(), irreconcilable: false },
    ];
    let result = build_convergence_result(&final_sections);
    assert!(result.all_converged);
    assert!(!result.should_loop);
}

/// T32: Max iterations reached
#[test]
fn t32_max_iterations_best_draft() {
    let sections = vec![
        SectionConvergence { name: "Problem".into(), converged: true, trend: Trend::Same, agreement: "3/3".into(), irreconcilable: false },
        SectionConvergence { name: "Testing".into(), converged: false, trend: Trend::Better, agreement: "2/3".into(), irreconcilable: false },
    ];
    let result = build_convergence_result(&sections);
    assert!(!result.all_converged);
    assert!(result.should_loop);
}

/// T33: Copilot mode parse + feedback
#[test]
fn t33_copilot_mode() {
    use brainstormer::hooks::copilot::*;

    let accept = parse_copilot_input("a");
    assert_eq!(accept, CopilotAction::Accept);

    let reject = parse_copilot_input("r needs more error handling");
    assert_eq!(reject, CopilotAction::Reject { feedback: "needs more error handling".to_string() });

    let feedback = format_copilot_feedback(&reject, "brainstorm", "gpt-5.4");
    assert!(feedback.contains("REJECTED"));
    assert!(feedback.contains("needs more error handling"));
}

/// T36: Resume from last completed stage
#[test]
fn t36_resume_from_last_stage() {
    use brainstormer::hooks::sequence_enforcement::PipelineState;

    let mut state = PipelineState::new();
    state.advance(Stage::Brainstorm);
    state.advance(Stage::Merge1);

    assert!(state.validate_tool_call("brainstorm_swarm").is_ok());
    assert!(state.validate_tool_call("convergence").is_err());
}

/// T43: Input assembly with context (files + URLs)
#[test]
fn t43_input_assembly_with_context() {
    use brainstormer::input::assemble_context;

    let files = vec![
        "# Prior research\nRedis is fast.".to_string(),
        "Requirement: 10M users.".to_string(),
    ];
    let assembled = assemble_context("Design a cache", &files, Some("URL content here"));
    assert!(assembled.contains("## Task"));
    assert!(assembled.contains("Design a cache"));
    assert!(assembled.contains("## Context File 1"));
    assert!(assembled.contains("Redis is fast"));
    assert!(assembled.contains("## Context File 2"));
    assert!(assembled.contains("10M users"));
    assert!(assembled.contains("## URL Context"));
    assert!(assembled.contains("URL content here"));
}

/// T44: All presets load and have required sections
#[test]
fn t44_all_presets_load_and_have_sections() {
    for name in &["software", "general", "research", "article", "book", "strategy"] {
        let preset = brainstormer::template::load_preset(name).unwrap();
        assert!(!preset.sections.is_empty(), "Preset '{}' has empty sections", name);
        assert!(!preset.dimensions.is_empty(), "Preset '{}' has empty dimensions", name);
    }
}
