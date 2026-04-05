use brainstormer::input::*;

#[test]
fn read_markdown_file() {
    let dir = std::env::temp_dir().join("brainstormer_input_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("context.md");
    std::fs::write(&path, "# Prior Work\n\nWe explored Redis caching.").unwrap();

    let result = load_file_context(path.to_str().unwrap()).unwrap();
    assert!(result.contains("Prior Work"));
    assert!(result.contains("Redis caching"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn read_text_file() {
    let dir = std::env::temp_dir().join("brainstormer_input_test2");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notes.txt");
    std::fs::write(&path, "Key requirement: sub-100ms latency").unwrap();

    let result = load_file_context(path.to_str().unwrap()).unwrap();
    assert!(result.contains("sub-100ms"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn nonexistent_file_returns_error() {
    let result = load_file_context("/nonexistent/path.md");
    assert!(result.is_err());
}

#[test]
fn assemble_context_with_task_and_files() {
    let task = "Design a cache";
    let file_contents = vec![
        "# Requirements\nMust handle 10M users.".to_string(),
        "Notes: Use Redis cluster.".to_string(),
    ];
    let result = assemble_context(task, &file_contents, None);
    assert!(result.contains("Design a cache"));
    assert!(result.contains("10M users"));
    assert!(result.contains("Redis cluster"));
}

#[test]
fn assemble_context_without_files() {
    let result = assemble_context("Design a cache", &[], None);
    assert_eq!(result, "Design a cache");
}

#[test]
fn strip_html_removes_tags() {
    let html = "<html><body><h1>Hello</h1> <p>World</p></body></html>";
    let result = strip_html_tags(html);
    assert_eq!(result, "Hello World");
}

#[test]
fn strip_html_handles_plain_text() {
    let text = "Just plain text with no tags";
    let result = strip_html_tags(text);
    assert_eq!(result, "Just plain text with no tags");
}

#[test]
fn strip_html_collapses_whitespace() {
    let html = "<p>Lots   of   \n\n  spaces</p>";
    let result = strip_html_tags(html);
    assert_eq!(result, "Lots of spaces");
}
