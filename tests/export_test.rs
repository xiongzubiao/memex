use brainstormer::export::*;
use brainstormer::types::*;

#[test]
fn export_markdown_has_metadata_header() {
    let config = SessionConfig {
        task_type: "software".into(),
        mode: Mode::Autopilot,
        do_loop: true,
        brainstorm_models: vec![
            ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
        ],
        review_models: vec![
            ModelRef { provider: "openai".into(), model: "gpt-5.4-mini".into() },
        ],
        max_rounds: 5,
        merge_llm: ModelRef { provider: "openai".into(), model: "gpt-5.4".into() },
    };
    let result = format_export(
        "Design a cache",
        &config,
        "# Cache Design\n\nContent here.",
        3,
        true,
        1.24,
    );
    assert!(result.contains("---"));
    assert!(result.contains("task: Design a cache"));
    assert!(result.contains("type: software"));
    assert!(result.contains("rounds: 3"));
    assert!(result.contains("converged: true"));
    assert!(result.contains("# Cache Design"));
}

#[test]
fn export_markdown_with_output_path() {
    let dir = std::env::temp_dir().join("brainstormer_export_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test_output.md");

    let content = "# Test Document\n\nThis is a test.";
    write_export(&path, content).unwrap();

    let read_back = std::fs::read_to_string(&path).unwrap();
    assert_eq!(read_back, content);

    std::fs::remove_dir_all(&dir).unwrap();
}
